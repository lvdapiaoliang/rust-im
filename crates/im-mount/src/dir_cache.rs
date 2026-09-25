//! 目录项缓存（阶段 13）：[`LruCache`] 的落地场景 + 缓存一致性纪律。
//!
//! # 为什么目录列表值得缓存
//!
//! 真实文件系统里 `readdir` 是元数据操作，要遍历磁盘上的目录块；
//! 挂载盘的目录列表来自 IM 数据（联系人列表、会话历史），每次
//! 都现算一遍等于把映射逻辑跑一遍。目录项列表**读多写少**（ explorer
//! 反复列举同一个目录，联系人列表一天变不了几次）——教科书级的
//! 缓存收益模型，配上容量上限就是 LRU。
//!
//! # 缓存一致性的纪律：先改数据，再失效缓存
//!
//! 缓存最难的不是命中/淘汰，是**别给脏读留窗口**。本模块的
//! [`CachedFs`] 把「写操作 → 失效受影响目录」封装在一个屋檐下：
//!
//! ```text
//!   write_file("/contacts/alice.txt")  →  失效 "/contacts"
//!   mkdir("/history/alice")            →  失效 "/history"
//!   remove_file("/contacts/bob.txt")   →  失效 "/contacts"
//! ```
//!
//! 顺序必须是**先改 `MemFs`、后失效缓存**（本模块单线程演示；
//! 多线程下失效与失效前夜的读之间还有竞态，那时才轮得到 epoch/
//! 版本号方案——记入 docs/19 的已知边界，不装作解决了）。

use std::sync::Arc;

use crate::error::FsError;
use crate::lru::LruCache;
use crate::memfs::{DirEntry, MemFs};

/// 目录项缓存：路径 → 目录项列表（`Arc` 包着，命中时克隆引用
/// 计数即可，不复制列表内容——挂载盘的目录项列表是只读快照，
/// 多处共享同一份是安全的）。
pub struct DirCache {
    cache: LruCache<String, Arc<[DirEntry]>>,
    hits: u64,
    misses: u64,
}

impl DirCache {
    /// 构造指定容量（缓存目录数上限）的目录缓存。
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self { cache: LruCache::new(capacity), hits: 0, misses: 0 }
    }

    /// 命中次数（观测口径：真实文件系统也暴露这些计数给 perf 工具）。
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// 未命中次数。
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// 命中率（0.0~1.0；冷缓存 0/0 记作 0.0，不 NaN）。
    #[must_use]
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            f64::from(self.hits) / f64::from(total)
        }
    }

    /// 读目录：先查缓存，miss 走 [`MemFs`] 并回填。
    ///
    /// # Errors
    ///
    /// 同 [`MemFs::read_dir`]（miss 时的真实读取会穿透错误，
    /// 不把错误缓存起来——**缓存的是结果，不是错误**）。
    pub fn read_dir(&mut self, fs: &MemFs, path: &str) -> Result<Arc<[DirEntry]>, FsError> {
        if let Some(entries) = self.cache.get(&path.to_string()) {
            self.hits += 1;
            return Ok(Arc::clone(entries));
        }
        self.misses += 1;
        let entries: Arc<[DirEntry]> = fs.read_dir(path)?.into();
        self.cache.put(path.to_string(), Arc::clone(&entries));
        Ok(entries)
    }

    /// 失效一个目录（写操作后调用——见模块文档的顺序纪律）。
    pub fn invalidate(&mut self, path: &str) {
        self.cache.remove(&path.to_string());
    }

    /// 全量失效（批量导入 IM 数据之后的最粗暴也最安全的做法）。
    pub fn invalidate_all(&mut self) {
        self.cache.clear();
    }
}

/// `MemFs` + [`DirCache`] 的组合体：对外一套 FS 操作，对内保证
/// 「写 → 失效」的缓存一致性纪律不出本类型（调用方没有机会只改
/// 数据不失效缓存——**纪律用类型封装，不靠自觉**）。
pub struct CachedFs {
    fs: MemFs,
    dir_cache: DirCache,
}

