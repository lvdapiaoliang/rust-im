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
        Self {
            inner: Arc::new(Inner {
                config,
                router: Router::new(64),
                snowflake: Mutex::new(snowflake),
                offline: Mutex::new(HashMap::new()),
                conn_seq: AtomicU64::new(0),
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
