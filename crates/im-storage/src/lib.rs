//! # im-storage：存储层
//!
//! 职责（阶段 3 起逐步实现）：
//! - 服务端：消息落库、离线消息队列
//! - 客户端：本地消息库（SQLite → SQLCipher 加密）
//! - 写前日志（WAL）思路的高吞吐写入
//!
//! 学习文档：`docs/06-server-arch.md`（存储相关部分）

/// 消息 ID 类型：全局唯一、趋势递增（雪花 ID 或 ULID，阶段 3 定型）
pub type MessageId = u64;

#[cfg(test)]
mod tests {
    #[test]
    fn message_id_is_u64() {
        let id: super::MessageId = 1;
        assert_eq!(id, 1);
    }
}
