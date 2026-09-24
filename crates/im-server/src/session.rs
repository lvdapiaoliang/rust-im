//! 会话层：把网关递上来的「帧」变成 IM 语义——登录、路由、离线、同步。
//!
//! 这是阶段 3 的**总装车间**：前面造好的零件在这里合体。
//!
//! ```text
//!        ┌────────────────────────────────────────────────────┐
//!        │                    Sessions（共享）                  │
//!        │  Router<SessionHandle>   Mutex<Snowflake>           │
//!        │  （user → 连接，分片）    （msg_id/session_id）      │
//!        │  Mutex<HashMap<u64, VecDeque<Msg>>>                 │
//!        │  （离线暂存，阶段 4 换 im-storage 持久化）            │
//!        └────▲──────────────────▲──────────────────▲────────┘
//!             │ register/get     │ next_id          │ sync_since
//!        ┌────┴────┐        ┌────┴────┐        ┌────┴────┐
//!        │ 会话task │        │ 会话task │        │ 会话task │   ← 每连接一组
//!        │(serve_  │        │         │        │         │
//!        │connection)      └─────────┘        └─────────┘
//!        │  ▲ frame_rx（本地通道）              ▲
//!        │  └── run_gateway_connection（读循环/写actor/心跳）
//!        └──────────────────────────────────────────────▶ TCP
//! ```
//!
//! # 为什么没有「中央消息总线 task」
//!
//! 路由的本质是**查表**（`user_id → 连接`），而查表已被 `Router`
//! 分片并发化——任何会话 task 都能就地路由，无需排队经过中央 task。
//! 中央总线在「扇出/顺序性保证」时才有价值（阶段 6 群聊再评估）。
//!
//! # 模式落点
//!
//! - Actor 模式（复用网关的写 actor）+ **每连接一个会话 task**：
//!   连接的生命周期与会话状态（认证身份、发送序号、去重窗口）
//!   绑定在同一个 task 里——状态零共享，收尾（注销路由）天然不竞态；
//! - 值校验注销：旧连接收尾只注销自己的注册（`conn_id` 比对）；
//! - 投递降级：在线投递失败自动降级为离线入队——**消息不丢**优先于
//!   「连接状态刚好新鲜」；
//! - 依赖注入：[`Authenticator`] trait 让认证策略可替换（阶段 7 上真
//!   挑战-应答时只换实现，会话层不动）。

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use im_protocol::{Handshake, HandshakeAck, Msg, MsgAck, Payload, SyncReq, SyncResp};
use im_transport::{
    run_gateway_connection, shutdown_channel, ConnectionHandle, DedupWindow, GatewayConfig,
    InboundFrame, ShutdownRx, ShutdownTx, TransportError, Verdict,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::router::{Router, RouterError};
use crate::snowflake::{Snowflake, SnowflakeError, SystemClock};

/// 每用户离线消息上限（默认值）：超出丢最老的（内存保护的取舍）。
pub const DEFAULT_MAX_OFFLINE_PER_USER: usize = 1024;
/// 单次同步批量（默认值）：一批一帧，客户端分页拉取。
pub const DEFAULT_SYNC_BATCH: usize = 100;
/// 会话入站通道容量：网关 → 会话 task（反压点）。
const SESSION_CHANNEL_CAPACITY: usize = 16;
/// 雪花序列耗尽时的重试次数（每次等 1ms 换新毫秒）。
const ID_RETRY_ATTEMPTS: u32 = 3;

// ────────────────────────────────────────────────────────────────
// 认证
// ────────────────────────────────────────────────────────────────

/// 认证策略：校验「user_id + token 是否匹配」。
///
/// 阶段 3 是明文比对；阶段 7 升级为挑战-应答时**只换实现**，
/// 会话层的代码一行不改——这就是依赖注入买来的可替换性。
pub trait Authenticator: Send + Sync {
    /// 返回 `true` 表示认证通过。
    fn authenticate(&self, user_id: u64, token: &str) -> bool;
}

/// 静态口令：所有用户共用一个 token（演示与测试用）。
#[derive(Debug, Clone)]
pub struct StaticToken {
    /// 共享口令。
    pub token: String,
}

impl Authenticator for StaticToken {
    fn authenticate(&self, _user_id: u64, token: &str) -> bool {
        self.token == token
    }
}

/// 全放行：仅限本地开发与测试（生产环境是安全事故）。
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl Authenticator for AllowAll {
    fn authenticate(&self, _user_id: u64, _token: &str) -> bool {
        true
    }
}

// ────────────────────────────────────────────────────────────────
// 配置与会话中心
// ────────────────────────────────────────────────────────────────

/// 会话层配置。
#[derive(Clone)]
pub struct SessionConfig {
    /// 路由表分片数（内部取整到 2 的幂）。
    pub shard_count: usize,
    /// 雪花机器 ID（多实例部署时必须互不相同）。
    pub machine_id: u64,
    /// 每用户离线消息上限。
    pub max_offline_per_user: usize,
    /// 单次同步批量。
    pub sync_batch_size: usize,
    /// 认证策略。
    pub authenticator: Arc<dyn Authenticator>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            shard_count: 64,
            machine_id: 1,
            max_offline_per_user: DEFAULT_MAX_OFFLINE_PER_USER,
            sync_batch_size: DEFAULT_SYNC_BATCH,
            authenticator: Arc::new(StaticToken {
                token: "demo".to_string(),
            }),
        }
    }
}

