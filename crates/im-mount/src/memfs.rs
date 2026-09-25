//! 内存文件系统（阶段 13）：FUSE/WinFsp 内核回调的**纯逻辑部分**。
//!
//! # 挂载盘的分层视角
//!
//! 「把 IM 数据挂成一个盘」拆开是三层，本模块是中间那层：
//!
//! ```text
//!  ┌────────────────────────────────────────────────────┐
//!  │ 内核 VFS            （操作系统，不归我们写）        │
//!  │        ↓ 回调：lookup / readdir / read / getattr    │
//!  ├────────────────────────────────────────────────────┤
//!  │ 【本模块】MemFs     语义层：路径、inode、目录项     │  ← 可单测
//!  │        ↑ 数据源：IM 的联系人/会话/消息              │
//!  ├────────────────────────────────────────────────────┤
//!  │ 驱动接线 FUSE(libfuse) / WinFsp     （阶段 13 诚实  │
//!  │   边界：需要平台驱动，见 crate 文档与 docs/19）     │
//!  └────────────────────────────────────────────────────┘
//! ```
//!
//! 驱动层（真挂载）依赖内核态组件：Linux 的 FUSE 设备、Windows 的
//! `WinFsp` 驱动。本 crate 不带 unsafe、不碰内核——先把**中间层的
//! 语义**用可测试的方式钉死（路径解析、目录项、错误分类），驱动
//! 接线是纯粹的胶水（把回调参数翻译成本模块的方法调用）。
//!
//! # 与真实 FUSE 的语义对齐
//!
//! | 本模块方法 | FUSE 回调 | `WinFsp` 侧 | 说明 |
//! |-----------|----------|-----------|------|
//! | [`MemFs::lookup`] | `lookup` | `GetFileInfoByPath` | 路径 → 属性 |
//! | [`MemFs::read_dir`] | `readdir` | `FindFiles` | 目录项列表 |
//! | [`MemFs::read`] | `read` | `ReadFile` | 读文件内容 |
//! | [`MemFs::mkdir`] / [`write_file`] | `mkdir`/`mknod`+`write` | `Create`/`SetFileSize` | 构建/更新视图 |
//!
//! inode 号在本模块里是 `BTreeMap` 的 key（自增分配）；真实文件系统
//! 里它是磁盘寻址的句柄，语义角色相同：**路径是给人看的，inode 是
//! 给系统用的**。

use std::collections::BTreeMap;

use crate::error::FsError;

/// inode 号：文件系统内每个节点（文件/目录）的身份证。
pub type Ino = u64;

/// 节点类别（stat 的 `st_mode` 提炼到只剩挂载盘需要的两类）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// 普通文件（IM 视图里：名片、消息日志、共享文件）
    File,
    /// 目录（IM 视图里：联系人的分组、历史消息的容器）
    Directory,
}

/// 目录项（`readdir` 的产出单元）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// 文件名（不含路径前缀——目录项本来就是「相对父亲的」）
    pub name: String,
    /// 子节点的 inode
    pub ino: Ino,
    /// 类别（真实 readdir 也带类型，`ls -F` 靠它画斜杠）
    pub kind: NodeKind,
}

/// 节点属性（`getattr` 的产出，真实 stat 里时间戳/权限的极简版）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attr {
    pub kind: NodeKind,
    /// 文件字节数；目录为 0（语义层的目录没有「大小」这个概念）
    pub size: u64,
    /// 目录的直接子节点数（`ls | wc -l` 的语义层等价物）
    pub children: usize,
}

/// FS 节点：目录（孩子表）或文件（数据），一个结构两态。
#[derive(Debug)]
struct Node {
    kind: NodeKind,
    /// 目录：文件名 → 子 inode。用 `BTreeMap` 不是为了性能——
    /// 是为了 **readdir 的顺序稳定**（真实文件系统也保证同一次
    /// 打开期间列举顺序可预期，BTreeMap 顺便给了字典序）。
    children: BTreeMap<String, Ino>,
    /// 文件数据；目录恒为空 Vec（两态共存在一个结构里，靠 kind 分派）
    data: Vec<u8>,
}

/// 内存文件系统：inode 表 + 路径解析 + FUSE 语义的操作集。
#[derive(Debug)]
pub struct MemFs {
    /// inode → 节点（真实 FS 的 inode 表内存版）
    nodes: BTreeMap<Ino, Node>,
    /// 下一个分配的 inode（根固定是 1，真实 FS 也常这么约定）
    next_ino: Ino,
    /// 根 inode（路径解析的锚点）
    root: Ino,
}

