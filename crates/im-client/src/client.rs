//! 最小客户端：连接/握手/收发/重连/离线同步一体的状态机。
//!
//! # 架构（每客户端一个「连接状态机 task」）
//!
//! ```text
//!   业务层 ◀── mpsc ── ClientEvent     ClientCommand ── mpsc ──▶ 业务层(发送)
//!     ▲                                    │
//!     │            ┌───────────────────────┴────────┐
//!     │            │       run_client 主循环         │
//!     │            │  loop { connect_once(...);      │
//!     │            │        backoff.next_delay() }   │
//!     │            └───────┬────────────────────────┘
//!     │                    │ 每轮连接：spawn_gateway（心跳/写actor）
//!     │                    │ 握手 → SyncReq → select{ 命令, 入站帧 }
//!     │                    ▼
//!     └────────────────── TCP ──▶ 服务端
//! ```
//!
//! # 模式落点
//!
//! - **重连策略与状态机解耦**：`Backoff`（阶段 3 传输层）只管「下次等多久」，
//!   连接状态机只管「一轮连接的生命周期」——组合而非纠缠；
//! - **事件驱动**：客户端对业务层只暴露 `ClientEvent` 流，
//!   UI（阶段 4 的 TUI）订阅事件即可，不碰任何协议细节（观察者模式的通道版）；
//! - **命令排队即断线缓冲**：断线期间 `send_msg` 只是入队，
//!   重连成功后由新一轮连接循环统一发出——「无连接时发送」天然被缓冲
//!   （代价：未 ACK 的消息断线可能丢，阶段 4 本地消息库补重发表）。

use std::time::Duration;

use bytes::Bytes;
use im_protocol::{Handshake, HandshakeAck, Msg, MsgAck, Payload, SyncReq, SyncResp};
use im_transport::{
    shutdown_channel, spawn_gateway, Backoff, GatewayConfig, HeartbeatPolicy, InboundFrame,
    ShutdownRx,
};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

/// 默认心跳间隔（经验值：明显小于服务端 60s 空闲超时）。
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// 默认重连基准间隔（full jitter 会在此基础上打散）。
pub const DEFAULT_BACKOFF_BASE: Duration = Duration::from_secs(1);
/// 默认重连封顶间隔。
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// 握手应答等待上限：超时按连接故障处理（触发重连）。
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 客户端配置。
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// 服务端地址（`"127.0.0.1:8888"`）。
    pub server_addr: String,
    /// 登录用户 ID。
    pub user_id: u64,
    /// 认证令牌。
    pub token: String,
    /// 心跳间隔。
    pub heartbeat_interval: Duration,
    /// 重连基准间隔（指数退避的 base）。
    pub backoff_base: Duration,
    /// 重连封顶间隔。
    pub backoff_max: Duration,
    /// 握手应答等待上限。
    pub handshake_timeout: Duration,
}

impl ClientConfig {
    /// 以「地址 + 身份」构造，其余取默认值。
    #[must_use]
    pub fn new(server_addr: impl Into<String>, user_id: u64, token: impl Into<String>) -> Self {
        Self {
            server_addr: server_addr.into(),
            user_id,
            token: token.into(),
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            backoff_base: DEFAULT_BACKOFF_BASE,
            backoff_max: DEFAULT_BACKOFF_MAX,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }
}

/// 客户端对业务层的事件流（观察者模式的通道版）。
#[derive(Debug, Clone)]
pub enum ClientEvent {
    /// 握手成功（`session_id` 为服务端分配的会话 ID）。
    Connected {
        /// 服务端分配的会话 ID（雪花）。
        session_id: u64,
    },
    /// 连接断开（客户端会自动重连，业务层通常只需提示「连接中」）。
    Disconnected,
    /// 收到一条下行消息。
    Message(Msg),
    /// 一条上行消息被服务端确认（`msg_id` 可用于消除本地「发送中」标记）。
    Ack {
        /// 被确认的全局消息 ID。
        msg_id: u64,
    },
    /// 一批离线消息到达（连接建立后自动拉取）。
    SyncBatch(Vec<Msg>),
    /// 握手被服务端拒绝：**不再重连**（重试没有意义——换 token 或账号）。
    Rejected {
        /// 服务端给出的拒绝原因。
        reason: String,
    },
}

/// 业务层发给客户端的命令。
#[derive(Debug)]
enum ClientCommand {
    /// 发一条消息（`from`/`msg_id` 由服务端裁决）。
    SendMsg {
        /// 接收者用户 ID。
        to: u64,
        /// 消息内容。
        content: Bytes,
    },
}

/// 客户端错误。
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// 命令通道已关闭（客户端已退出）。
    #[error("client stopped: command channel closed")]
    Stopped,
}

