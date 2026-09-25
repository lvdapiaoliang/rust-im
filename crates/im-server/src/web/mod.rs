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
//! - [`ws`]：WS 网关——JSON 信封 ↔ `Frame` 翻译层 + 会话核心对接
//!   （鉴权在 HTTP 升级前，握手/消息/同步复用会话层状态机）。
//!
//! 与 TCP/TUI 客户端的关系：**双接入并存**。TCP 路径的二进制协议与
//! [`crate::session`] 逻辑一行不改；WS 路径把 JSON 信封翻译成
//! `Frame` 后复用同一套会话核心（见 [`crate::sink::FrameSink`]）。

pub mod account;
pub mod api;
pub mod db;
pub mod fanout;
pub mod files;
pub mod friends;
pub mod groups;
pub mod ws;

/// `u64` 雪花 ID → `i64`（PG `BIGINT`）：发号器保证 < 2^63。
///
/// 集中转换、集中 panic 文档——各仓储的 `bind` 不再重复 `expect`。
///
/// # Panics
///
/// ID ≥ 2^63 时 panic（发号器保证不会发生）。
pub(crate) fn id_i64(id: u64) -> i64 {
    i64::try_from(id).expect("雪花 ID 装得下 i64")
}

/// [`id_i64`] 的反向转换（行元组/`FromRow` → 领域实体用）。
///
/// # Panics
///
/// ID < 0 时 panic（列上存的都是雪花 ID，不会为负）。
pub(crate) fn id_u64(id: i64) -> u64 {
    u64::try_from(id).expect("雪花 ID 装得下 u64")
}

/// 雪花 ID 的 JSON 形态约定：**字符串**。
///
/// 63 位雪花超出 JS `Number.MAX_SAFE_INTEGER`（2^53），数字形态在
/// 前端会静默丢精度（后续所有按 ID 路由的请求全部错位）——与
/// WS 信封（`web::ws` 模块文档）同一约定。入站宽容接受数字或字符串。
pub mod serde_id {
    use serde::{Deserializer, Serializer, de};

    /// 序列化为十进制字符串。
    ///
    /// # Errors
    ///
    /// 序列化器自身失败时上抛（字符串化本身不会失败）。
    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    /// 反序列化：接受字符串（推荐）或数字（宽容手写客户端/测试）。
    ///
    /// # Errors
    ///
    /// 非字符串/数字形态，或字符串不是合法 u64 时报错。
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        struct IdVisitor;

        impl de::Visitor<'_> for IdVisitor {
            type Value = u64;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("字符串或数字形态的雪花 ID")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<u64, E> {
                Ok(value)
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<u64, E> {
                u64::try_from(value).map_err(E::custom)
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<u64, E> {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(IdVisitor)
    }
}
