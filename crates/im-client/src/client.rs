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
//!   重连成功后由新一轮连接循环统一发出；
//! - **消息级重传（阶段 4）**：所有上行消息先入 [`Outbox`](crate::outbox)
//!   （本地持久化），Ack 核销前按指数退避重发——「至少一次」发送，
//!   配合接收端按 `client_msg_id` 去重拼出「恰好一次」。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytes::Bytes;
use im_protocol::{Handshake, HandshakeAck, Msg, MsgAck, Payload, SyncReq, SyncResp};
use im_storage::{LocalStore, StorageError};
use im_transport::{
    shutdown_channel, spawn_gateway, Backoff, ConnectionHandle, GatewayConfig, HeartbeatPolicy,
    InboundFrame, ShutdownRx,
};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

use crate::dedup::{DedupWindow, DEFAULT_CAPACITY as DEFAULT_DEDUP_CAPACITY};
use crate::outbox::Outbox;

/// 默认心跳间隔（经验值：明显小于服务端 60s 空闲超时）。
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// 默认重连基准间隔（full jitter 会在此基础上打散）。
pub const DEFAULT_BACKOFF_BASE: Duration = Duration::from_secs(1);
/// 默认重连封顶间隔。
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// 握手应答等待上限：超时按连接故障处理（触发重连）。
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// 消息级重传的首次等待（RTO）：Ack 未到则重发。
pub const DEFAULT_RETRY_TIMEOUT: Duration = Duration::from_secs(3);
/// 单条消息最大发送次数（含首次）：超过即放弃并上报 `SendFailed`。
pub const DEFAULT_RETRY_MAX_ATTEMPTS: u32 = 8;

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
    /// 本地消息库目录（聊天历史/重发表/游标）。
    ///
    /// `None` 时退到进程唯一的临时目录：适合测试与体验，
    /// 但重启换目录等于丢历史——长期使用的客户端应显式配置。
    pub data_dir: Option<PathBuf>,
    /// 消息级重传的首次等待（RTO）。
    pub retry_timeout: Duration,
    /// 单条消息最大发送次数（含首次）。
    pub retry_max_attempts: u32,
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
            data_dir: None,
            retry_timeout: DEFAULT_RETRY_TIMEOUT,
            retry_max_attempts: DEFAULT_RETRY_MAX_ATTEMPTS,
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
    /// 一条上行消息已入重发表（分配了 `client_msg_id`，尚未确认）。
    ///
    /// UI 据此立刻显示「发送中」的转圈条目；后续由
    /// [`ClientEvent::Ack`]（送达）或 [`ClientEvent::SendFailed`]（放弃）收尾。
    MessageQueued {
        /// 刚分配的去重键。
        client_msg_id: u64,
        /// 接收者。
        to: u64,
        /// 消息内容。
        content: Bytes,
    },
    /// 一条上行消息被服务端确认。
    ///
    /// `client_msg_id` 用于消除本地「发送中」标记（跨重发稳定）；
    /// `msg_id` 是服务端裁决的全局 ID（排序/同步游标用）。
    Ack {
        /// 服务端分配的全局消息 ID。
        msg_id: u64,
        /// 发送方本地生成的去重键。
        client_msg_id: u64,
    },
    /// 一批离线消息到达（连接建立后自动拉取）。
    SyncBatch(Vec<Msg>),
    /// 握手被服务端拒绝：**不再重连**（重试没有意义——换 token 或账号）。
    Rejected {
        /// 服务端给出的拒绝原因。
        reason: String,
    },
    /// 一条上行消息重传次数耗尽仍未被确认：放弃（业务层可提示发送失败）。
    ///
    /// 「至少一次」不等于「无限重」——无限重发会拖垮客户端与服务端；
    /// 达到上限后把决定权交回业务层（重发/放弃/提示）。
    SendFailed {
        /// 被放弃消息的去重键（业务层用它找到 UI 上的「转圈」条目）。
        client_msg_id: u64,
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
#[derive(Debug)]
enum Outcome {
    /// 连接结束（网络故障/对端关闭/握手超时）：值得重连。
    Disconnected,
    /// 握手被拒：重连无意义，停止。
    Rejected(String),
    /// 外部关停：正常退出。
    Stopped,
}

/// 客户端本地持久化状态：消息库 + 重发邮箱 + 去重窗口 + 同步游标。
///
/// 四者同源（同一目录、同一生命周期），且消息循环对它们的访问总是
/// 交织的（入库要查去重、发送要写重发表）——打包成一个对象传递，
/// 「重启恢复 = `LocalState::open` 一件事」也因此成立。
struct LocalState {
    store: LocalStore,
    outbox: Outbox,
    dedup: DedupWindow,
    /// 同步游标：已落盘的最大 `msg_id`（重启不回退，重连只补增量）。
    cursor: u64,
}

impl LocalState {
    /// 打开本地状态：重载重发表与游标（去重窗口从空开始——
    /// 游标保证旧消息不会被重新拉取，窗口只需覆盖重传的时效）。
    fn open(
        dir: &Path,
        retry_timeout: Duration,
        retry_max_attempts: u32,
    ) -> Result<Self, StorageError> {
        let mut store = LocalStore::open(dir)?;
        let outbox = Outbox::load(&mut store, retry_timeout, retry_max_attempts)?;
        let cursor = store.sync_cursor()?;
        Ok(Self {
            store,
            outbox,
            dedup: DedupWindow::new(DEFAULT_DEDUP_CAPACITY),
            cursor,
        })
    }

    /// 入一条收到的消息：去重 → 落盘 → 游标推进。
    /// `Some(msg)` = 新消息（可上抛 UI）；`None` = 重复，丢弃。
    ///
    /// 先落盘再上抛：崩溃时宁可重收（去重窗口兜底）不可丢。
    fn ingest(&mut self, msg: Msg) -> Result<Option<Msg>, StorageError> {
        if !self.dedup.admit((msg.from, msg.client_msg_id)) {
            return Ok(None);
        }
        self.store.append_incoming(&msg)?;
        if msg.msg_id > self.cursor {
            self.cursor = msg.msg_id;
            self.store.set_sync_cursor(self.cursor)?;
        }
        Ok(Some(msg))
    }

    /// 批量入库（离线补投）。返回新消息子集。
    fn ingest_batch(&mut self, messages: Vec<Msg>) -> Result<Vec<Msg>, StorageError> {
        let mut fresh = Vec::with_capacity(messages.len());
        for msg in messages {
            if let Some(msg) = self.ingest(msg)? {
                fresh.push(msg);
            }
        }
        Ok(fresh)
    }
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

    // 本地持久化状态：消息库/重发表/去重窗口/游标。
    // 未配置目录时退到进程唯一临时目录（测试/体验；重启丢历史）
    let dir = config.data_dir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!(
            "im-client-{}-{}-{}",
            config.user_id,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("系统时钟正常")
                .as_nanos(),
        ))
    });
    let mut local =
        LocalState::open(&dir, config.retry_timeout, config.retry_max_attempts)
            .expect("本地消息库应能打开");

    loop {
        let outcome =
            connect_once(&config, &events, &mut cmd_rx, &shutdown, &mut local).await;

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
/// 任何失败路径都先等网关收尾——本函数返回时这轮连接的资源确定已回收。
async fn connect_once(
    config: &ClientConfig,
    events: &mpsc::Sender<ClientEvent>,
    cmd_rx: &mut mpsc::Receiver<ClientCommand>,
    shutdown: &ShutdownRx,
    local: &mut LocalState,
) -> Outcome {
    // 本轮连接的关停信号：外层 shutdown 或本轮结束时触发，停掉网关
    let (local_shutdown_tx, local_shutdown_rx) = shutdown_channel();
    let outer_shutdown = shutdown.clone();

    // 1. TCP 连接失败：按普通断线处理（外层退避重试）
    let Ok(stream) = TcpStream::connect(&config.server_addr).await else {
        return Outcome::Disconnected;
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
        return Outcome::Disconnected;
    }

    // 4. 等握手应答（区分「被拒」与「网络故障」——语义不同，处理不同）。
    //    超时、通道关闭（网关先退）、应答解码失败一律按断线处理
    let Ok(Some(event)) = timeout(config.handshake_timeout, frame_rx.recv()).await else {
        finish_gateway(local_shutdown_tx, gateway_task).await;
        return Outcome::Disconnected;
    };
    let Ok(ack) = HandshakeAck::decode_frame(&event.frame) else {
        finish_gateway(local_shutdown_tx, gateway_task).await;
        return Outcome::Disconnected;
    };
    if !ack.is_accepted() {
        finish_gateway(local_shutdown_tx, gateway_task).await;
        return Outcome::Rejected(ack.reason);
    }
    let session_id = ack.session_id;
    if events
        .send(ClientEvent::Connected { session_id })
        .await
        .is_err()
    {
        // 业务层不在了：客户端没有存在意义
        finish_gateway(local_shutdown_tx, gateway_task).await;
        return Outcome::Stopped;
    }

    // 5. 离线同步：从本地游标补齐断线期间的消息（重启后从磁盘恢复）
    send_seq += 1;
    let sync_req = SyncReq { since: local.cursor };
    if handle.send(sync_req.encode_frame(send_seq, 0)).await.is_err() {
        finish_gateway(local_shutdown_tx, gateway_task).await;
        return Outcome::Disconnected;
    }

    // 6. 消息循环：命令、入站帧、重传定时三路 select
    let outcome = message_loop(
        events,
        cmd_rx,
        outer_shutdown,
        &handle,
        &mut frame_rx,
        &mut send_seq,
        local,
        config.user_id,
    )
    .await;

    finish_gateway(local_shutdown_tx, gateway_task).await;
    outcome
}

/// 消息循环：命令、入站帧、重传定时三路 `select`，直到本轮连接结束。
///
/// 退出路径与结局的对应：外部关停/命令通道关闭 → `Stopped`；
/// 发送失败/网关退出 → `Disconnected`；业务层事件通道关闭 → `Stopped`
/// （客户端没有存在意义）；本地磁盘故障 → `Stopped`
/// （所有可靠性承诺已失效，继续运行是自欺）。
///
/// 参数里同时有命令流、帧流、本地状态三类——它们本来就是同一轮
/// 连接的「三个面」，拆再细也只是搬家（`too_many_arguments` 在此豁免）。
#[allow(clippy::too_many_arguments)]
async fn message_loop(
    events: &mpsc::Sender<ClientEvent>,
    cmd_rx: &mut mpsc::Receiver<ClientCommand>,
    mut outer_shutdown: ShutdownRx,
    handle: &ConnectionHandle,
    frame_rx: &mut mpsc::Receiver<InboundFrame>,
    send_seq: &mut u64,
    local: &mut LocalState,
    self_id: u64,
) -> Outcome {
    // 连接建立即补发：重发表里所有未确认消息发一遍（接收端按
    // `client_msg_id` 去重，重复投递无害）——新连接是一次「全量追赶」，
    // 不依赖旧连接的 RTO 状态
    let resends = local.outbox.resends();
    if let Some(outcome) = send_batch(handle, send_seq, resends).await {
        return outcome;
    }

    loop {
        tokio::select! {
            // 外部关停：退出（网关在 finish_gateway 里收尾）
            () = outer_shutdown.wait() => break Outcome::Stopped,

            cmd = cmd_rx.recv() => match cmd {
                Some(ClientCommand::SendMsg { to, content }) => {
                    // 先入重发表（持久化 + 分配 client_msg_id）再上线：
                    // Ack 之前它一直是「未确认」，断线/超时都会被重发。
                    // store 与 outbox 是 LocalState 的不同字段，借用互不干扰
                    let msg = match local.outbox.enqueue(&mut local.store, to, content) {
                        Ok(msg) => msg,
                        Err(_) => break Outcome::Stopped, // 磁盘故障
                    };
                    // 上抛「发送中」：UI 立刻显示转圈条目
                    // （在 Ack 之前发出，顺序由同一通道保证）
                    if events
                        .send(ClientEvent::MessageQueued {
                            client_msg_id: msg.client_msg_id,
                            to: msg.to,
                            content: msg.content.clone(),
                        })
                        .await
                        .is_err()
                    {
                        break Outcome::Stopped;
                    }
                    *send_seq += 1;
                    // 发送失败 = 本轮连接已死：交给断线路径
                    if handle.send(msg.encode_frame(*send_seq, 0)).await.is_err() {
                        break Outcome::Disconnected;
                    }
                }
                None => break Outcome::Stopped, // 业务层放手：正常退出
            },

            // 重传定时：最早到期的未确认消息（无在途时永久挂起）
            () = async {
                match local.outbox.next_deadline() {
                    Some(deadline) => tokio::time::sleep_until(
                        tokio::time::Instant::from_std(deadline),
                    ).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let Ok((resends, failed)) =
                    local.outbox.due(&mut local.store, Instant::now())
                else {
                    break Outcome::Stopped; // 磁盘故障
                };
                // 先上报放弃的，再补发到期的（两者互不影响）
                let mut events_dead = false;
                for client_msg_id in failed {
                    if events
                        .send(ClientEvent::SendFailed { client_msg_id })
                        .await
                        .is_err()
                    {
                        events_dead = true;
                        break;
                    }
                }
                if events_dead {
                    break Outcome::Stopped;
                }
                if let Some(outcome) = send_batch(handle, send_seq, resends).await {
                    break outcome;
                }
            }

            frame = frame_rx.recv() => match frame {
                Some(event) => match event.frame.cmd {
                    im_protocol::Cmd::Msg => {
                        if let Ok(msg) = Msg::decode_frame(&event.frame) {
                            match local.ingest(msg) {
                                // 去重 → 落盘 → 游标推进后才上抛 UI
                                Ok(Some(msg)) => {
                                    if events.send(ClientEvent::Message(msg)).await.is_err() {
                                        break Outcome::Stopped;
                                    }
                                }
                                Ok(None) => {} // 重复投递：静默丢弃
                                Err(_) => break Outcome::Stopped, // 磁盘故障
                            }
                        }
                    }
                    im_protocol::Cmd::MsgAck => {
                        if let Ok(ack) = MsgAck::decode_frame(&event.frame) {
                            // 核销重发表：pending → 正式消息。
                            // 跨重发稳定的是 client_msg_id（重发会换新 msg_id）
                            if local
                                .outbox
                                .ack(&mut local.store, ack.client_msg_id, ack.msg_id, self_id)
                                .is_err()
                            {
                                break Outcome::Stopped; // 磁盘故障
                            }
                            if events
                                .send(ClientEvent::Ack {
                                    msg_id: ack.msg_id,
                                    client_msg_id: ack.client_msg_id,
                                })
                                .await
                                .is_err()
                            {
                                break Outcome::Stopped;
                            }
                        }
                    }
                    im_protocol::Cmd::SyncResp => {
                        if let Ok(resp) = SyncResp::decode_frame(&event.frame) {
                            // 逐条去重 + 入库，只上抛「新」消息子集；
                            // 全为重复（或空批）时不上抛——连接层的
                            // 例行同步噪音不出协议层
                            match local.ingest_batch(resp.messages) {
                                Ok(fresh) if !fresh.is_empty() => {
                                    if events
                                        .send(ClientEvent::SyncBatch(fresh))
                                        .await
                                        .is_err()
                                    {
                                        break Outcome::Stopped;
                                    }
                                }
                                Ok(_) => {}
                                Err(_) => break Outcome::Stopped, // 磁盘故障
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
    }
}

/// 批量发送一批上行消息：任一发送失败即返回对应结局
/// （`None` = 全部成功）。连接刚建立时的补发与到期重传共用。
async fn send_batch(
    handle: &ConnectionHandle,
    send_seq: &mut u64,
    msgs: Vec<Msg>,
) -> Option<Outcome> {
    for msg in msgs {
        *send_seq += 1;
        if handle.send(msg.encode_frame(*send_seq, 0)).await.is_err() {
            return Some(Outcome::Disconnected);
        }
    }
    None
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
    /// 局部的 `shutdown_tx` 会被 drop——服务端会立即退场。泄漏这一份 sender
    /// 保活（`watch::Sender` 极小，测试进程内泄漏无害）。
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
        /// 本地库目录（持久化断言用）。
        data_dir: std::path::PathBuf,
        _shutdown: ShutdownTx,
    }

    async fn client(addr: SocketAddr, user_id: u64, backoff_base: Duration) -> TestClient {
        // 每个客户端独立临时目录：本地库互不串味（进程退出后由系统清理）
        let data_dir = std::env::temp_dir().join(format!(
            "im-client-test-{}-{}-{}",
            std::process::id(),
            user_id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("系统时钟正常")
                .as_nanos(),
        ));
        let config = ClientConfig {
            backoff_base,
            data_dir: Some(data_dir.clone()),
            // 短 RTO：断线重发的等待不拖慢测试
            retry_timeout: Duration::from_millis(100),
            ..ClientConfig::new(addr.to_string(), user_id, "any")
        };
        let (events_tx, events_rx) = mpsc::channel(64);
        let (shutdown_tx, shutdown_rx) = shutdown_channel();
        let handle = run_client(config, events_tx, shutdown_rx).await;
        TestClient {
            handle,
            events: events_rx,
            data_dir,
            _shutdown: shutdown_tx,
        }
    }

    /// 等待下一个业务事件（断言在 WAIT 内到达）。
    ///
    /// 过滤两类「过程噪音」：
    /// - 每次连接后的例行空 `SyncBatch`（连接层噪音）；
    /// - `MessageQueued`（发送过程事件，专门的测试覆盖它的顺序）。
    /// 非空批量（离线补投）依然原样上递。
    async fn next_event(events: &mut mpsc::Receiver<ClientEvent>) -> ClientEvent {
        loop {
            let event = timeout(WAIT, events.recv())
                .await
                .expect("2s 内应收到事件")
                .expect("客户端存活");
            match event {
                ClientEvent::SyncBatch(ref batch) if batch.is_empty() => {}
                ClientEvent::MessageQueued { .. } => {}
                other => return other,
            }
        }
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
            ClientEvent::Ack { msg_id, .. } => msg_id,
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
                        let (shutdown_tx, rx) = shutdown_channel();
                        // move 进 owned clone：serve_connection 借用 sessions，
                        // 而 tokio::spawn 要求 Future 满足 'static。
                        // shutdown_tx 也必须 move 进去保活——「sender 全部
                        // drop = 视为已关停」的语义下，留在外层会立即杀死连接
                        tokio::spawn(async move {
                            let _keep_alive = shutdown_tx;
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
            ClientEvent::Ack { msg_id, .. } => msg_id,
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

        // 拒绝后客户端退出：不再有任何事件。两种「安静」都合法——
        // 超时（客户端已退出但通道未关）或 `None`（client_loop 结束时 drop
        // 了发送端，recv 立即返回 None）。真正的失败是又收到新事件。
        match timeout(Duration::from_millis(300), intruder.events.recv()).await {
            Err(_elapsed) => {}
            Ok(None) => {}
            Ok(Some(event)) => panic!("被拒后不应继续重连，实际收到 {event:?}"),
        }
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
                    // 第 1 个连接是 Alice（直连，必须活）；
                    // 第 2 个是 Bob 的首轮连接——闪断它，制造「断线排队」窗口
                    if accepted == 2 {
                        drop(stream);
                    } else {
                        let sessions = sessions.clone();
                        let conn_id = sessions.next_conn_id();
                        let (shutdown_tx, rx) = shutdown_channel();
                        // 同上：owned clone 满足 'static，shutdown_tx 保活进 task
                        tokio::spawn(async move {
                            let _keep_alive = shutdown_tx;
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

    /// 消息级重传：在途消息未被 Ack 时连接死亡，重连后由重发表补发。
    ///
    /// 用「拿到服务端连接的关闭开关」人为制造 Ack 丢失：消息确定到达
    /// 服务端（Alice 已收到）后杀死 Bob 的连接——Ack 是否来得及回不再
    /// 重要，两种结局都合法且都被断言覆盖（Ack 已到 = 已核销；
    /// Ack 丢失 = 重连补发后再核销）。
    #[tokio::test]
    async fn in_flight_msg_is_retransmitted_after_connection_death() {
        let sessions = Sessions::new(SessionConfig {
            authenticator: std::sync::Arc::new(AllowAll),
            ..SessionConfig::default()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // 第 2 条连接（Bob）的关闭开关交给测试，其余连接正常服务
        let (kill_tx, mut kill_rx) = mpsc::channel::<ShutdownTx>(1);
        tokio::spawn({
            let sessions = sessions.clone();
            async move {
                let mut accepted = 0u32;
                loop {
                    let Ok((stream, _)) = listener.accept().await else { continue };
                    accepted += 1;
                    let (shutdown_tx, rx) = shutdown_channel();
                    if accepted == 2 {
                        let _ = kill_tx.send(shutdown_tx.clone()).await;
                    }
                    let sessions = sessions.clone();
                    let conn_id = sessions.next_conn_id();
                    // owned clone 满足 'static；shutdown_tx move 进去保活
                    tokio::spawn(async move {
                        let _keep_alive = shutdown_tx;
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
        });

        let mut alice = client(addr, 1, Duration::from_millis(50)).await;
        assert!(matches!(
            next_event(&mut alice.events).await,
            ClientEvent::Connected { .. }
        ));
        let mut bob = client(addr, 2, Duration::from_millis(20)).await;
        assert!(matches!(
            next_event(&mut bob.events).await,
            ClientEvent::Connected { .. }
        ));

        // 拿到 Bob 首轮连接的关闭开关（服务端已 spawn 该连接）
        let kill = kill_rx.recv().await.expect("应拿到关闭开关");

        bob.handle
            .send_msg(1, Bytes::from_static(b"must arrive"))
            .await
            .unwrap();
        // Alice 收到 = 消息确定到达服务端；此刻杀连接，Ack 生死由天
        let msg = match next_event(&mut alice.events).await {
            ClientEvent::Message(msg) => msg,
            other => panic!("Alice 应收到消息，实际 {other:?}"),
        };
        assert_eq!(msg.content, Bytes::from_static(b"must arrive"));
        kill.trigger();

        // Bob 终将收到 Ack：要么断线前已到（已核销），
        // 要么重连补发后到（二次投递 + 二次 Ack）
        let mut acked = false;
        for _ in 0..10 {
            match next_event(&mut bob.events).await {
                ClientEvent::Ack { client_msg_id, .. } => {
                    assert_eq!(client_msg_id, 1, "Bob 的首条消息");
                    acked = true;
                    break;
                }
                ClientEvent::Disconnected
                | ClientEvent::Connected { .. }
                | ClientEvent::MessageQueued { .. } => {}
                other => panic!("Bob 不应收到 {other:?}"),
            }
        }
        assert!(acked, "重连补发后应收到 Ack");
    }

    /// 事件顺序：发送后先 `MessageQueued`（UI 转圈）后 `Ack`（送达），
    /// 同一通道保证顺序——UI 状态机依赖这个不变量。
    #[tokio::test]
    async fn message_queued_precedes_ack() {
        let addr = server().await;
        let mut alice = client(addr, 1, Duration::from_millis(50)).await;
        assert!(matches!(
            next_event(&mut alice.events).await,
            ClientEvent::Connected { .. }
        ));

        alice
            .handle
            .send_msg(2, Bytes::from_static(b"spin then tick"))
            .await
            .unwrap();
        // 不经过 next_event 的过滤：原始事件序列必须先是 MessageQueued
        match timeout(WAIT, alice.events.recv()).await {
            Ok(Some(ClientEvent::MessageQueued { client_msg_id, to, .. })) => {
                assert_eq!(client_msg_id, 1);
                assert_eq!(to, 2);
            }
            other => panic!("第一事件应是 MessageQueued，实际 {other:?}"),
        }
        assert!(matches!(
            next_event(&mut alice.events).await,
            ClientEvent::Ack { .. }
        ));
    }

    /// 收到的消息落盘：直接重开本地库验证（绕过客户端，防自说自话），
    /// 游标同时推进（重启后只补增量）。
    #[tokio::test]
    async fn received_messages_are_persisted() {
        let addr = server().await;
        let mut alice = client(addr, 1, Duration::from_millis(50)).await;
        let mut bob = client(addr, 2, Duration::from_millis(50)).await;
        assert!(matches!(
            next_event(&mut alice.events).await,
            ClientEvent::Connected { .. }
        ));
        assert!(matches!(
            next_event(&mut bob.events).await,
            ClientEvent::Connected { .. }
        ));

        bob.handle
            .send_msg(1, Bytes::from_static(b"persisted please"))
            .await
            .unwrap();
        let msg = match next_event(&mut alice.events).await {
            ClientEvent::Message(msg) => msg,
            other => panic!("Alice 应收到消息，实际 {other:?}"),
        };
        assert_eq!(msg.content, Bytes::from_static(b"persisted please"));

        // 直接开 Alice 的本地库：消息在历史里，游标已推进
        let mut store = im_storage::LocalStore::open(&alice.data_dir).unwrap();
        let history = store.history(2, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].msg_id, msg.msg_id);
        assert_eq!(history[0].content, Bytes::from_static(b"persisted please"));
        assert!(store.sync_cursor().unwrap() >= msg.msg_id);
    }
}
