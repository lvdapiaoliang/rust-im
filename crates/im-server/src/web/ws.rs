//! WS 网关（阶段 5）：JSON 信封协议 ↔ 二进制帧的翻译层 + 会话核心对接。
//!
//! ```text
//!  浏览器 (WebSocket)
//!     │  文本帧：{"type":"msg","seq":3,"ack":0,"payload":{…}}
//!     ▼
//!  ┌─ web::ws ─────────────────────────────────────────────┐
//!  │  envelope_to_frame / frame_to_envelope（翻译层）       │
//!  │  WsSink: FrameSink 出站通道 → WS 写循环                │
//!  └──────────────┬─────────────────────────────────────────┘
//!                 │ Frame（与 TCP 路径同一格式）
//!                 ▼
//!  会话核心（SessionState::feed_seq 去重 + handle_frame），
//!  复用 Sessions 的路由/离线/同步——传输不同，语义同源。
//! ```
//!
//! # 与 TCP 路径的差异对照
//!
//! | 环节     | TCP（二进制）                    | WS（本模块）                      |
//! |----------|----------------------------------|-----------------------------------|
//! | 鉴权     | `Handshake` 帧 + `Authenticator` | HTTP 升级前查库（`?token=`）      |
//! | 就绪通知 | `HandshakeAck` 帧                | `welcome`（复用 `HandshakeAck` 载荷） |
//! | 心跳     | 网关层 Ping/Pong 控制帧          | 应用层 `ping`/`pong` 信封（浏览器无法发自定义控制帧） |
//! | 去重     | `DedupWindow`（帧 seq）          | 同一套（信封 seq → 帧 seq）       |
//!
//! # ID 串化约定
//!
//! 信封里**所有雪花 ID 一律是 JSON 字符串**：63 位雪花超出 JS
//! `Number.MAX_SAFE_INTEGER`（2^53），数字形态在前端会静默丢精度
//! （Telegram 的网页端同样用字符串 ID）。翻译层入站宽容接受
//! 数字或字符串两种形态，出站统一字符串。

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use im_protocol::{Cmd, Frame, HandshakeAck, Msg, MsgAck, Payload, SyncReq, SyncResp};
use im_transport::TransportError;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::session::{SessionState, Sessions, handle_frame, reply};
use crate::sink::{FrameSink, SendFuture};

use super::api::AppState;

/// 信封类型名（协议的「动词」，对照二进制路径的 [`Cmd`]）。
mod envelope_type {
    /// 上行：发消息。
    pub const MSG: &str = "msg";
    /// 下行：下行消息 / 离线同步里的消息体。
    pub const MSG_ACK: &str = "msg_ack";
    /// 上行：离线同步请求。
    pub const SYNC: &str = "sync";
    /// 下行：离线同步应答。
    pub const SYNC_RESP: &str = "sync_resp";
    /// 下行：连接就绪（升级即认证，注册成功后下发）。
    pub const WELCOME: &str = "welcome";
    /// 上行：应用层心跳。
    pub const PING: &str = "ping";
    /// 下行：心跳应答。
    pub const PONG: &str = "pong";
    /// 下行：协议错误（坏信封/未知类型），连接不断。
    pub const ERROR: &str = "error";
}

/// 出站通道容量：下行推送（消息/事件/回执）+ 控制帧的缓冲上限。
///
/// 与 TCP 写 actor 的队列同职务：瞬时突发吸收；持续打满说明消费者
/// 太慢（阶段 7 的慢消费者隔离在此排队策略上做）。
const OUTBOUND_CAPACITY: usize = 64;

// ────────────────────────────────────────────────────────────────
// 信封结构与翻译层
// ────────────────────────────────────────────────────────────────

/// 上行信封：`{"type":…, "seq":…, "ack":…, "payload":{…}}`。
#[derive(Debug, Deserialize)]
struct Envelope {
    /// 信封类型（见 [`envelope_type`]）。
    #[serde(rename = "type")]
    kind: String,
    /// 客户端上行序号（去重窗口的输入，单调递增）。
    #[serde(default)]
    seq: u64,
    /// 帧级累计确认（与二进制帧头语义一致）。
    #[serde(default)]
    ack: u64,
    /// 类型专属载荷。
    #[serde(default)]
    payload: Value,
}

