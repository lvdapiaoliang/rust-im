//! # im-storage：存储层（阶段 4）
//!
//! 客户端本地消息库：聊天历史、离线同步游标、发送重发表的持久化。
//!
//! # 架构（两层）
//!
//! ```text
//! store.rs    LocalStore —— IM 语义层：消息历史 / pending 重发表 / 游标
//! engine.rs   Engine    —— 简化版 LSM：追加段 + memtable + 归并压实
//! ```
//!
//! 底层是**手写的简化版 LSM 存储**（roadmap 4.5 的「B+ 树 / LSM 思想」
//! 落点），不是 SQLite——IM 客户端负载（写多读少 + 万级数据量）恰好是
//! 手写一遍就能吃透 LSM 核心权衡的规模。服务端持久化（阶段 5 后）再
//! 换嵌入式数据库时，`LocalStore` 的语义 API 不变。
//!
//! # 学习文档
//!
//! - `docs/07-client.md`（阶段 4 编写）
//! - `learning-rust-from-scratch/04-algorithms/`：BTreeMap、哈希索引

pub mod engine;
pub mod error;
pub mod store;

pub use error::StorageError;
pub use store::{LocalStore, PendingMsg};

/// 消息 ID 类型：全局唯一、趋势递增（雪花 ID 或服务端分配）。
pub type MessageId = u64;

#[cfg(test)]
mod tests {
    /// 门面测试：crate 根导出路径可用（API 稳定性哨兵）。
    #[test]
    fn public_api_is_reachable() {
        let id: super::MessageId = 1;
        assert_eq!(id, 1);
    }
}