impl CachedFs {
    /// 构造：语义层 + 指定容量的目录缓存。
    #[must_use]
    pub fn new(fs: MemFs, dir_cache_capacity: usize) -> Self {
        Self { fs, dir_cache: DirCache::new(dir_cache_capacity) }
    }

    /// 目录缓存观测（原样暴露，压测/调试用）。
    #[must_use]
    pub fn cache_stats(&self) -> (u64, u64) {
        (self.dir_cache.hits(), self.dir_cache.misses())
    }

    /// 读目录（走缓存）。见 [`DirCache::read_dir`]。
    ///
    /// # Errors
    ///
    /// 同 [`MemFs::read_dir`]。
    pub fn read_dir(&mut self, path: &str) -> Result<Arc<[DirEntry]>, FsError> {
        self.dir_cache.read_dir(&self.fs, path)
    }

    /// 读文件（不缓存：文件内容直读——IM 视图的文件都很小，
    /// 缓存文件数据是把简单问题复杂化，见 docs/19 的取舍）。
    ///
    /// # Errors
    ///
    /// 同 [`MemFs::read`]。
    pub fn read(&self, path: &str) -> Result<&[u8], FsError> {
        self.fs.read(path)
    }

    /// 属性查询（不缓存：O(路径深度) 的纯内存遍历，缓存它收益
    /// 覆盖不了失效成本——**不是所有东西都该缓存**，这个决定
    /// 本身就是教学点）。
    ///
    /// # Errors
    ///
    /// 同 [`MemFs::lookup`]。
    pub fn lookup(&self, path: &str) -> Result<crate::memfs::Attr, FsError> {
        self.fs.lookup(path)
    }

    /// mkdir + 失效父目录。
    ///
    /// # Errors
    ///
    /// 同 [`MemFs::mkdir`]（错误发生时缓存不动——没改成就不失效）。
    pub fn mkdir(&mut self, path: &str) -> Result<u64, FsError> {
        let ino = self.fs.mkdir(path)?;
        self.invalidate_parent_of(path);
        Ok(ino)
    }

    /// 写文件 + 失效父目录。
    ///
    /// # Errors
    ///
    /// 同 [`MemFs::write_file`]。
    pub fn write_file(&mut self, path: &str, data: impl Into<Vec<u8>>) -> Result<u64, FsError> {
        let ino = self.fs.write_file(path, data)?;
        self.invalidate_parent_of(path);
        Ok(ino)
    }

    /// 删文件 + 失效父目录。
    ///
    /// # Errors
    ///
    /// 同 [`MemFs::remove_file`]。
    pub fn remove_file(&mut self, path: &str) -> Result<(), FsError> {
        self.fs.remove_file(path)?;
        self.invalidate_parent_of(path);
        Ok(())
    }

