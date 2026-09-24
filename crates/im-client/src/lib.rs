//! # im-client：IM 客户端
//!
//! 连接、握手认证、收发消息、断线重连、离线同步（阶段 3），
//! 消息级重传、本地持久化、接收去重与 ratatui TUI 界面（阶段 4）。
//!
//! # 学习文档
//!
//! - `docs/07-client.md`
//! - `learning-rust-from-scratch/03-tokio/`：select、channel、超时与取消

pub mod chat;
pub mod client;
mod dedup;
mod outbox;
pub mod tui;

pub use chat::{ChatMsg, ChatState, Conversation, SendStatus};
pub use client::{ClientConfig, ClientError, ClientEvent, ClientHandle, run_client};
