//! # im-transport：传输层
//!
//! 本 crate 是整个项目的「网络心脏」，随项目阶段持续演进：
//!
//! | 阶段 | 内容 | 状态 |
//! |------|------|------|
//! | 0 | Tokio echo server / client：异步 TCP 编程热身（[`echo`]） | ✅ |
//! | 2 | 帧连接 [`Connection`]、网关 [`run_gateway_connection`]：心跳、读空闲超时、优雅关闭 | ✅ |
//! | 3 | 指数退避重连、seq/ACK 去重窗口（会话层） | 计划 |
//! | 5 | 性能优化：`io_uring`、内核调优、`SO_REUSEPORT` 多进程 | 计划 |
//! | 7 | rustls TLS | 计划 |
//! | 8 | QUIC（quinn）备用传输路径，弱网对比测试 | 计划 |
//!
//! ## 快速上手（阶段 2 的标准姿势）
//!
//! 服务端对每条 accepted 连接跑一个 [`run_gateway_connection`]，
//! 业务层从入站通道拿 [`InboundFrame`]（帧 + 回话句柄），用
//! `InboundFrame::handle` 回话：
//!
//! ```no_run
//! use im_transport::{run_gateway_connection, GatewayConfig, shutdown_channel};
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
//! let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(64);
//! let (_shutdown_tx, shutdown_rx) = shutdown_channel();
//!
//! let (stream, _peer) = listener.accept().await?;
//! tokio::spawn(run_gateway_connection(
//!     stream,
//!     GatewayConfig::default(),
//!     inbound_tx,
//!     shutdown_rx,
//! ));
//!
//! while let Some(event) = inbound_rx.recv().await {
//!     event.handle.send(event.frame).await?; // echo 业务
//! }
//! # Ok(()) }
//! ```
//!
//! ## 依赖方向
//!
//! 依赖 `im-protocol`（帧编解码），被 `im-server` / `im-client` / `im-sdk` 依赖。
//!
//! ## 学习文档
//! - `docs/03-async-tokio.md`：Future / Waker / Tokio 调度模型
//! - `docs/05-network-tokio.md`：阶段 2 设计文档（连接层/心跳/优雅关闭）
//! - `learning-rust-from-scratch/03-tokio/`：select、channel、超时与取消

pub mod connection;
pub mod dedup;
pub mod echo;
pub mod error;
pub mod gateway;
pub mod shutdown;

pub use connection::{Connection, ReadHalf, WriteHalf};
pub use dedup::{DedupWindow, Verdict, WINDOW_SIZE};
pub use error::TransportError;
pub use gateway::{
    run_gateway_connection, ConnectionHandle, GatewayConfig, HeartbeatPolicy, InboundFrame,
    DEFAULT_IDLE_TIMEOUT,
};
pub use shutdown::{shutdown_channel, ShutdownRx, ShutdownTx};

pub use echo::{
    run_echo_client, run_echo_server, serve_connection, spawn_echo_server_on_random_port,
};