    /// 失效 `path` 的父目录（改动影响的是父目录的**列表**）。
    fn invalidate_parent_of(&mut self, path: &str) {
        // "/a/b/c" → "/a/b"；根下的直接子项影响的是根目录 "/"
        let parent = match path.rsplit_once('/') {
            Some((dir, _)) if dir.is_empty() => "/".to_string(),
            Some((dir, _)) => dir.to_string(),
            None => "/".to_string(),
        };
        self.dir_cache.invalidate(&parent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memfs::NodeKind;

    /// 脚手架：三个目录的 FS + 容量 2 的目录缓存。
    fn cached_fs() -> CachedFs {
        let mut fs = MemFs::new();
        fs.mkdir("/contacts").unwrap();
        fs.mkdir("/history").unwrap();
        fs.mkdir("/files").unwrap();
        fs.write_file("/contacts/alice.txt", b"alice").unwrap();
        fs.write_file("/history/day1.log", b"day1").unwrap();
        fs.write_file("/files/a.bin", b"\x01").unwrap();
        CachedFs::new(fs, 2)
    }

    #[test]
    fn second_read_hits_the_cache() {
        let mut fs = cached_fs();
        fs.read_dir("/contacts").unwrap();
        fs.read_dir("/contacts").unwrap();
        let (hits, misses) = fs.cache_stats();
        assert_eq!((hits, misses), (1, 1), "第二次列举必须命中");
    }

    #[test]
    fn lru_evicts_the_coldest_directory() {
        let mut fs = cached_fs(); // 缓存容量 2
        fs.read_dir("/contacts").unwrap(); // 冷→热
        fs.read_dir("/history").unwrap(); // 满：踢 contacts？不——
        fs.read_dir("/contacts").unwrap(); // contacts 回热，history 最冷
        fs.read_dir("/files").unwrap(); // 容量 2：踢的是 history
        fs.read_dir("/contacts").unwrap(); // 仍命中（没被踢）
        let (hits, misses) = fs.cache_stats();
        assert_eq!(misses, 3, "history 被踢后再列举才 miss");
        assert!(hits >= 2, "contacts 两次热读都命中");
    }

    #[test]
    fn write_invalidates_the_parent_listing() {
        let mut fs = cached_fs();
        fs.read_dir("/contacts").unwrap(); // 预热
        fs.write_file("/contacts/bob.txt", b"bob").unwrap();
        let (before_hits, before_misses) = fs.cache_stats();
        fs.read_dir("/contacts").unwrap(); // 必须看到新文件 → 说明 miss 了
        let (hits, misses) = fs.cache_stats();
        assert_eq!(misses, before_misses + 1, "写之后父目录的缓存必须已失效");
        assert_eq!(hits, before_hits);
        // 新内容真的在（不是缓存给了脏数据）
        let entries = fs.read_dir("/contacts").unwrap();
        assert!(entries.iter().any(|e| e.name == "bob.txt"));
    }

    #[test]
    fn failed_write_keeps_cache_intact() {
        let mut fs = cached_fs();
        fs.read_dir("/history").unwrap(); // 预热
        // 写一个父目录不存在的路径：失败，不该动缓存
        let result = fs.write_file("/nope/x.txt", b"x");
        assert!(result.is_err());
        fs.read_dir("/history").unwrap();
        let (hits, _misses) = fs.cache_stats();
        assert_eq!(hits, 1, "失败的写不该把缓存打穿");
    }

    #[test]
    fn remove_and_mkdir_also_invalidate() {
        let mut fs = cached_fs();
        fs.read_dir("/files").unwrap();
        fs.remove_file("/files/a.bin").unwrap();
        fs.read_dir("/files").unwrap(); // miss（失效了）
        assert_eq!(fs.lookup("/files").unwrap().children, 0);

        fs.read_dir("/history").unwrap(); // 预热
        fs.mkdir("/history/alice").unwrap();
        fs.read_dir("/history").unwrap(); // miss
        let entries = fs.read_dir("/history").unwrap();
        assert!(entries.iter().any(|e| e.kind == NodeKind::Directory && e.name == "alice"));
    }

    #[test]
    fn errors_are_not_cached() {
        let mut fs = cached_fs();
        assert!(fs.read_dir("/ghost").is_err());
        let (hits, misses) = fs.cache_stats();
        assert_eq!((hits, misses), (0, 1), "错误也计一次 miss");
        // 目录后来出现了：必须能读到（错误没被缓存）
        fs.mkdir("/ghost").unwrap();
        let entries = fs.read_dir("/ghost").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn hit_rate_handles_cold_cache() {
        let mut cache = DirCache::new(4);
        assert_eq!(cache.hit_rate(), 0.0, "0/0 不 NaN");
        let fs = MemFs::new();
        cache.read_dir(&fs, "/").unwrap();
        cache.read_dir(&fs, "/").unwrap();
        let rate = cache.hit_rate();
        assert!((rate - 0.5).abs() < f64::EPSILON, "1 命中 1 未命中 → 50%");
    }
}