/// 路由表里的值：连接句柄 + 连接唯一 ID + 下行序号。
///
/// - `conn_id`：注销时的**身份凭据**——收尾逻辑用它做谓词校验
///   （见 [`Router::remove_if`]），防止旧连接误删新连接的注册；
/// - `send_seq`：服务端 → 该连接的下行帧序号。原子计数器放在这里，
///   任何 task 投递消息时 `fetch_add` 都不冲突（无锁分配序号）。
#[derive(Debug, Clone)]
pub struct SessionHandle {
    /// 连接唯一 ID（`Sessions` 分配，进程内递增）。
    pub conn_id: u64,
    /// 下行帧序号分配器。
    pub send_seq: Arc<AtomicU64>,
    /// 回话句柄。
    pub handle: ConnectionHandle,
}

/// 会话中心：路由表 + 雪花 ID + 离线暂存（共享状态，`Arc` 克隆）。
///
/// 所有锁的纪律与 [`crate::router`] 相同：**微秒级纯内存临界区，
/// 不跨 `await` 持锁**。
#[derive(Clone)]
pub struct Sessions {
    inner: Arc<Inner>,
}

struct Inner {
    config: SessionConfig,
    router: Router<SessionHandle>,
    snowflake: Mutex<Snowflake<SystemClock>>,
    offline: Mutex<HashMap<u64, VecDeque<Msg>>>,
    /// 连接 ID 分配器（雪花之外的轻量序号：重启清零也无妨，
    /// 它只在一轮服务进程内做身份区分）。
    conn_seq: AtomicU64,
}

impl Sessions {
    /// 创建会话中心。
    ///
    /// # Panics
    ///
    /// `machine_id > 1023` 时 panic（雪花位段装不下，部署期配置错误）。
    #[must_use]
    pub fn new(config: SessionConfig) -> Self {
        let snowflake = Snowflake::new(config.machine_id, Arc::new(SystemClock));
        let shard_count = config.shard_count;
        Self {
            inner: Arc::new(Inner {
                router: Router::new(shard_count),
                snowflake: Mutex::new(snowflake),
                offline: Mutex::new(HashMap::new()),
                conn_seq: AtomicU64::new(0),
                config,
            }),
        }
    }

    /// 当前配置（诊断与测试）。
    #[must_use]
    pub fn config(&self) -> &SessionConfig {
        &self.inner.config
    }