/// 出站信封（组装即序列化，通道里传文本——与 WS 文本帧同形态）。
fn outbound_envelope(kind: &str, seq: u64, ack: u64, payload: &Value) -> String {
    serde_json::to_string(&json!({
        "type": kind,
        "seq": seq,
        "ack": ack,
        "payload": payload,
    }))
    .expect("信封组装不会失败")
}

/// 协议错误信封：连接保持，前端按 code 提示。
fn error_envelope(code: &str, message: &str) -> String {
    outbound_envelope(envelope_type::ERROR, 0, 0, &json!({ "code": code, "message": message }))
}

/// 入站 ID 取值：宽容接受字符串（推荐，见模块文档）或数字。
fn id_from_value(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// 消息内容 → JSON 值：服务端视角 `content` 一直是不透明字节
/// （阶段 6 的 `{"kind":"text"|…}` 内容模型天然兼容）；非 UTF-8/非 JSON
/// 字节降级为 lossy 字符串，**不丢弃**（展示降级优于静默吞消息）。
fn content_to_value(content: &[u8]) -> Value {
    serde_json::from_slice(content)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(content).into_owned()))
}

/// 下行帧 → 信封文本。`None` = 本帧不出 WS（Ping/Pong 等传输层帧）。
fn frame_to_envelope(frame: &Frame) -> Option<String> {
    let (kind, payload) = match frame.cmd {
        Cmd::HandshakeAck => {
            let ack = HandshakeAck::decode_frame(frame).ok()?;
            (
                envelope_type::WELCOME,
                json!({ "session_id": ack.session_id.to_string(), "reason": ack.reason }),
            )
        }
        Cmd::Msg => {
            let m = Msg::decode_frame(frame).ok()?;
            (
                envelope_type::MSG,
                json!({
                    "from": m.from.to_string(),
                    "to": m.to.to_string(),
                    "msg_id": m.msg_id.to_string(),
                    "client_msg_id": m.client_msg_id.to_string(),
                    "content": content_to_value(&m.content),
                }),
            )
        }
        Cmd::MsgAck => {
            let ack = MsgAck::decode_frame(frame).ok()?;
            (
                envelope_type::MSG_ACK,
                json!({
                    "msg_id": ack.msg_id.to_string(),
                    "client_msg_id": ack.client_msg_id.to_string(),
                }),
            )
        }
        Cmd::SyncResp => {
            let resp = SyncResp::decode_frame(frame).ok()?;
            let messages: Vec<Value> = resp
                .messages
                .iter()
                .map(|m| {
                    json!({
                        "from": m.from.to_string(),
                        "to": m.to.to_string(),
                        "msg_id": m.msg_id.to_string(),
                        "client_msg_id": m.client_msg_id.to_string(),
                        "content": content_to_value(&m.content),
                    })
                })
                .collect();
            (envelope_type::SYNC_RESP, json!({ "messages": messages }))
        }
        // 传输层帧不进业务通道；未知命令字在解码边界已被拦截
        Cmd::Handshake | Cmd::Ping | Cmd::Pong | Cmd::SyncReq => return None,
    };
    Some(outbound_envelope(kind, frame.seq, frame.ack, &payload))
}

/// 上行信封 → 帧。`Ok(None)` = 非业务类型（ping 等，调用方就地处理）；
/// `Err(message)` = 业务类型但载荷不合法（回 error 信封）。
fn envelope_to_frame(env: &Envelope) -> Result<Option<Frame>, &'static str> {
    match env.kind.as_str() {
        envelope_type::MSG => {
            let to = env
                .payload
                .get("to")
                .and_then(id_from_value)
                .ok_or("msg 需要 to（用户或群 ID）")?;
            let client_msg_id =
                env.payload.get("client_msg_id").and_then(id_from_value).unwrap_or(0);
            // content 任意 JSON 值：整体序列化为字节（不透明语义，
            // 阶段 6 的 {"kind":...} 内容模型直接放行）
            let content = env
                .payload
                .get("content")
                .map(Value::to_string)
                .map(Bytes::from)
                .unwrap_or_default();
            // from/msg_id 由服务端裁决（与 TCP 路径同一纪律：伪造无效）
            let msg = Msg { from: 0, to, msg_id: 0, client_msg_id, content };
            Ok(Some(msg.encode_frame(env.seq, env.ack)))
        }
        envelope_type::SYNC => {
            let since = env.payload.get("since").and_then(id_from_value).unwrap_or(0);
            Ok(Some(SyncReq { since }.encode_frame(env.seq, env.ack)))
        }
        envelope_type::PING => Ok(None),
        _ => Err("未知信封类型"),
    }
}

