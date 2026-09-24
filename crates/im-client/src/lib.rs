//! # im-client：IM 客户端
//!
//! 阶段 3 的最小客户端：连接、握手认证、收发消息、断线重连、离线同步。
//! TUI 界面（ratatui）是阶段 4 的话题——本 crate 先把「协议跑通」。
//!
//! # 学习文档
//!
//! - `docs/07-client.md`（阶段 4 编写）
//! - `learning-rust-from-scratch/03-tokio/`：select、channel、超时与取消

pub mod chat;
pub mod client;
mod dedup;
mod outbox;

pub use chat::{ChatMsg, ChatState, Conversation, SendStatus};
pub use client::{run_client, ClientConfig, ClientError, ClientEvent, ClientHandle};