    /// 分配连接 ID。
    #[must_use]
    pub fn next_conn_id(&self) -> u64 {
        self.inner.conn_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 生成全局 ID（`msg_id` / `session_id`），序列耗尽时等下一毫秒重试。
    ///
    /// 时钟回拨不可恢复（拒绝发号），返回 `None`——调用方应放弃本次
    /// 操作并让客户端超时重试。
    async fn next_id(&self) -> Option<u64> {
        for _ in 0..ID_RETRY_ATTEMPTS {
            // guard 在块内结束：sleep 跨 await 时不持锁
            let verdict = self
                .inner
                .snowflake
                .lock()
                .expect("雪花锁中毒")
                .next_id();
            match verdict {
                Ok(id) => return Some(id),
                Err(SnowflakeError::SequenceExhausted) => {
                    sleep(Duration::from_millis(1)).await; // 换个毫秒再来
                }
                Err(SnowflakeError::ClockMovedBackwards { .. }) => return None,
            }
        }
        None
    }

    /// 注册上线（握手成功后调用）。
    ///
    /// # Errors
    ///
    /// 该用户已在线时返回 [`RouterError::AlreadyOnline`]。
    pub fn register(
        &self,
        user_id: u64,
        conn_id: u64,
        handle: ConnectionHandle,
    ) -> Result<(), RouterError> {
        let session_handle = SessionHandle {
            conn_id,
            send_seq: Arc::new(AtomicU64::new(0)),
            handle,
        };
        self.inner.router.register(user_id, session_handle)
    }

    /// 注销下线（连接收尾时调用）：值校验——只有正确的 `conn_id` 才摘得掉。
    ///
    /// 返回是否真的移除（`false` = 已被顶替或不存在，不动是对的）。
    pub fn unregister(&self, user_id: u64, conn_id: u64) -> bool {
        // 谓词版注销：避免为了值校验去构造占位句柄
        self.inner.router.remove_if(user_id, |session| session.conn_id == conn_id)
    }

    /// 在线人数（诊断指标）。
    #[must_use]
    pub fn online_count(&self) -> usize {
        self.inner.router.len()
    }

    /// 某用户的离线消息数（诊断指标与测试）。
    #[must_use]
    pub fn offline_count(&self, user_id: u64) -> usize {
        self.inner
            .offline
            .lock()
            .expect("离线表锁中毒")
            .get(&user_id)
            .map_or(0, VecDeque::len)
    }

    /// 投递一条消息：在线则转发到接收者连接，否则（离线或投递失败）
    /// 降级进离线队列。
    ///
    /// **投递失败降级**：在线路由查到了、但 `send` 失败——说明接收者
    /// 的连接正在死亡（写 actor 已退役），此刻路由项还没被收尾逻辑
    /// 摘掉。消息进离线队列，等接收者重连后 `SyncReq` 补投——
    /// 「消息不丢」优先于「状态新鲜」。
    pub async fn deliver(&self, msg: &Msg) {
        if let Some(session) = self.inner.router.get(msg.to) {
            // 无锁分配下行序号：多个发送方同时投递也不冲突
            let seq = session.send_seq.fetch_add(1, Ordering::Relaxed) + 1;
            if session.handle.send(msg.encode_frame(seq, 0)).await.is_ok() {
                return; // 在线送达
            }
        }
        self.store_offline(msg.clone());
    }

    /// 离线入队：超出上限丢最老的（`VecDeque` 头部 O(1)）。
    fn store_offline(&self, msg: Msg) {
        let max = self.inner.config.max_offline_per_user;
        let mut offline = self.inner.offline.lock().expect("离线表锁中毒");
        let queue = offline.entry(msg.to).or_default();
        if queue.len() >= max {
            queue.pop_front();
        }
        queue.push_back(msg);
    }

    /// 拉取并移除 `user_id` 的离线消息中 `msg_id > since` 的前 `batch` 条。
    ///
    /// 队列按入队序 = `msg_id` 升序（同一台机器的雪花 ID 单调），
    /// 所以「取走头部大于游标的元素」天然就是顺序分页。
    /// 已同步过的（`msg_id <= since`）顺手丢弃——游标之前的数据没有
    /// 保留价值（阶段 4 持久化后改为「送达确认游标」更严谨）。
    #[must_use]
    pub fn sync_since(&self, user_id: u64, since: u64, batch: usize) -> Vec<Msg> {
        let mut offline = self.inner.offline.lock().expect("离线表锁中毒");
        let Some(queue) = offline.get_mut(&user_id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Some(front) = queue.front() {
            if front.msg_id <= since {
                queue.pop_front(); // 游标之前：已同步过，丢弃
            } else if out.len() < batch {
                out.push(queue.pop_front().expect("front 刚检查过"));
            } else {
                break; // 本批装满
            }
        }
        out
    }
}

// ────────────────────────────────────────────────────────────────
// 每连接会话 task
// ────────────────────────────────────────────────────────────────

/// 一条连接上的会话状态（会话 task 独占，零共享）。
struct SessionState {
    /// 认证通过的用户 ID（`None` = 未登录）。
    user: Option<u64>,
    /// 服务端下行帧序号（下一个待用值 + 1，从 1 开始）。
    send_seq: u64,
    /// 上行业务帧 seq 去重窗口。
    ///
    /// 懒初始化：以客户端首帧的 seq 为基准——不依赖「客户端 seq 从 1
    /// 开始」的约定，乱序起点也能对齐（`Option` 状态机的小实战）。
    dedup: Option<DedupWindow>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            user: None,
            send_seq: 0,
            dedup: None,
        }
    }