/// 客户端发送句柄：克隆廉价，任意 task 可持有。
#[derive(Clone, Debug)]
pub struct ClientHandle {
    cmd_tx: mpsc::Sender<ClientCommand>,
}

impl ClientHandle {
    /// 发一条消息。
    ///
    /// 返回 `Ok` 只表示**命令已入队**（断线时排队等重连），
    /// 不等于已送达——送达以 [`ClientEvent::Ack`] 为准。
    ///
    /// # Errors
    ///
    /// 客户端已退出时返回 [`ClientError::Stopped`]。
    pub async fn send_msg(&self, to: u64, content: Bytes) -> Result<(), ClientError> {
        self.cmd_tx
            .send(ClientCommand::SendMsg { to, content })
            .await
            .map_err(|_| ClientError::Stopped)
    }
}

/// 一轮连接的结局（驱动外层重连循环的状态转移）。
enum Outcome {
    /// 连接结束（网络故障/对端关闭/握手超时）：值得重连。
    Disconnected,
    /// 握手被拒：重连无意义，停止。
    Rejected(String),
    /// 外部关停：正常退出。
    Stopped,
}

/// 启动客户端：后台运行连接状态机，立刻返回发送句柄。
///
/// 事件经 `events` 流向业务层；`shutdown` 触发后客户端排干命令队列并退出。
///
/// # Examples
///
/// ```no_run
/// use im_client::{run_client, ClientConfig};
/// use tokio::sync::mpsc;
///
/// # async fn example() {
/// let config = ClientConfig::new("127.0.0.1:8888", 42, "token");
/// let (events_tx, mut events_rx) = mpsc::channel(64);
/// let (_shutdown_tx, shutdown_rx) = im_transport::shutdown_channel();
///
/// let handle = run_client(config, events_tx, shutdown_rx).await;
/// handle.send_msg(7, bytes::Bytes::from_static(b"hi")).await.unwrap();
///
/// while let Some(event) = events_rx.recv().await {
///     println!("{event:?}");
/// }
/// # }
/// ```
pub async fn run_client(
    config: ClientConfig,
    events: mpsc::Sender<ClientEvent>,
    shutdown: ShutdownRx,
) -> ClientHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    tokio::spawn(client_loop(config, events, cmd_rx, shutdown));
    ClientHandle { cmd_tx }
}

/// 连接状态机主循环：`connect_once` + 指数退避。
async fn client_loop(
    config: ClientConfig,
    events: mpsc::Sender<ClientEvent>,
    mut cmd_rx: mpsc::Receiver<ClientCommand>,
    shutdown: ShutdownRx,
) {
    let mut backoff = Backoff::new(config.backoff_base, config.backoff_max);
    // 同步游标：已收到的最大 `msg_id`（跨重连保留——重连只补增量）
    let mut last_msg_id = 0u64;

    loop {
        let (outcome, cursor) =
            connect_once(&config, &events, &mut cmd_rx, &shutdown, last_msg_id).await;
        last_msg_id = cursor;

        match outcome {
            Outcome::Rejected(reason) => {
                let _ = events.send(ClientEvent::Rejected { reason }).await;
                break;
            }
            Outcome::Stopped => break,
            Outcome::Disconnected => {
                let _ = events.send(ClientEvent::Disconnected).await;
                // full jitter 的退避：雷打散，重连风暴不成形
                sleep(backoff.next_delay()).await;
            }
        }
    }
}

