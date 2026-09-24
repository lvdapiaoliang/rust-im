//! # im-transport：传输层
//!
//! 本 crate 是整个项目的「网络心脏」，从阶段 0 到阶段 5 持续演进：
//!
//! | 阶段 | 内容 |
//! |------|------|
//! | 0（现在） | Tokio echo server / client：异步 TCP 编程热身 |
//! | 2 | 心跳 keep-alive、指数退避重连、seq/ACK、去重窗口、rustls TLS |
//! | 5 | 性能优化：io_uring、内核调优、SO_REUSEPORT 多进程 |
//! | 8 | QUIC（quinn）备用传输路径，弱网对比测试 |
//!
//! ## 依赖方向
//! 依赖 `im-protocol`（阶段 1 接入帧编解码），被 `im-server` / `im-client` / `im-sdk` 依赖。
//!
//! ## 学习文档
//! - `docs/03-async-tokio.md`：Future / Waker / Tokio 调度模型（配合本 crate 的 echo 代码阅读）
//! - `docs/05-network-tokio.md`：阶段 2 编写

pub mod echo;

pub use echo::{run_echo_client, run_echo_server, serve_connection};
