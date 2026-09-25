//! # im-server：IM 服务端
//!
//! 职责（阶段 3 实现）：
//! - 网关接入层：每连接一个 task + 有界 channel 背压（复用 `im-transport`）
//! - 会话路由表（手写分片并发哈希表 [`router`]）：`user_id` → 连接
//! - 消息扇出、离线消息（内存版，阶段 4 持久化进 `im-storage`）
//! - 雪花 ID（[`snowflake`]）：全局消息 ID 生成
//! - 分布式预留：一致性哈希环路由
//!
//! 学习文档：`docs/06-server-arch.md`

pub mod router;
pub mod session;
pub mod sink;
pub mod snowflake;
pub mod web;

pub use router::{Router, RouterError};
pub use session::{
    AllowAll, Authenticator, DEFAULT_MAX_OFFLINE_PER_USER, DEFAULT_SYNC_BATCH, FanoutOutcome,
    GroupRouter, RouteFuture, SessionConfig, SessionHandle, Sessions, StaticToken, serve,
    serve_connection, spawn_server,
};
pub use sink::{FrameSink, SendFuture, TrySendError};
pub use snowflake::{Snowflake, SnowflakeError, SystemClock};