/// 一轮连接：TCP 连接 → 网关（心跳）→ 握手 → 离线同步 → 消息循环。
///
/// 返回（结局, 最新的同步游标）。任何失败路径都先等网关收尾——
/// 本函数返回时这轮连接的资源确定已回收。
async fn connect_once(
    config: &ClientConfig,
    events: &mpsc::Sender<ClientEvent>,
    cmd_rx: &mut mpsc::Receiver<ClientCommand>,
    shutdown: &ShutdownRx,
    last_msg_id: u64,
) -> (Outcome, u64) {
    // 本轮连接的关停信号：外层 shutdown 或本轮结束时触发，停掉网关
    let (local_shutdown_tx, local_shutdown_rx) = shutdown_channel();
    let mut outer_shutdown = shutdown.clone();

    // 1. TCP 连接失败：按普通断线处理（外层退避重试）
    let stream = match TcpStream::connect(&config.server_addr).await {
        Ok(s) => s,
        Err(_) => return (Outcome::Disconnected, last_msg_id),
    };

    // 2. 网关（客户端角色：心跳保活、写 actor、空闲超时）
    let gateway_config = GatewayConfig {
        heartbeat: HeartbeatPolicy::Client {
            interval: config.heartbeat_interval,
        },
        ..GatewayConfig::default()
    };
    let (frame_tx, mut frame_rx) = mpsc::channel::<InboundFrame>(32);
    let (handle, gateway_task) = spawn_gateway(stream, gateway_config, frame_tx, local_shutdown_rx);

    // 3. 握手（上行 seq 每轮从 1 重新开始——服务端去重窗口以首帧为基准，
    //    不依赖跨连接的 seq 连续性）
    let mut send_seq: u64 = 0;
    let handshake = Handshake {
        user_id: config.user_id,
        token: config.token.clone(),
    };
    send_seq += 1;
    if handle
        .send(handshake.encode_frame(send_seq, 0))
        .await
        .is_err()
    {
        let _ = gateway_task.await;
        return (Outcome::Disconnected, last_msg_id);
    }

    // 4. 等握手应答（区分「被拒」与「网络故障」——语义不同，处理不同）
    let ack = match timeout(config.handshake_timeout, frame_rx.recv()).await {
        Ok(Some(event)) => match HandshakeAck::decode_frame(&event.frame) {
            Ok(ack) => ack,
            Err(_) => {
                // 协议错乱的应答：按断线处理
                finish_gateway(local_shutdown_tx, gateway_task).await;
                return (Outcome::Disconnected, last_msg_id);
            }
        },
        Ok(None) | Err(_) => {
            finish_gateway(local_shutdown_tx, gateway_task).await;
            return (Outcome::Disconnected, last_msg_id);
        }
    };
    if !ack.is_accepted() {
        finish_gateway(local_shutdown_tx, gateway_task).await;
        return (Outcome::Rejected(ack.reason), last_msg_id);
    }
    let session_id = ack.session_id;
    if events
        .send(ClientEvent::Connected { session_id })
        .await
        .is_err()
    {
        // 业务层不在了：客户端没有存在意义
        finish_gateway(local_shutdown_tx, gateway_task).await;
        return (Outcome::Stopped, last_msg_id);
    }

    // 5. 离线同步：从上次游标补齐断线期间的消息
    let mut cursor = last_msg_id;
    send_seq += 1;
    let sync_req = SyncReq { since: last_msg_id };
    if handle.send(sync_req.encode_frame(send_seq, 0)).await.is_err() {
        finish_gateway(local_shutdown_tx, gateway_task).await;
        return (Outcome::Disconnected, cursor);
    }

    // 6. 消息循环：命令与入站帧双路 select
    let outcome = loop {
        tokio::select! {
            // 外部关停：退出（网关在 finish_gateway 里收尾）
            () = outer_shutdown.wait() => break Outcome::Stopped,

            cmd = cmd_rx.recv() => match cmd {
                Some(ClientCommand::SendMsg { to, content }) => {
                    let msg = Msg { from: 0, to, msg_id: 0, content };
                    send_seq += 1;
                    // 发送失败 = 本轮连接已死：交给断线路径
                    if handle.send(msg.encode_frame(send_seq, 0)).await.is_err() {
                        break Outcome::Disconnected;
                    }
                }
                None => break Outcome::Stopped, // 业务层放手：正常退出
            },

            frame = frame_rx.recv() => match frame {
                Some(event) => match event.frame.cmd {
                    im_protocol::Cmd::Msg => {
                        if let Ok(msg) = Msg::decode_frame(&event.frame) {
                            cursor = cursor.max(msg.msg_id);
                            if events.send(ClientEvent::Message(msg)).await.is_err() {
                                break Outcome::Stopped;
                            }
                        }
                    }
                    im_protocol::Cmd::MsgAck => {
                        if let Ok(ack) = MsgAck::decode_frame(&event.frame) {
                            if events
                                .send(ClientEvent::Ack { msg_id: ack.msg_id })
                                .await
                                .is_err()
                            {
                                break Outcome::Stopped;
                            }
                        }
                    }
                    im_protocol::Cmd::SyncResp => {
                        if let Ok(resp) = SyncResp::decode_frame(&event.frame) {
                            if let Some(max_id) = resp.messages.iter().map(|m| m.msg_id).max() {
                                cursor = cursor.max(max_id);
                            }
                            if events
                                .send(ClientEvent::SyncBatch(resp.messages))
                                .await
                                .is_err()
                            {
                                break Outcome::Stopped;
                            }
                        }
                    }
                    // Pong（心跳应答）：忽略；其他命令字与本客户端无关
                    _ => {}
                },
                // 网关退出：连接死亡（EOF/IO 故障/空闲超时）
                None => break Outcome::Disconnected,
            },
        }
    };

    finish_gateway(local_shutdown_tx, gateway_task).await;
    (outcome, cursor)
}