    /// 喂入一帧的 seq，返回去重判定。
    fn feed_seq(&mut self, seq: u64) -> Verdict {
        if let Some(window) = self.dedup.as_mut() {
            return window.feed(seq);
        }
        // 首帧：以它为基准建窗，然后喂入（必为 InOrder）
        let mut window = DedupWindow::new(seq);
        let verdict = window.feed(seq);
        self.dedup = Some(window);
        verdict
    }

    /// 帧级累计确认：「ack 之前的 seq 我已收齐」。
    fn ack(&self) -> u64 {
        self.dedup.as_ref().map_or(0, DedupWindow::ack)
    }
}

/// 回一帧载荷：分配下行 seq，填帧级累计确认，送出。
async fn reply<T: Payload>(
    state: &mut SessionState,
    handle: &ConnectionHandle,
    payload: &T,
) -> Result<(), TransportError> {
    state.send_seq += 1;
    handle.send(payload.encode_frame(state.send_seq, state.ack())).await
}

/// 单条连接的会话 task：认证、路由、同步，以及连接死亡后的收尾。
///
/// 本函数是**会话生命周期**的唯一属主（网关是连接生命周期的属主）：
/// 它返回即会话终结、路由注销完成。
///
/// 优雅关闭的链式传导：会话 task 退出 → 本地 `frame_rx` drop →
/// 网关读循环的 `inbound.send` 失败 → 网关回收连接 → TCP 关闭。
/// 不需要显式「关连接」的信号——**通道的 drop 就是信号**。
///
/// # Errors
///
/// 返回网关的结束原因（连接为何终结）；会话层自身没有 IO 失败路径。
pub async fn serve_connection(
    sessions: &Sessions,
    conn_id: u64,
    stream: TcpStream,
    gateway_config: GatewayConfig,
    shutdown: ShutdownRx,
) -> Result<(), TransportError> {
    // 网关 → 会话 task 的本地通道（与外界无涉，容量即反压点）
    let (frame_tx, mut frame_rx) = mpsc::channel::<InboundFrame>(SESSION_CHANNEL_CAPACITY);
    let gateway =
        tokio::spawn(run_gateway_connection(stream, gateway_config, frame_tx, shutdown));

    let mut state = SessionState::new();

    // 业务帧循环：网关退出（连接死亡/关停）时 frame_rx 结束
    while let Some(event) = frame_rx.recv().await {
        // seq 去重：应用层重发/乱序在进入业务前就被挡下
        match state.feed_seq(event.frame.seq) {
            Verdict::Duplicate | Verdict::TooFar { .. } => continue, // 丢弃
            Verdict::InOrder | Verdict::OutOfOrder => {}               // 上递
        }
        handle_frame(sessions, &mut state, conn_id, event).await;
    }

    // ── 收尾：注销路由（带 conn_id 谓词校验，见 Sessions::unregister）
    if let Some(user_id) = state.user {
        sessions.unregister(user_id, conn_id);
    }

    // 连接生命周期的权威结论来自网关
    gateway
        .await
        .expect("网关 task 不应 panic")
}

/// 业务帧分发：握手 / 消息 / 同步三正餐，其余忽略。
///
/// 解码失败的帧**丢弃而非断连**：坏载荷无法威胁会话状态
/// （状态机不推进），恶意流充其量浪费一点 CPU——这是「容忍与隔离」
/// 对「严格断连」的取舍，阶段 7 引入限流后再收紧。
async fn handle_frame(
    sessions: &Sessions,
    state: &mut SessionState,
    conn_id: u64,
    event: InboundFrame,
) {
    let frame = event.frame;
    let handle = &event.handle;

    match frame.cmd {
        im_protocol::Cmd::Handshake => {
            handle_handshake(sessions, state, conn_id, handle, &frame).await
        }
        im_protocol::Cmd::Msg => handle_msg(sessions, state, handle, &frame).await,
        im_protocol::Cmd::SyncReq => handle_sync(sessions, state, handle, &frame).await,
        // Ping/Pong 由网关消化或心跳产生；未知命令字进不到这里
        // （解码层已拦截 `UnknownCommand`）
        _ => {}
    }
}