impl MemFs {
    /// 构造空文件系统（只有一个根目录）。
    #[must_use]
    pub fn new() -> Self {
        let root = 1;
        let mut nodes = BTreeMap::new();
        nodes.insert(
            root,
            Node { kind: NodeKind::Directory, children: BTreeMap::new(), data: Vec::new() },
        );
        Self { nodes, next_ino: root + 1, root }
    }

    /// 解析路径到 inode（不返回属性——[`MemFs::lookup`] 是它的门面）。
    fn resolve(&self, path: &str) -> Result<Ino, FsError> {
        let mut cur = self.root;
        for component in components(path) {
            let node = self.node(cur)?;
            if node.kind != NodeKind::Directory {
                return Err(FsError::NotADirectory { path: path.into() });
            }
            cur = *node
                .children
                .get(component)
                .ok_or_else(|| FsError::NotFound { path: path.into() })?;
        }
        Ok(cur)
    }

    /// lookup（FUSE 回调同名）：路径 → 属性。
    ///
    /// # Errors
    ///
    /// 路径不存在（[`FsError::NotFound`]）或中间组件不是目录
    /// （[`FsError::NotADirectory`]）。
    pub fn lookup(&self, path: &str) -> Result<Attr, FsError> {
        let ino = self.resolve(path)?;
        self.attr(ino)
    }