// ────────────────────────────────────────────────────────────────
// FrameSink：WS 出站通道
// ────────────────────────────────────────────────────────────────

/// 出站消息：文本信封或 WS 控制帧（Pong 由 Ping 触发，不走信封）。
#[derive(Debug)]
enum Outbound {
    /// JSON 信封文本。
    Text(String),
    /// 心跳应答控制帧。
    Pong(Bytes),
}

/// WS 版 [`FrameSink`]：帧 → 信封文本 → 出站通道。
///
/// 会话核心（`deliver`/`reply`）拿到的 `Arc<WsSink>` 与 TCP 的
/// `ConnectionHandle` 适配完全同构——它不知道对面是浏览器。
#[derive(Debug)]
struct WsSink {
    tx: mpsc::Sender<Outbound>,
}

impl FrameSink for WsSink {
    fn send(&self, frame: Frame) -> SendFuture<'_> {
        Box::pin(async move {
            // None（传输层帧）按成功对待：语义是「这条我管了」，调用方
            // 无需知道 WS 路径根本不会发这种帧
            let Some(text) = frame_to_envelope(&frame) else {
                return Ok(());
            };
            self.tx.send(Outbound::Text(text)).await.map_err(|_| TransportError::Closed)
        })
    }
}

// ────────────────────────────────────────────────────────────────
// 接入：HTTP 升级与连接生命周期
// ────────────────────────────────────────────────────────────────

/// `GET /ws?token=…` 的查询参数。
#[derive(Debug, Deserialize)]
pub struct WsParams {
    /// 登录令牌（REST `/api/login` 签发的同一个）。
    pub token: Option<String>,
}

/// WS 入口：鉴权在升级**之前**——坏令牌连 WebSocket 都不建立
/// （省一次握手往返；客户端拿到标准 401 而非协议内错误）。
///
/// 鉴权通过 ≠ 注册成功：升级后仍可能撞「单端登录」（同账号已在线），
/// 那是 `welcome` 信封里说的事。
pub async fn ws_handler(
    State(state): State<AppState>,
    Query(params): Query<WsParams>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(token) = params.token else {
        return (StatusCode::UNAUTHORIZED, "缺少 token 参数").into_response();
    };
    match state.accounts.user_by_token(&token).await {
        Ok(user) => ws.on_upgrade(move |socket| handle_socket(state, user.id, socket)),
        Err(_) => (StatusCode::UNAUTHORIZED, "无效或过期的令牌").into_response(),
    }
}