/// 握手：认证 → 注册路由 → 回 `HandshakeAck`。
async fn handle_handshake(
    sessions: &Sessions,
    state: &mut SessionState,
    conn_id: u64,
    handle: &ConnectionHandle,
    frame: &im_protocol::Frame,
) {
    let Ok(hs) = Handshake::decode_frame(frame) else {
        return; // 坏载荷：丢弃
    };

    // 重复握手：同一连接不允许二次登录（要换账号请重连）
    if state.user.is_some() {
        let ack = HandshakeAck::rejected("already authenticated");
        let _ = reply(state, handle, &ack).await;
        return;
    }

    let authenticator = sessions.config().authenticator.as_ref();
    if !authenticator.authenticate(hs.user_id, &hs.token) {
        let ack = HandshakeAck::rejected("bad credentials");
        let _ = reply(state, handle, &ack).await;
        return;
    }

    // 会话 ID 也由雪花分配（与 msg_id 同源，全局唯一）
    let Some(session_id) = sessions.next_id().await else {
        let ack = HandshakeAck::rejected("id generator unavailable");
        let _ = reply(state, handle, &ack).await;
        return;
    };

    let ack = match sessions.register(hs.user_id, conn_id, handle.clone()) {
        Err(_) => HandshakeAck::rejected("already online"), // 单端登录：顶不掉旧连接
        Ok(()) => {
            state.user = Some(hs.user_id);
            HandshakeAck::accepted(session_id)
        }
    };
    let _ = reply(state, handle, &ack).await;
}

/// 上行消息：覆盖发送者身份 → 分配全局 ID → 路由/离线 → 回 `MsgAck`。
async fn handle_msg(
    sessions: &Sessions,
    state: &mut SessionState,
    handle: &ConnectionHandle,
    frame: &im_protocol::Frame,
) {
    // 未登录先说话：丢弃（不回执，客户端超时重发后自然回到正轨）
    let Some(from) = state.user else {
        return;
    };
    let Ok(upstream) = Msg::decode_frame(frame) else {
        return;
    };

    // 发号失败（时钟回拨）：丢弃整条消息，靠客户端超时重发补投
    let Some(msg_id) = sessions.next_id().await else {
        return;
    };

    // from 由服务端裁决（客户端伪造无效）；content 的 `Bytes`
    // 零拷贝传递给转发路径
    let outgoing = Msg {
        from,
        to: upstream.to,
        msg_id,
        content: upstream.content,
    };
    sessions.deliver(&outgoing).await;

    // 消息级确认：告诉发送方全局 msg_id（本地排序/去重/同步游标都用它）
    let ack = MsgAck { msg_id };
    let _ = reply(state, handle, &ack).await;
}

/// 离线同步：按游标拉取一批，空批 = 「没有更多」。
async fn handle_sync(
    sessions: &Sessions,
    state: &mut SessionState,
    handle: &ConnectionHandle,
    frame: &im_protocol::Frame,
) {
    let Some(user_id) = state.user else {
        return; // 未登录：同步无意义
    };
    let Ok(req) = SyncReq::decode_frame(frame) else {
        return;
    };

    let batch = sessions.config().sync_batch_size;
    let messages = sessions.sync_since(user_id, req.since, batch);
    let resp = SyncResp { messages };
    let _ = reply(state, handle, &resp).await;
}

// ────────────────────────────────────────────────────────────────
// 接入：accept 循环
// ────────────────────────────────────────────────────────────────