/// 收尾一轮连接：触发关停、等网关排干退出。
async fn finish_gateway(
    shutdown_tx: im_transport::ShutdownTx,
    gateway_task: tokio::task::JoinHandle<Result<(), im_transport::TransportError>>,
) {
    shutdown_tx.trigger();
    let _ = gateway_task.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use im_server::{AllowAll, SessionConfig, Sessions};
    use im_transport::{shutdown_channel, ShutdownTx};
    use std::net::SocketAddr;
    use tokio::time::timeout;

    const WAIT: Duration = Duration::from_secs(2);

    /// 起完整服务端（随机端口，AllowAll 认证）。
    ///
    /// 关停信号语义是「sender 全部 drop = 视为已关停」，而本辅助函数返回后
    /// 局部的 shutdown_tx 会被 drop——服务端会立即退场。泄漏这一份 sender
    /// 保活（watch::Sender 极小，测试进程内泄漏无害）。
    async fn server() -> SocketAddr {
        let (addr, _sessions, shutdown) =
            im_server::spawn_server(SessionConfig {
                authenticator: std::sync::Arc::new(AllowAll),
                ..SessionConfig::default()
            })
            .await
            .expect("服务应能启动");
        std::mem::forget(shutdown);
        addr
    }

    /// 客户端测试脚手架：起客户端 + 事件接收端 + 关停信号。
    struct TestClient {
        handle: ClientHandle,
        events: mpsc::Receiver<ClientEvent>,
        _shutdown: ShutdownTx,
    }

    async fn client(addr: SocketAddr, user_id: u64, backoff_base: Duration) -> TestClient {
        let config = ClientConfig {
            backoff_base,
            ..ClientConfig::new(addr.to_string(), user_id, "any")
        };
        let (events_tx, events_rx) = mpsc::channel(64);
        let (shutdown_tx, shutdown_rx) = shutdown_channel();
        let handle = run_client(config, events_tx, shutdown_rx).await;
        TestClient {
            handle,
            events: events_rx,
            _shutdown: shutdown_tx,
        }
    }

    /// 等待下一个事件（断言在 WAIT 内到达）。
    async fn next_event(events: &mut mpsc::Receiver<ClientEvent>) -> ClientEvent {
        timeout(WAIT, events.recv())
            .await
            .expect("2s 内应收到事件")
            .expect("客户端存活")
    }

    /// 主线用例：双客户端互发——B 收到消息、A 收到确认。
    #[tokio::test]
    async fn two_clients_exchange_messages() {
        let addr = server().await;
        let mut alice = client(addr, 1, Duration::from_millis(50)).await;
        let mut bob = client(addr, 2, Duration::from_millis(50)).await;

        // 双方都握手成功
        assert!(matches!(
            next_event(&mut alice.events).await,
            ClientEvent::Connected { .. }
        ));
        assert!(matches!(
            next_event(&mut bob.events).await,
            ClientEvent::Connected { .. }
        ));

        // Alice → Bob
        alice
            .handle
            .send_msg(2, Bytes::from_static(b"hello from alice"))
            .await
            .unwrap();

        let bob_msg = match next_event(&mut bob.events).await {
            ClientEvent::Message(msg) => msg,
            other => panic!("Bob 应先收到消息，实际 {other:?}"),
        };
        assert_eq!(bob_msg.from, 1);
        assert_eq!(bob_msg.content, Bytes::from_static(b"hello from alice"));

        let ack = match next_event(&mut alice.events).await {
            ClientEvent::Ack { msg_id } => msg_id,
            other => panic!("Alice 应收到确认，实际 {other:?}"),
        };
        assert_eq!(ack, bob_msg.msg_id);

        // Bob → Alice（双向都要通）
        bob.handle
            .send_msg(1, Bytes::from_static(b"hi alice"))
            .await
            .unwrap();
        let alice_msg = match next_event(&mut alice.events).await {
            ClientEvent::Message(msg) => msg,
            other => panic!("Alice 应收到消息，实际 {other:?}"),
        };
        assert_eq!(alice_msg.from, 2);
        assert_eq!(alice_msg.content, Bytes::from_static(b"hi alice"));
    }

    /// 离线同步：B 离线时 A 发消息；B 上线后自动补齐（SyncBatch）。
    #[tokio::test]
    async fn offline_messages_sync_on_connect() {
        let addr = server().await;
        let mut alice = client(addr, 1, Duration::from_millis(50)).await;
        assert!(matches!(
            next_event(&mut alice.events).await,
            ClientEvent::Connected { .. }
        ));

        // B 未上线：消息进离线队列
        alice
            .handle
            .send_msg(2, Bytes::from_static(b"while you were away"))
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut alice.events).await,
            ClientEvent::Ack { .. }
        ));

        // B 上线：Connected 之后应收到 SyncBatch 补投
        let mut bob = client(addr, 2, Duration::from_millis(50)).await;
        assert!(matches!(
            next_event(&mut bob.events).await,
            ClientEvent::Connected { .. }
        ));
        let batch = match next_event(&mut bob.events).await {
            ClientEvent::SyncBatch(messages) => messages,
            other => panic!("Bob 应收到离线补投，实际 {other:?}"),
        };
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].content, Bytes::from_static(b"while you were away"));
        assert_eq!(batch[0].from, 1);
    }

    /// 闪断重连：服务端先拒绝/闪断两次，客户端退避后第三次连上。
    #[tokio::test]
    async fn reconnects_after_transient_failures() {
        // 「闪断」服务端：前 2 次 accept 后立刻断开，之后交给正式会话层
        let sessions = Sessions::new(SessionConfig {
            authenticator: std::sync::Arc::new(AllowAll),
            ..SessionConfig::default()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn({
            let sessions = sessions.clone();
            async move {
                let mut accepted = 0u32;
                loop {
                    let Ok((stream, _)) = listener.accept().await else { continue };
                    accepted += 1;
                    if accepted <= 2 {
                        drop(stream); // 模拟闪断：accept 后立即关闭
                    } else {
                        let sessions = sessions.clone();
                        let conn_id = sessions.next_conn_id();
                        let (_tx, rx) = shutdown_channel();
                        // move 进 owned clone：serve_connection 借用 sessions，
                        // 而 tokio::spawn 要求 Future 满足 'static
                        tokio::spawn(async move {
                            let _ = im_server::serve_connection(
                                &sessions,
                                conn_id,
                                stream,
                                GatewayConfig::default(),
                                rx,
                            )
                            .await;
                        });
                    }
                }
            }
        });

        // 退避 base 20ms：测试不被默认 1s 拖慢
        let mut bob = client(addr, 2, Duration::from_millis(20)).await;

        // 事件序列：两次断开后第三次握手成功
        let mut disconnects = 0;
        let connected = loop {
            match next_event(&mut bob.events).await {
                ClientEvent::Disconnected => disconnects += 1,
                ClientEvent::Connected { session_id } => break session_id,
                other => panic!("不应出现 {other:?}"),
            }
        };
        assert!(disconnects >= 1, "闪断至少触发一次 Disconnected");
        assert_ne!(connected, 0);

        // 重连成功后收发仍然可用
        bob.handle
            .send_msg(1, Bytes::from_static(b"back online"))
            .await
            .unwrap();
        let ack = match next_event(&mut bob.events).await {
            ClientEvent::Ack { msg_id } => msg_id,
            other => panic!("重连后应能发消息，实际 {other:?}"),
        };
        assert_ne!(ack, 0);
    }

    /// 握手被拒（服务端用 StaticToken，客户端给错 token）：不重连，事件终止。
    #[tokio::test]
    async fn rejected_handshake_stops_reconnecting() {
        let config = SessionConfig {
            authenticator: std::sync::Arc::new(im_server::StaticToken {
                token: "right".to_string(),
            }),
            ..SessionConfig::default()
        };
        let (addr, _sessions, _shutdown) =
            im_server::spawn_server(config).await.expect("服务应能启动");

        let mut intruder = client(addr, 1, Duration::from_millis(20)).await;
        // ClientConfig::new 的 token 是 "any"——必然被拒
        match next_event(&mut intruder.events).await {
            ClientEvent::Rejected { reason } => {
                assert!(reason.contains("credentials"), "原因应可读: {reason}");
            }
            other => panic!("应收到 Rejected，实际 {other:?}"),
        }

        // 拒绝后客户端退出：不再有事件（包括 Disconnected）
        assert!(
            timeout(Duration::from_millis(300), intruder.events.recv())
                .await
                .is_err(),
            "被拒后不应继续重连"
        );
    }

    /// 断线期间的消息命令在重连后被发出（命令排队即断线缓冲）。
    #[tokio::test]
    async fn queued_msg_is_sent_after_reconnect() {
        // 闪断一次的服务端
        let sessions = Sessions::new(SessionConfig {
            authenticator: std::sync::Arc::new(AllowAll),
            ..SessionConfig::default()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn({
            let sessions = sessions.clone();
            async move {
                let mut accepted = 0u32;
                loop {
                    let Ok((stream, _)) = listener.accept().await else { continue };
                    accepted += 1;
                    if accepted == 1 {
                        drop(stream); // 首次连接闪断
                    } else {
                        let sessions = sessions.clone();
                        let conn_id = sessions.next_conn_id();
                        let (_tx, rx) = shutdown_channel();
                        // 同上：move 进 owned clone 满足 'static
                        tokio::spawn(async move {
                            let _ = im_server::serve_connection(
                                &sessions,
                                conn_id,
                                stream,
                                GatewayConfig::default(),
                                rx,
                            )
                            .await;
                        });
                    }
                }
            }
        });

        // 先起一个在线的 Alice 接收
        let mut alice = client(addr, 1, Duration::from_millis(50)).await;
        assert!(matches!(
            next_event(&mut alice.events).await,
            ClientEvent::Connected { .. }
        ));

        let mut bob = client(addr, 2, Duration::from_millis(20)).await;
        // 第一轮：闪断（Disconnected）
        match next_event(&mut bob.events).await {
            ClientEvent::Disconnected => {}
            other => panic!("首个事件应是断线，实际 {other:?}"),
        }
        // 断线期间排队一条消息
        bob.handle
            .send_msg(1, Bytes::from_static(b"queued while offline"))
            .await
            .unwrap();

        // 重连成功
        assert!(matches!(
            next_event(&mut bob.events).await,
            ClientEvent::Connected { .. }
        ));

        // Alice 应收到排队的消息；Bob 收到它的 Ack
        let msg = match next_event(&mut alice.events).await {
            ClientEvent::Message(msg) => msg,
            other => panic!("Alice 应收到排队的消息，实际 {other:?}"),
        };
        assert_eq!(msg.content, Bytes::from_static(b"queued while offline"));
        assert!(matches!(
            next_event(&mut bob.events).await,
            ClientEvent::Ack { .. }
        ));
    }
}