/// 一条 WS 连接的会话生命周期（与 TCP 的 `serve_connection` 同构）：
/// 注册路由 → `welcome` → 读写循环 → 注销收尾。
///
/// 读循环与写出站是同一 `select` 的两个分支（`split` 把 WebSocket
/// 拆成 Sink/Stream 两半，各自被一个分支独占借用）。
async fn handle_socket(state: AppState, user_id: u64, socket: WebSocket) {
    let sessions = state.sessions.clone();
    let (mut outbound, mut inbound) = socket.split();

    let (tx, mut rx) = mpsc::channel::<Outbound>(OUTBOUND_CAPACITY);
    let sink: Arc<dyn FrameSink> = Arc::new(WsSink { tx: tx.clone() });
    let conn_id = sessions.next_conn_id();

    // 鉴权已在升级前完成：直接以已认证状态进入会话状态机
    let mut session = SessionState::authenticated(user_id);

    // 「握手」只剩注册与会话 ID 分配（TCP 路径的其余握手职责都已被
    // HTTP 升级吸收）；失败也走 welcome 信封告知原因后关闭——
    // 客户端要能区分「令牌无效」与「已在别处登录」
    let welcome =
        match (sessions.next_id().await, sessions.register(user_id, conn_id, Arc::clone(&sink))) {
            (Some(session_id), Ok(())) => HandshakeAck::accepted(session_id),
            (Some(_), Err(_)) => HandshakeAck::rejected("already online"),
            (None, _) => HandshakeAck::rejected("id generator unavailable"),
        };
    if reply(&mut session, &sink, &welcome).await.is_err() {
        return; // 连接已死：直接收尾
    }
    let accepted = welcome.is_accepted();

    // 主循环：出站排空与入站分发并发（谁先结束都退出）
    loop {
        tokio::select! {
            out = rx.recv() => {
                match out {
                    Some(Outbound::Text(text)) => {
                        if outbound.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(Outbound::Pong(data)) => {
                        if outbound.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    None => break, // 会话路径全部关闭：无需再写
                }
            }
            inbound = inbound.next() => {
                match inbound {
                    // 对端关闭、协议错误、Close 帧：连接生命周期终结
                    None | Some(Err(_) | Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Ping(data))) => {
                        // 浏览器不发 Ping，但非浏览器客户端可能发——照回不误
                        let _ = tx.try_send(Outbound::Pong(data));
                    }
                    Some(Ok(Message::Text(text))) => {
                        if !accepted {
                            // 已拒绝还继续说话：直接关闭（正常客户端见
                            // welcome 即断开）
                            break;
                        }
                        dispatch_inbound(&sessions, &mut session, conn_id, &sink, &tx, text.as_str()).await;
                    }
                    Some(Ok(_)) => {} // Binary 等非文本帧：忽略（协议是文本 JSON）
                }
            }
        }
    }

    // 收尾：注销自己的注册（值校验，见 Sessions::unregister）
    let _ = sessions.unregister(user_id, conn_id);
    // 出站通道随 WsSink drop；这里补一个 Close 让对端尽快感知
    let _ = outbound.send(Message::Close(None)).await;
}

/// 入站信封分发：解析 →（ping 就地回 / 业务帧翻译 → 去重 → 会话核心）。
async fn dispatch_inbound(
    sessions: &Sessions,
    session: &mut SessionState,
    conn_id: u64,
    sink: &Arc<dyn FrameSink>,
    tx: &mpsc::Sender<Outbound>,
    text: &str,
) {
    let env: Envelope = match serde_json::from_str(text) {
        Ok(env) => env,
        Err(e) => {
            let _ = tx.try_send(Outbound::Text(error_envelope("bad_envelope", &e.to_string())));
            return;
        }
    };

    let frame = match envelope_to_frame(&env) {
        Err(message) => {
            let _ = tx.try_send(Outbound::Text(error_envelope("bad_payload", message)));
            return;
        }
        Ok(None) => {
            // 应用层心跳：回显对端 seq，前端按它配对
            let pong = outbound_envelope(envelope_type::PONG, env.seq, 0, &json!({}));
            let _ = tx.try_send(Outbound::Text(pong));
            return;
        }
        Ok(Some(frame)) => frame,
    };
    // 帧级去重：与 TCP 路径同一窗口同一语义（重发/乱序在业务前被挡下）
    match session.feed_seq(frame.seq) {
        im_transport::Verdict::Duplicate | im_transport::Verdict::TooFar { .. } => return,
        im_transport::Verdict::InOrder | im_transport::Verdict::OutOfOrder => {}
    }

    handle_frame(sessions, session, conn_id, &frame, sink).await;
}

// ────────────────────────────────────────────────────────────────
// 测试
// ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionConfig;
    use crate::web::db::testing::pool_or_skip;

    /// 翻译层：msg 上行往返（信封 → 帧 → 载荷字段一致，content 整体序列化）。
    #[test]
    fn msg_envelope_roundtrip() {
        let env = Envelope {
            kind: envelope_type::MSG.to_string(),
            seq: 3,
            ack: 0,
            payload: json!({ "to": "42", "client_msg_id": 7, "content": "你好" }),
        };
        let frame = envelope_to_frame(&env).expect("msg 信封应可翻译").expect("应为业务帧");
        assert_eq!(frame.cmd, Cmd::Msg);
        assert_eq!(frame.seq, 3);

        let msg = Msg::decode_frame(&frame).expect("载荷应与命令字匹配");
        assert_eq!(msg.to, 42);
        assert_eq!(msg.client_msg_id, 7);
        // content 是 JSON 值的序列化字节（不透明语义）
        assert_eq!(msg.content, Bytes::from("\"你好\"".to_string()));
    }

    /// 翻译层：ID 双形态入站（字符串与数字都认——宽容协议面向手写客户端）。
    #[test]
    fn ids_accept_number_and_string_forms() {
        let env = Envelope {
            kind: envelope_type::SYNC.to_string(),
            seq: 1,
            ack: 0,
            payload: json!({ "since": 123 }),
        };
        let frame = envelope_to_frame(&env).expect("sync 信封应可翻译").expect("应为业务帧");
        let req = SyncReq::decode_frame(&frame).expect("载荷应与命令字匹配");
        assert_eq!(req.since, 123);
    }

    /// 翻译层：下行帧 → 信封（ID 串化、content 反解为 JSON）。
    #[test]
    fn downstream_frame_becomes_envelope() {
        let msg = Msg {
            from: 1,
            to: 2,
            msg_id: 1_234_567_890_123_456_789,
            client_msg_id: 5,
            content: Bytes::from_static(b"{\"kind\":\"text\",\"text\":\"hi\"}"),
        };
        let frame = msg.encode_frame(9, 3);
        let text = frame_to_envelope(&frame).expect("Msg 帧应翻译为信封");
        let env: Value = serde_json::from_str(&text).expect("信封应是合法 JSON");

        assert_eq!(env["type"], envelope_type::MSG);
        assert_eq!(env["seq"], 9);
        assert_eq!(env["ack"], 3);
        // 雪花 ID 串化（超 JS 安全整数范围也不丢精度）
        assert_eq!(env["payload"]["msg_id"], "1234567890123456789");
        // content 反解回结构化 JSON
        assert_eq!(env["payload"]["content"]["kind"], "text");
    }

    /// 翻译层：非 JSON 字节的内容降级为 lossy 字符串（不丢消息）。
    #[test]
    fn non_json_content_degrades_gracefully() {
        let msg = Msg {
            from: 1,
            to: 2,
            msg_id: 3,
            client_msg_id: 4,
            content: Bytes::from_static(b"\xff\xfe raw bytes"),
        };
        let text = frame_to_envelope(&msg.encode_frame(1, 0)).expect("应可翻译");
        let env: Value = serde_json::from_str(&text).expect("信封应是合法 JSON");
        assert!(env["payload"]["content"].is_string());
    }

    /// 翻译层：未知类型回错误；ping 不产生业务帧。
    #[test]
    fn unknown_type_and_ping() {
        let unknown = Envelope { kind: "wat".to_string(), seq: 1, ack: 0, payload: json!({}) };
        assert_eq!(envelope_to_frame(&unknown), Err("未知信封类型"));

        let ping =
            Envelope { kind: envelope_type::PING.to_string(), seq: 2, ack: 0, payload: json!({}) };
        assert!(envelope_to_frame(&ping).expect("ping 不该报错").is_none());
    }

    /// WsSink：传输层帧（Ping）静默成功；信封文本进通道。
    #[tokio::test]
    async fn ws_sink_translates_and_skips_transport_frames() {
        let (tx, mut rx) = mpsc::channel(4);
        let sink = WsSink { tx };

        let ping = Frame::new(Cmd::Ping, 1, 0, Bytes::new());
        sink.send(ping).await.expect("传输层帧按成功对待");
        assert!(rx.try_recv().is_err(), "Ping 不产生出站消息");

        let ack = MsgAck { msg_id: 7, client_msg_id: 8 };
        sink.send(ack.encode_frame(1, 0)).await.expect("信封应入通道");
        let Outbound::Text(text) = rx.recv().await.expect("应收到出站文本") else {
            panic!("应为文本出站");
        };
        let env: Value = serde_json::from_str(&text).expect("应为合法 JSON");
        assert_eq!(env["type"], envelope_type::MSG_ACK);
        assert_eq!(env["payload"]["msg_id"], "7");
    }

    // ── 集成测试（真 HTTP + 真 WS + 真 PG；PG 不可达则跳过）──

    use std::time::Duration;

    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message as TgMessage;
    use tokio_tungstenite::tungstenite::http::StatusCode as HttpCode;
    use tokio_tungstenite::{connect_async, tungstenite};

    /// 测试等待上限：本地回环上任何正常交互都应远快于此。
    const WAIT: Duration = Duration::from_secs(5);

    /// 起 WS 测试服务：迁移到位的 `AppState` + 随机端口的 axum 服务。
    async fn ws_server_or_skip() -> Option<(String, AppState)> {
        let pool = pool_or_skip().await?;
        let root = std::env::temp_dir().join(format!("im-ws-{}", uuid::Uuid::new_v4()));
        let sessions = Sessions::new(SessionConfig::default());
        let state = AppState::new(pool, sessions, &root).await.ok()?;
        let app = super::super::api::router(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.ok()?;
        let addr = listener.local_addr().ok()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Some((format!("ws://{addr}/ws"), state))
    }

    /// 直接建用户并签发令牌（不经 REST——本模块只考 WS 语义）。
    async fn user_with_token(state: &AppState, name: &str) -> (u64, String) {
        let user = state
            .accounts
            .register(&state.sessions, name, "pw123456", name)
            .await
            .expect("注册应成功");
        let token = state
            .accounts
            .issue_token(user.id, Duration::from_secs(3600))
            .await
            .expect("签发应成功");
        (user.id, token)
    }

    /// WS 客户端包装：收发 JSON 信封 + 自增 seq。
    struct WsClient {
        stream: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        seq: u64,
    }

    impl WsClient {
        async fn connect(url: &str) -> Self {
            let (stream, _) = connect_async(url).await.expect("WS 连接应成功");
            Self { stream, seq: 0 }
        }

        fn next_seq(&mut self) -> u64 {
            self.seq += 1;
            self.seq
        }

        /// 发一个信封（`seq` 自动填）。
        async fn send(&mut self, kind: &str, payload: Value) {
            let seq = self.next_seq();
            let text = outbound_envelope(kind, seq, 0, &payload);
            self.stream.send(TgMessage::Text(text.into())).await.expect("发送应成功");
        }

        /// 收一个信封（跳过协议级 Pong 帧）。
        async fn recv(&mut self) -> Value {
            loop {
                let msg = tokio::time::timeout(WAIT, self.stream.next())
                    .await
                    .expect("5s 内应收到信封")
                    .expect("连接应存活")
                    .expect("帧应合法");
                if let TgMessage::Text(text) = msg {
                    return serde_json::from_str(text.as_str()).expect("信封应为合法 JSON");
                }
            }
        }
    }

    /// 坏令牌：HTTP 层直接拒绝（连 WebSocket 都不升级）。
    #[tokio::test]
    async fn bad_token_is_rejected_before_upgrade() {
        let Some((url, _state)) = ws_server_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };

        let full = format!("{url}?token=not-a-real-token");
        match connect_async(full).await {
            Err(tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status(), HttpCode::UNAUTHORIZED);
            }
            Err(e) => panic!("应为 HTTP 层拒绝: {e}"),
            Ok(_) => panic!("坏令牌不应升级"),
        }
    }

    /// 主线：`welcome`（session_id 非零）→ 双端互发 → 对端收 `msg`、本端收 `msg_ack`。
    #[tokio::test]
    async fn welcome_and_msg_roundtrip() {
        let Some((url, state)) = ws_server_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let (id_a, token_a) =
            user_with_token(&state, &format!("ws_a_{}", uuid::Uuid::new_v4().simple())).await;
        let (id_b, token_b) =
            user_with_token(&state, &format!("ws_b_{}", uuid::Uuid::new_v4().simple())).await;

        let mut alice = WsClient::connect(&format!("{url}?token={token_a}")).await;
        let mut bob = WsClient::connect(&format!("{url}?token={token_b}")).await;

        // 双端 welcome：session_id 非零、seq 从 1 开始
        for c in [&mut alice, &mut bob] {
            let welcome = c.recv().await;
            assert_eq!(welcome["type"], envelope_type::WELCOME);
            assert_eq!(welcome["seq"], 1);
            assert!(welcome["payload"]["session_id"].as_str().is_some_and(|s| s != "0"));
        }

        // A → B
        alice
            .send(
                envelope_type::MSG,
                json!({ "to": id_b.to_string(), "client_msg_id": 1, "content": "hi bob" }),
            )
            .await;

        let got = bob.recv().await;
        assert_eq!(got["type"], envelope_type::MSG);
        assert_eq!(got["payload"]["from"], id_a.to_string());
        assert_eq!(got["payload"]["to"], id_b.to_string());
        assert_eq!(got["payload"]["content"], "hi bob");

        let ack = alice.recv().await;
        assert_eq!(ack["type"], envelope_type::MSG_ACK);
        assert_eq!(ack["payload"]["client_msg_id"], "1");
        assert_eq!(ack["payload"]["msg_id"], got["payload"]["msg_id"], "确认与下行同 ID");
    }

    /// 单端登录：同账号第二个连接被 welcome 拒绝，且旧连接不受影响。
    #[tokio::test]
    async fn duplicate_login_is_rejected_via_welcome() {
        let Some((url, state)) = ws_server_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let name = format!("ws_dup_{}", uuid::Uuid::new_v4().simple());
        let (_id, token) = user_with_token(&state, &name).await;

        let mut first = WsClient::connect(&format!("{url}?token={token}")).await;
        let welcome = first.recv().await;
        assert!(welcome["payload"]["session_id"].as_str().is_some_and(|s| s != "0"));

        let mut second = WsClient::connect(&format!("{url}?token={token}")).await;
        let rejected = second.recv().await;
        assert_eq!(rejected["type"], envelope_type::WELCOME);
        assert_eq!(rejected["payload"]["session_id"], "0");
        assert_eq!(rejected["payload"]["reason"], "already online");

        // 旧连接照常工作（发消息有回执）
        first.send(envelope_type::PING, json!({})).await;
        let pong = first.recv().await;
        assert_eq!(pong["type"], envelope_type::PONG);
    }

    /// 离线暂存 + 同步：B 未连接时 A 发消息；B 连上后 sync 拉走。
    #[tokio::test]
    async fn offline_msg_is_synced_after_reconnect() {
        let Some((url, state)) = ws_server_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let (id_a, token_a) =
            user_with_token(&state, &format!("ws_off_a_{}", uuid::Uuid::new_v4().simple())).await;
        let (id_b, token_b) =
            user_with_token(&state, &format!("ws_off_b_{}", uuid::Uuid::new_v4().simple())).await;

        let mut alice = WsClient::connect(&format!("{url}?token={token_a}")).await;
        let _ = alice.recv().await; // 消化 welcome

        alice
            .send(
                envelope_type::MSG,
                json!({ "to": id_b.to_string(), "client_msg_id": 1, "content": "offline-hello" }),
            )
            .await;
        let ack = alice.recv().await;
        assert_eq!(ack["type"], envelope_type::MSG_ACK);

        // B 上线：welcome 后立即同步
        let mut bob = WsClient::connect(&format!("{url}?token={token_b}")).await;
        let _ = bob.recv().await; // 消化 welcome
        bob.send(envelope_type::SYNC, json!({ "since": 0 })).await;

        let resp = bob.recv().await;
        assert_eq!(resp["type"], envelope_type::SYNC_RESP);
        let messages = resp["payload"]["messages"].as_array().expect("应为数组");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["from"], id_a.to_string());
        assert_eq!(messages[0]["content"], "offline-hello");
    }

    /// 坏信封：error 信封回执且连接不断（后续消息照常处理）。
    #[tokio::test]
    async fn malformed_envelope_gets_error_but_connection_survives() {
        let Some((url, state)) = ws_server_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let (_id, token) =
            user_with_token(&state, &format!("ws_bad_{}", uuid::Uuid::new_v4().simple())).await;
        let mut client = WsClient::connect(&format!("{url}?token={token}")).await;
        let _ = client.recv().await; // 消化 welcome

        // 不是 JSON
        client.stream.send(TgMessage::Text("not json".into())).await.expect("发送应成功");
        let err = client.recv().await;
        assert_eq!(err["type"], envelope_type::ERROR);
        assert_eq!(err["payload"]["code"], "bad_envelope");

        // 未知类型
        client.send("wat", json!({})).await;
        let err = client.recv().await;
        assert_eq!(err["type"], envelope_type::ERROR);
        assert_eq!(err["payload"]["code"], "bad_payload");

        // 连接仍活着：ping → pong
        client.send(envelope_type::PING, json!({})).await;
        let pong = client.recv().await;
        assert_eq!(pong["type"], envelope_type::PONG);
    }
}
