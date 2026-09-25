//! # im-mount：挂载盘语义层（阶段 13）
//!
//! 「把 IM 数据挂成一个盘」（Telegram Desktop 就带这个功能：把
//! 聊天记录挂成只读盘，用任意工具消费）。本 crate 交付**语义层**：
//!
//! | 模块 | 职责 | 落点 |
//! |------|------|------|
//! | [`lru`] | 手写 LRU（slab 双向链表 + 哈希表，O(1)，零 unsafe） | roadmap 数据结构表「挂载盘目录缓存」 |
//! | [`memfs`] | 内存文件系统：inode / 路径解析 / FUSE 回调语义的操作集 | 挂载盘的内核回调纯逻辑部分 |
//! | [`dir_cache`] | 目录项 LRU 缓存 + 「先改数据再失效」的一致性纪律 | [`lru`] 的落地场景 |
//! | [`im_view`] | IM 数据 → FS 视图的协议映射（联系人/消息 → 目录/文件） | 挂载盘的世界观翻译 |
//! | [`error`] | POSIX errno 对齐的错误分类（驱动胶水纯翻译） | 跨驱动层的错误契约 |
//!
//! ## 分层与诚实边界
//!
//! 真挂载需要内核态组件（Linux FUSE 设备 / Windows WinFsp 驱动），
//! 那是 unsafe + 平台胶水的领域。本 crate 刻意停在语义层：
//! 驱动接线是把内核回调参数翻译成本模块方法调用的纯胶水，语义
//! 与一致性先在这里钉死（全部可单测），docs/19 记录接线方案与
//! 本机环境（无 WinFsp 驱动时挂载步骤如实标注为环境依赖，与
//! 阶段 9 LiveKit/Docker 的诚实记录同一纪律）。
//!
//! ## 依赖方向
//!
//! **零内部依赖、纯 std**：不依赖 im-protocol/im-server——挂载盘
//! 消费的是 IM 数据的**投影**（谁提供数据谁负责翻译成
//! [`im_view::Contact`]/[`im_view::ChatMessage`]），语义层保持
//! 确定性与可测试性最大化。
//!
//! ## 学习文档
//! - `docs/19-quic-fuse.md`：阶段 13 设计文档（QUIC + 挂载盘）

pub mod dir_cache;
pub mod error;
pub mod im_view;
pub mod lru;
pub mod memfs;

pub use dir_cache::{CachedFs, DirCache};
pub use error::FsError;
pub use lru::LruCache;
pub use memfs::{Attr, DirEntry, Ino, MemFs, NodeKind};

#[cfg(test)]
mod tests {
    // crate 级集成冒烟：四个模块合起来讲一个完整故事——
    // IM 数据 → FS 视图 → 缓存列举 → 读文件。单测都在各自模块，
    // 这里验证的是「拼起来也顺」。
    use super::dir_cache::CachedFs;
    use super::im_view::{ChatMessage, Contact, build_im_view, layout};
    use super::memfs::NodeKind;

    #[test]
    fn end_to_end_im_data_to_mountable_view() {
        let contacts = vec![Contact { name: "alice".into(), username: "alice_wx".into() }];
        let messages = vec![ChatMessage {
            peer: "alice".into(),
            from: "alice".into(),
            text: "挂载盘视角：我就是个文本文件".into(),
            date: "2026-09-25".into(),
        }];
        let fs = build_im_view(&contacts, &messages).expect("投影构建不该失败");
        let mut mounted = CachedFs::new(fs, 4);

        // 模拟 explorer 的消费路径：先列目录（miss）→ 再列（hit）→ 打开文件
        let history_dir = format!("{}/alice", layout::HISTORY_DIR);
        let first = mounted.read_dir(&history_dir).expect("列举历史目录");
        assert_eq!(first.len(), 1);
        let _second = mounted.read_dir(&history_dir).unwrap();
        assert_eq!(mounted.cache_stats(), (1, 1), "第二次列举命中缓存");

        let log_path = layout::history_log("alice", "2026-09-25");
        let log = mounted.read(&log_path).expect("读日志文件");
        let text = String::from_utf8(log.to_vec()).unwrap();
        assert_eq!(text, "alice: 挂载盘视角：我就是个文本文件\n");

        // 属性查询走语义层直通（不缓存——见 dir_cache 的取舍注释）
        let attr = mounted.lookup(&log_path).unwrap();
        assert_eq!(attr.kind, NodeKind::File);
        assert_eq!(attr.size, u64::try_from(text.len()).unwrap());
    }
}