    /// readdir（FUSE 回调同名）：路径 → 目录项列表（字典序）。
    ///
    /// # Errors
    ///
    /// 路径不存在，或路径不是目录（[`FsError::NotADirectory`]，
    /// POSIX 的 `ENOTDIR` 同名对齐）。
    ///
    /// # Panics
    ///
    /// 目录项指向的 inode 失踪时 panic——那是内部一致性 bug
    ///（目录项与 inode 表同源维护，正常代码路径不可达），不拿
    /// IO 错误掩盖，见 [`FsError::InodeGone`] 的说明。
    pub fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        let ino = self.resolve(path)?;
        let node = self.node(ino)?;
        if node.kind != NodeKind::Directory {
            return Err(FsError::NotADirectory { path: path.into() });
        }
        Ok(node
            .children
            .iter()
            .map(|(name, &child_ino)| DirEntry {
                name: name.clone(),
                ino: child_ino,
                kind: self.node(child_ino).expect("目录项指向的 inode 必须存在").kind,
            })
            .collect())
    }

    /// read（FUSE 回调同名）：读整个文件（挂载盘视图的文件都很小，
    /// 语义层不做 offset/分页——那是驱动层把「大文件读」切片的职责）。
    ///
    /// # Errors
    ///
    /// 路径不存在，或路径指向目录（[`FsError::IsADirectory`]——
    /// POSIX 的 `EISDIR` 同名对齐）。
    pub fn read(&self, path: &str) -> Result<&[u8], FsError> {
        let ino = self.resolve(path)?;
        let node = self.node(ino)?;
        // 目录没有内容可读：EISDIR（正向判断——「是目录才拒」比
        // 「不是目录才收」把错误分支放在面前）
        if node.kind == NodeKind::Directory {
            Err(FsError::IsADirectory { path: path.into() })
        } else {
            Ok(&node.data)
        }
    }

    /// mkdir（FUSE 回调同名）：创建目录（父目录必须已存在——
    /// 不做 `mkdir -p`，语义层的规则要显式，方便是上层的事）。
    ///
    /// # Errors
    ///
    /// 父目录不存在/父路径是文件/已存在同名子项。
    pub fn mkdir(&mut self, path: &str) -> Result<Ino, FsError> {
        let (parent, name) = self.split_parent(path)?;
        let parent_node = self.node_mut(parent)?;
        if parent_node.kind != NodeKind::Directory {
            return Err(FsError::NotADirectory { path: path.into() });
        }
        if parent_node.children.contains_key(name) {
            return Err(FsError::AlreadyExists { path: path.into() });
        }
        let ino = self.alloc(Node {
            kind: NodeKind::Directory,
            children: BTreeMap::new(),
            data: Vec::new(),
        });
        self.node_mut(parent)?.children.insert(name.to_string(), ino);
        Ok(ino)
    }

    /// 创建/覆写文件（`mknod` + `write` 的合并便捷形态——
    /// 构建 IM 视图的代码更关心「把这段字节放到这个路径」）。
    ///
    /// # Errors
    ///
    /// 父目录不存在/父路径是文件（与 [`MemFs::mkdir`] 同一套错误）。
    pub fn write_file(&mut self, path: &str, data: impl Into<Vec<u8>>) -> Result<Ino, FsError> {
        let (parent, name) = self.split_parent(path)?;
        let parent_node = self.node_mut(parent)?;
        if parent_node.kind != NodeKind::Directory {
            return Err(FsError::NotADirectory { path: path.into() });
        }
        // 已存在：覆写数据（类型必须还是文件——目录名被文件顶掉是数据损坏）
        if let Some(&child) = parent_node.children.get(name) {
            let child_node = self.node_mut(child)?;
            if child_node.kind == NodeKind::Directory {
                return Err(FsError::AlreadyExists { path: path.into() });
            }
            child_node.data = data.into();
            return Ok(child);
        }
        let ino =
            self.alloc(Node { kind: NodeKind::File, children: BTreeMap::new(), data: data.into() });
        self.node_mut(parent)?.children.insert(name.to_string(), ino);
        Ok(ino)
    }

    /// 删除文件（`unlink`）。目录不删——挂载盘视图是 IM 数据的
    /// 投影，删除走 IM 的协议，不该从文件系统侧绕过。
    ///
    /// # Errors
    ///
    /// 路径不存在，或路径指向目录（[`FsError::IsADirectory`]）。
    pub fn remove_file(&mut self, path: &str) -> Result<(), FsError> {
        let (parent, name) = self.split_parent(path)?;
        let child = {
            let parent_node = self.node_mut(parent)?;
            if parent_node.kind != NodeKind::Directory {
                return Err(FsError::NotADirectory { path: path.into() });
            }
            *parent_node
                .children
                .get(name)
                .ok_or_else(|| FsError::NotFound { path: path.into() })?
        };
        if self.node(child)?.kind == NodeKind::Directory {
            return Err(FsError::IsADirectory { path: path.into() });
        }
        self.node_mut(parent)?.children.remove(name);
        self.nodes.remove(&child);
        Ok(())
    }

    // ── 内部：分配/访问 ──

    /// 分配新 inode（自增——真实 FS 的分配器要管复用/持久化，
    /// 内存版语义相同：**新节点的号必须唯一且不再变化**）。
    fn alloc(&mut self, node: Node) -> Ino {
        let ino = self.next_ino;
        self.next_ino += 1;
        self.nodes.insert(ino, node);
        ino
    }

    fn node(&self, ino: Ino) -> Result<&Node, FsError> {
        self.nodes.get(&ino).ok_or(FsError::InodeGone { ino })
    }

    fn node_mut(&mut self, ino: Ino) -> Result<&mut Node, FsError> {
        self.nodes.get_mut(&ino).ok_or(FsError::InodeGone { ino })
    }

    fn attr(&self, ino: Ino) -> Result<Attr, FsError> {
        let node = self.node(ino)?;
        Ok(Attr {
            kind: node.kind,
            size: u64::try_from(node.data.len()).unwrap_or(u64::MAX),
            children: node.children.len(),
        })
    }

    /// 把路径拆成（父 inode，最后一段文件名）。根路径没有父——
    /// `mkdir("/")` 之类的操作在这里就报错。
    ///
    /// 返回的 `&str` 生命周期绑 **path**（不绑 `&self`）：它只是
    /// 入参的切片，如果不显式标注，省略规则会把它绑到 `&self` 上，
    /// 后续任何 `&mut self` 的操作（alloc/insert）都要与这个不可变
    /// 借用打架——生命周期标注不是美学问题，是借用检查器的合同。
    fn split_parent<'a>(&self, path: &'a str) -> Result<(Ino, &'a str), FsError> {
        let components = components(path).collect::<Vec<_>>();
        let (name, ancestors) =
            components.split_last().ok_or_else(|| FsError::InvalidPath { path: path.into() })?;
        // 逐级走到父目录
        let mut parent = self.root;
        for component in ancestors {
            let node = self.node(parent)?;
            if node.kind != NodeKind::Directory {
                return Err(FsError::NotADirectory { path: path.into() });
            }
            parent = *node
                .children
                .get(*component)
                .ok_or_else(|| FsError::NotFound { path: path.into() })?;
        }
        Ok((parent, name))
    }
}

impl Default for MemFs {
    fn default() -> Self {
        Self::new()
    }
}

