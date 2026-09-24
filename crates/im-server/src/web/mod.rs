//! Web 接入模块（阶段 5）：REST API + WS 网关 + `PostgreSQL` 持久化。
//!
//! ```text
//!   Vue 前端（web/）
//!      │  HTTPS REST（注册/登录/好友/群组/文件）
//!      │  WSS  /ws（JSON 信封，聊天/信令/事件推送）
//!      ▼
//!   ┌─ web 模块（axum）─────────────────────────────┐
//!   │  REST API ──▶ PostgreSQL（sqlx）              │
//!   │  WS 网关  ──▶ 会话核心（Sessions，与 TCP 共用）│
//!   └───────────────────────────────────────────────┘
//! ```
//!
//! 分层纪律（与 TCP 路径对齐）：
//! - [`db`]：连接池与迁移——唯一的 sqlx 连接入口；
//! - [`account`] / [`friends`] / [`groups`] / [`files`]：各域仓储——
//!   业务语义（状态机、事务、安全取舍）收敛在这里；
//! - [`api`]：REST 路由与处理器——只做解析/组装/错误映射，不写 SQL；
//! - WS 网关（JSON 信封 ↔ `Frame` 翻译）在后续子模块落地。
//!
//! 与 TCP/TUI 客户端的关系：**双接入并存**。TCP 路径的二进制协议与
//! [`crate::session`] 逻辑一行不改；WS 路径把 JSON 信封翻译成
//! `Frame` 后复用同一套会话核心（见 [`crate::sink::FrameSink`]）。

pub mod account;
pub mod api;
pub mod db;
pub mod files;
pub mod friends;
pub mod groups;