/// accept 循环：每条新连接分配 `conn_id` 并 spawn 会话 task。
///
/// # Errors
///
/// accept 发生 IO 故障时返回 [`TransportError::Io`]；收到关停信号返回 `Ok(())`。
pub async fn serve(
    listener: TcpListener,
    sessions: Sessions,
    shutdown: ShutdownRx,
) -> Result<(), TransportError> {
    let mut accept_shutdown = shutdown.clone();
    loop {
        tokio::select! {
            () = accept_shutdown.wait() => return Ok(()),
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else { continue };
                let sessions = sessions.clone();
                let shutdown = shutdown.clone();
                let conn_id = sessions.next_conn_id();
                tokio::spawn(async move {
                    // 返回值只用于诊断：连接的失败原因已在网关层处理
                    let _ =
                        serve_connection(&sessions, conn_id, stream, GatewayConfig::default(), shutdown)
                            .await;
                });
            }
        }
    }
}

/// 测试/嵌入脚手架：在随机端口起一个完整服务，返回地址与句柄。
///
/// # Errors
///
/// 端口绑定失败时返回 [`TransportError::Io`]。
pub async fn spawn_server(
    config: SessionConfig,
) -> Result<(SocketAddr, Sessions, ShutdownTx), TransportError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let sessions = Sessions::new(config);
    let (shutdown_tx, shutdown_rx) = shutdown_channel();
    tokio::spawn(serve(listener, sessions.clone(), shutdown_rx));
    Ok((addr, sessions, shutdown_tx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use im_protocol::Frame;
    use im_transport::Connection;
    use tokio::time::timeout;

    /// 测试统一的等待上限：本地回环上任何正常交互都应远快于此。
    const WAIT: Duration = Duration::from_secs(2);

    /// 测试配置：口令 "t"，其余默认。
    fn test_config() -> SessionConfig {
        SessionConfig {
            authenticator: Arc::new(StaticToken { token: "t".to_string() }),
            ..SessionConfig::default()
        }
    }

    /// 起服务（随机端口）。
    async fn server() -> (SocketAddr, Sessions, ShutdownTx) {
        spawn_server(test_config()).await.expect("服务应能启动")
    }

    /// 测试客户端：裸 `Connection` + 每帧递增的 seq（与服务端约定的协议用法）。
    struct TestClient {
        conn: Connection,
        seq: u64,
    }

    impl TestClient {
        async fn connect(addr: SocketAddr) -> Self {
            Self {
                conn: Connection::connect(&addr.to_string()).await.unwrap(),
                seq: 0,
            }
        }

        fn next_seq(&mut self) -> u64 {
            self.seq += 1;
            self.seq
        }

        async fn send(&mut self, frame: &Frame) {
            self.conn.write_frame(frame).await.unwrap();
        }

        /// 收一帧并按载荷类型解码。
        async fn recv<T: Payload>(&mut self) -> T {
            let frame = timeout(WAIT, self.conn.read_frame())
                .await
                .expect("2s 内应收到帧")
                .expect("连接正常")
                .expect("连接未关闭");
            T::decode_frame(&frame).expect("载荷应与命令字匹配")
        }

        /// 握手并返回应答。
        async fn handshake(&mut self, user_id: u64, token: &str) -> HandshakeAck {
            let hs = Handshake {
                user_id,
                token: token.to_string(),
            };
            let frame = hs.encode_frame(self.next_seq(), 0);
            self.send(&frame).await;
            self.recv().await
        }

        /// 发一条上行消息（from/msg_id 留给服务端裁决）。
        async fn send_msg(&mut self, to: u64, content: &[u8]) {
            let msg = Msg {
                from: 0,
                to,
                msg_id: 0,
                content: Bytes::copy_from_slice(content),
            };
            let frame = msg.encode_frame(self.next_seq(), 0);
            self.send(&frame).await;
        }

        /// 发起离线同步。
        async fn sync(&mut self, since: u64) -> SyncResp {
            let req = SyncReq { since };
            let frame = req.encode_frame(self.next_seq(), 0);
            self.send(&frame).await;
            self.recv().await
        }

        /// 断言一小段时间内没有任何帧到达。
        async fn expect_silence(&mut self) {
            assert!(
                timeout(Duration::from_millis(300), self.conn.read_frame())
                    .await
                    .is_err(),
                "不应有任何回帧"
            );
        }
    }

    /// 握手成功：session_id 是雪花 ID（非零），路由表 +1。
    #[tokio::test]
    async fn handshake_accepts_and_assigns_session_id() {
        let (addr, sessions, _shutdown) = server().await;
        let mut alice = TestClient::connect(addr).await;

        let ack = alice.handshake(1, "t").await;
        assert!(ack.is_accepted());
        assert_ne!(ack.session_id, 0, "session_id 应由雪花分配");
        assert_eq!(sessions.online_count(), 1);
    }

    /// 错误口令：拒绝且说明原因（不区分「用户不存在/密码错」，防账号探测）。
    #[tokio::test]
    async fn handshake_rejects_bad_token() {
        let (addr, sessions, _shutdown) = server().await;
        let mut alice = TestClient::connect(addr).await;

        let ack = alice.handshake(1, "wrong").await;
        assert!(!ack.is_accepted());
        assert!(
            ack.reason.contains("credentials"),
            "原因应可读: {}",
            ack.reason
        );
        assert_eq!(sessions.online_count(), 0, "拒绝登录不占路由");
    }

    /// 单端登录：同账号第二个连接被拒，旧连接不受影响。
    #[tokio::test]
    async fn duplicate_login_is_rejected() {
        let (addr, sessions, _shutdown) = server().await;
        let mut first = TestClient::connect(addr).await;
        let mut second = TestClient::connect(addr).await;

        assert!(first.handshake(7, "t").await.is_accepted());
        let ack = second.handshake(7, "t").await;
        assert!(!ack.is_accepted());
        assert!(
            ack.reason.contains("already online"),
            "原因应可读: {}",
            ack.reason
        );
        assert_eq!(sessions.online_count(), 1, "只有旧连接在线");
    }

    /// 主线用例：两个在线用户互发——Bob 收到被改写 sender 的下行消息，
    /// Alice 收到携带同一 msg_id 的确认。
    #[tokio::test]
    async fn msg_routes_between_online_users() {
        let (addr, _sessions, _shutdown) = server().await;
        let mut alice = TestClient::connect(addr).await;
        let mut bob = TestClient::connect(addr).await;
        assert!(alice.handshake(1, "t").await.is_accepted());
        assert!(bob.handshake(2, "t").await.is_accepted());

        alice.send_msg(2, b"hi bob").await;

        // Bob 视角：from 被服务端裁决为 1（客户端伪造无效），msg_id 已分配
        let msg: Msg = bob.recv().await;
        assert_eq!(msg.from, 1);
        assert_eq!(msg.to, 2);
        assert_eq!(msg.content, Bytes::from_static(b"hi bob"));
        assert_ne!(msg.msg_id, 0);

        // Alice 视角：消息级确认与 Bob 收到的 msg_id 一致
        let ack: MsgAck = alice.recv().await;
        assert_eq!(ack.msg_id, msg.msg_id);
    }

    /// 离线暂存 + 登录同步：Bob 不在线时消息入队，
    /// Bob 上线后 SyncReq(since=0) 一次拉走；再拉为空。
    #[tokio::test]
    async fn msg_to_offline_user_is_queued_then_synced() {
        let (addr, sessions, _shutdown) = server().await;
        let mut alice = TestClient::connect(addr).await;
        assert!(alice.handshake(1, "t").await.is_accepted());

        alice.send_msg(2, b"offline-hello").await;
        let ack: MsgAck = alice.recv().await; // 等服务端处理完
        assert_eq!(sessions.offline_count(2), 1);

        let mut bob = TestClient::connect(addr).await;
        assert!(bob.handshake(2, "t").await.is_accepted());
        let resp = bob.sync(0).await;
        assert_eq!(resp.messages.len(), 1);
        assert_eq!(
            resp.messages[0].content,
            Bytes::from_static(b"offline-hello")
        );
        assert_eq!(resp.messages[0].from, 1);
        assert_eq!(resp.messages[0].msg_id, ack.msg_id, "离线的 msg_id 与 Ack 一致");

        // 已取走：再拉为空（「没有更多」）
        let again = bob.sync(0).await;
        assert!(again.messages.is_empty());
    }

    /// 同 seq 重发：去重窗口丢弃第二份，接收方只看到一条。
    #[tokio::test]
    async fn duplicate_upstream_seq_is_dropped() {
        let (addr, _sessions, _shutdown) = server().await;
        let mut alice = TestClient::connect(addr).await;
        let mut bob = TestClient::connect(addr).await;
        assert!(alice.handshake(1, "t").await.is_accepted());
        assert!(bob.handshake(2, "t").await.is_accepted());

        // 同一帧字节原样发两次（应用层重发的最真实形态）
        let msg = Msg {
            from: 0,
            to: 2,
            msg_id: 0,
            content: Bytes::from_static(b"dup"),
        };
        let frame = msg.encode_frame(2, 0); // handshake 用了 seq=1，此处 seq=2
        alice.send(&frame).await;
        alice.send(&frame).await;

        let received: Msg = bob.recv().await;
        let ack: MsgAck = alice.recv().await;
        assert_eq!(received.msg_id, ack.msg_id);

        // 第二份被去重：双端都不再有新帧
        alice.expect_silence().await;
        bob.expect_silence().await;
    }

    /// 未登录先发消息：无任何回执（丢弃，不进路由也不入离线）。
    #[tokio::test]
    async fn unauthenticated_msg_gets_no_ack() {
        let (addr, sessions, _shutdown) = server().await;
        let mut intruder = TestClient::connect(addr).await;

        intruder.send_msg(2, b"spoof").await;
        intruder.expect_silence().await;
        assert_eq!(sessions.offline_count(2), 0, "未认证消息不入离线队列");
    }

    /// 离线队列有界：超限丢最老的，登录后只能同步到最新 N 条。
    #[tokio::test]
    async fn offline_queue_is_bounded() {
        let config = SessionConfig {
            max_offline_per_user: 3,
            ..test_config()
        };
        let (addr, _sessions, _shutdown) =
            spawn_server(config).await.expect("服务应能启动");

        let mut alice = TestClient::connect(addr).await;
        assert!(alice.handshake(1, "t").await.is_accepted());

        for i in 0..5 {
            alice.send_msg(2, format!("m{i}").as_bytes()).await;
        }
        for _ in 0..5 {
            let _: MsgAck = alice.recv().await; // 消化完 5 个确认
        }

        let mut bob = TestClient::connect(addr).await;
        assert!(bob.handshake(2, "t").await.is_accepted());
        let resp = bob.sync(0).await;
        assert_eq!(resp.messages.len(), 3, "只保留最新 3 条");
        // 丢最老：内容是 m2、m3、m4，且 msg_id 升序
        assert_eq!(resp.messages[0].content, Bytes::from_static(b"m2"));
        assert_eq!(resp.messages[2].content, Bytes::from_static(b"m4"));
        assert!(
            resp.messages[0].msg_id < resp.messages[2].msg_id,
            "离线队列按 msg_id 升序"
        );
    }

    /// 断连收尾：连接死亡后路由被注销（带 conn_id 校验），
    /// 同账号可立即重连登录。
    #[tokio::test]
    async fn disconnect_unregisters_session() {
        let (addr, sessions, _shutdown) = server().await;
        let mut alice = TestClient::connect(addr).await;
        assert!(alice.handshake(1, "t").await.is_accepted());

        drop(alice); // 直接断开（客户端崩溃/网络中断的最简模拟）

        // 收尾是异步的（网关先感知 EOF）：轮询等它发生
        for _ in 0..100 {
            if sessions.online_count() == 0 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sessions.online_count(), 0, "断连后路由应被注销");

        // 同账号重连成功（旧路由已摘掉，不会 AlreadyOnline）
        let mut alice2 = TestClient::connect(addr).await;
        assert!(alice2.handshake(1, "t").await.is_accepted());
    }

    /// 下行帧序号：离线批量同步时消息按 msg_id 升序（入队序 = 雪花生成序）。
    #[tokio::test]
    async fn offline_batch_is_ordered_by_msg_id() {
        let (addr, _sessions, _shutdown) = server().await;
        let mut alice = TestClient::connect(addr).await;
        assert!(alice.handshake(1, "t").await.is_accepted());

        // 三条离线消息
        for i in 0..3 {
            alice.send_msg(2, format!("s{i}").as_bytes()).await;
        }
        for _ in 0..3 {
            let _: MsgAck = alice.recv().await;
        }

        let mut bob = TestClient::connect(addr).await;
        assert!(bob.handshake(2, "t").await.is_accepted());
        let resp = bob.sync(0).await;
        assert_eq!(resp.messages.len(), 3);
        assert!(resp.messages[0].msg_id < resp.messages[1].msg_id);
        assert!(resp.messages[1].msg_id < resp.messages[2].msg_id);
    }
}