/// 路径切片：`"/a//b/"` → `["a", "b"]`（POSIX 语义：空段忽略，
/// 结尾斜杠不区分——挂载盘场景够用；符号链接之类的花活不进语义层）。
fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|c| !c.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 脚手架：搭一个小树
    /// ```text
    /// /
    /// ├── contacts/
    /// │   ├── alice.txt
    /// │   └── bob.txt
    /// └── history/
    ///     └── alice/
    ///         └── 2026-09-24.log
    /// ```
    fn sample_fs() -> MemFs {
        let mut fs = MemFs::new();
        fs.mkdir("/contacts").unwrap();
        fs.mkdir("/history").unwrap();
        fs.mkdir("/history/alice").unwrap();
        fs.write_file("/contacts/alice.txt", b"alice@example.com").unwrap();
        fs.write_file("/contacts/bob.txt", b"bob@example.com").unwrap();
        fs.write_file("/history/alice/2026-09-24.log", b"alice: hello").unwrap();
        fs
    }

    #[test]
    fn root_is_an_empty_directory() {
        let fs = MemFs::new();
        assert_eq!(fs.lookup("/"), Ok(Attr { kind: NodeKind::Directory, size: 0, children: 0 }));
        assert_eq!(fs.read_dir("/"), Ok(Vec::new()));
    }

    #[test]
    fn lookup_walks_nested_paths() {
        let fs = sample_fs();
        let attr = fs.lookup("/history/alice/2026-09-24.log").unwrap();
        assert_eq!(attr.kind, NodeKind::File);
        assert_eq!(attr.size, u64::try_from("alice: hello".len()).unwrap());

        assert_eq!(
            fs.lookup("/contacts/alice.txt/more"),
            Err(FsError::NotADirectory { path: "/contacts/alice.txt/more".into() }),
            "文件下面还有路径段：ENOTDIR"
        );
        assert_eq!(fs.lookup("/nope"), Err(FsError::NotFound { path: "/nope".into() }));
    }

    #[test]
    fn read_dir_lists_in_lexicographic_order() {
        let fs = sample_fs();
        let entries = fs.read_dir("/contacts").unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alice.txt", "bob.txt"], "BTreeMap 保证字典序");
        assert_eq!(entries[0].kind, NodeKind::File);

        assert_eq!(
            fs.read_dir("/contacts/alice.txt"),
            Err(FsError::NotADirectory { path: "/contacts/alice.txt".into() }),
            "对文件 readdir：ENOTDIR"
        );
    }

    #[test]
    fn read_file_and_isdir_error() {
        let fs = sample_fs();
        assert_eq!(fs.read("/contacts/alice.txt"), Ok(&b"alice@example.com"[..]));
        assert_eq!(
            fs.read("/contacts"),
            Err(FsError::IsADirectory { path: "/contacts".into() }),
            "对目录 read：EISDIR"
        );
    }

    #[test]
    fn mkdir_rejects_duplicates_and_missing_parents() {
        let mut fs = sample_fs();
        assert_eq!(fs.mkdir("/contacts"), Err(FsError::AlreadyExists { path: "/contacts".into() }));
        assert_eq!(
            fs.mkdir("/deep/nested"),
            Err(FsError::NotFound { path: "/deep/nested".into() }),
            "不做 mkdir -p：父目录不存在要明说"
        );
        // 成功路径：新目录立刻可见
        fs.mkdir("/contacts/group").unwrap();
        assert_eq!(fs.lookup("/contacts/group").unwrap().kind, NodeKind::Directory);
    }

    #[test]
    fn write_file_creates_then_overwrites() {
        let mut fs = sample_fs();
        let ino_first = fs.write_file("/contacts/alice.txt", b"updated").unwrap();
        let ino_second = fs.write_file("/contacts/alice.txt", b"updated again").unwrap();
        assert_eq!(ino_first, ino_second, "覆写不该换 inode（真实 FS 覆写也不换）");
        assert_eq!(fs.read("/contacts/alice.txt"), Ok(&b"updated again"[..]));
        // 文件名顶目录：拒绝
        assert!(fs.write_file("/contacts", b"data").is_err());
    }

    #[test]
    fn remove_file_frees_and_rejects_directories() {
        let mut fs = sample_fs();
        fs.remove_file("/contacts/bob.txt").unwrap();
        assert!(fs.lookup("/contacts/bob.txt").is_err());
        assert_eq!(fs.lookup("/contacts").unwrap().children, 1);
        // 目录不可从 FS 侧删（IM 视图是投影，删除走 IM 协议）
        assert_eq!(
            fs.remove_file("/contacts"),
            Err(FsError::IsADirectory { path: "/contacts".into() })
        );
    }

    #[test]
    fn path_parsing_is_posix_lenient() {
        let mut fs = MemFs::new();
        fs.mkdir("/a").unwrap();
        fs.mkdir("//a//b/").unwrap(); // 多余斜杠、结尾斜杠
        assert!(fs.lookup("/a/b").is_ok());
        assert!(fs.lookup("a/b").is_ok(), "开头的斜杠可有可无（语义层从根出发）");
    }
}
