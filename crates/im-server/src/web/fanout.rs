//! 群扇出中枢（阶段 7）：每群一个 actor + 成员快照缓存 + 慢消费者隔离。
//!
//! # 解决什么问题
//!
//! 一条群消息要复制给群里**每个成员**。朴素做法（在 `handle_msg` 里
//! 循环成员逐个 `deliver`）有三个致命伤：
//!
//! 1. **成员表在哪**：会话核心没有 DB（刻意的，见
//!    [`crate::session::GroupRouter`] 的论证），逐条消息查一次全量
//!    成员表，2 万人 = 2 万行/条消息；
//! 2. **顺序性**：多个发送者并发进群，成员 A 可能先看到 3 号消息
//!    再看到 2 号——群聊顺序是产品语义，不是锦上添花；
//! 3. **慢消费者**：`deliver` 的 `send` 满则挂起，一个不读消息的
//!    成员能把整条扇出路径卡死（队头阻塞）。
//!
//! 三个问题的解恰好是同一个结构——**每群一个扇出 actor**：
//!
//! ```text
//!   handle_msg（任意发送连接）
//!       │  GroupRouter::route(group_id, msg)     ← 依赖注入（trait）
//!       ▼
//!   ┌─ GroupHub ─────────────────────────────────────────┐
//!   │  Mutex<HashMap<group_id, GroupActor>>               │
//!   │     │ 查表命中：直接投递                          │
//!   │     │ 未命中：查库装载快照 → 孵化 actor            │
//!   └─────┬─────────────────────────────────────────────┘
//!         ▼  mpsc（有界收件箱：反压传导给发送者）
//!   ┌─ 单群 actor task ──────────────────────────────────┐
//!   │  members: Vec<u64>   ← 成员快照（Invalidate 置脏）│
//!   │  loop {                                             │
//!   │    Fanout(msg)  → for member: fanout_one（try_send）│
//!   │    Invalidate   → dirty = true                      │
//!   │  }                                                  │
//!   └─────────────────────────────────────────────────────┘
//! ```
//!
//! - **成员快照缓存**：一次全量装载，之后所有扇出吃缓存；REST 加人
//!   只发一条 `Invalidate`（置脏标记），下条消息前才重载——
//!   大多数加人操作后并没有消息紧跟，白查一次库不划算；
//! - **群内顺序**：actor 是单 task，收件箱天然 FIFO——所有成员看到
//!   的消息序 = actor 处理序。群与群之间互不排队（隔离），
//!   这正是「不搞全局总线」的理由（见 [`crate::session`] 模块文档）；
//! - **慢消费者隔离**：投递走 [`Sessions::fanout_one`] 的 `try_send`
//!   同步快路径——满即跳过（`Skipped` 计数），绝不挂起 actor。
//!
//! # 模式落点
//!
//! - **Actor 模式**（第三次实战）：单 task 独占快照状态，消息通信；
//! - **写时失效（invalidate-on-write）**：缓存一致性策略——DB 是
//!   真相源，写路径（REST 加人）负责打脏缓存，读路径（actor）重载；
//! - **接口隔离**：hub 实现 [`GroupRouter`] 只暴露 `route` 一个方法给
//!   会话核心；`invalidate`/`stats` 是 web 层的私交；
//! - **依赖倒置（再一次）**：成员表来源抽象成 [`MemberSource`]——
//!   [`GroupStore`] 是 DB 实现，压测（`im-bench`）与无 DB 测试用
//!   内存实现，扇出引擎对「成员表在哪」零假设。
//!
//! # 已知取舍（诚实的账单）
//!
//! - actor **不退役**：群消息冷了 actor 也常驻（一个 mpsc + 一个 Vec，
//!   空闲成本 ≈ 0）。无限多群的回收策略留给阶段 10 工程化；
//! - 快照重载失败**保旧**：可用性优先——空快照等于全群吞消息，
//!   比暂时多/少一个成员的代价大得多；
//! - `skipped` 的消息**不可恢复**：群消息尚无持久化（离线队列只暂存
//!   离线成员），慢消费者本轮丢失，后续阶段补同步游标。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use im_protocol::Msg;
use tokio::sync::mpsc;

use crate::session::{FanoutOutcome, GroupRouter, RouteFuture, Sessions};

use super::groups::{GroupError, GroupStore};

/// 每群 actor 的收件箱容量。
///
/// 扇出本身是同步快路径（微秒级/人），收件箱真正会积压的场景只有
/// 快照重载卡在 DB 上——256 条缓冲足够消化一次 DB 抖动。
const ACTOR_INBOX_CAPACITY: usize = 256;

/// actor 的两种工作。
enum Job {
    /// 扇出一条群消息（已分配 `msg_id` 的最终形态）。
    Fanout {
        /// 消息体。
        msg: Msg,
    },
    /// 成员表已变更（REST 加人后调用）：快照置脏，下条消息前重载。
    Invalidate,
}

/// 成员表来源：扇出中枢的唯一外部数据依赖（DB 只是其中一种实现）。
///
/// 阶段 7 压测（`im-bench` 的 `group-fanout` 场景）要驱动**真实扇出
/// 引擎**而不连库——把「成员表在哪」抽象成 trait，与
/// [`crate::session::GroupRouter`] 同一手法：核心定契约，外面换实现。
/// 热路径本来就不碰它（成员快照只装载一次），所以替换来源不影响
/// 测量口径。
pub trait MemberSource: Send + Sync {
    /// 全量成员 ID 快照（空 = 群不存在；仅在孵化/重载时被调用）。
    fn list_members(&self, group_id: u64) -> MemberList<'_>;
}

/// [`MemberSource::list_members`] 的返回形态（装箱 future，trait 可作 `dyn`）。
pub type MemberList<'a> = Pin<Box<dyn Future<Output = Result<Vec<u64>, GroupError>> + Send + 'a>>;

// DB 实现：仓储的固有 async 方法摆进 trait（与 GroupRouter 的 hub 实现同一换法）。
impl MemberSource for GroupStore {
    fn list_members(&self, group_id: u64) -> MemberList<'_> {
        Box::pin(GroupStore::list_members(self, group_id))
    }
}

/// 单群统计（诊断与压测的口径；原子计数，actor 与读者无锁并行）。
#[derive(Debug, Default)]
pub struct GroupStats {
    /// 接管的消息数（进入扇出的条数）。
    pub fanned: AtomicU64,
    /// 在线送达的接收者次数。
    pub delivered: AtomicU64,
    /// 慢消费者跳过的次数（隔离取舍的账单，见模块文档）。
    pub skipped: AtomicU64,
    /// 离线降级的次数。
    pub offline: AtomicU64,
}

/// 群 actor 的句柄（hub 表里的值；克隆廉价——channel sender + Arc）。
#[derive(Debug, Clone)]
struct GroupActor {
    /// actor 收件箱。
    tx: mpsc::Sender<Job>,
    /// 统计口径（与 actor 共享）。
    stats: Arc<GroupStats>,
}

/// 群扇出中枢：`group_id → actor` 的注册表 + 未命中时的孵化器。
pub struct GroupHub {
    /// 成员表来源（快照装载与重载；DB 实现是 [`GroupStore`]）。
    source: Arc<dyn MemberSource>,
    /// 会话中心（扇出投递的执行面）。
    sessions: Sessions,
    /// actor 注册表。`std::sync::Mutex`：微秒级临界区（查/插一个条目），
    /// 不跨 `await`——与 [`crate::router::Router`] 同一纪律。
    actors: Mutex<HashMap<u64, GroupActor>>,
}

// 手动 Clone（不能 derive）：注册表克隆的是「句柄快照」——actor task
// 不受影响，新旧中枢共享同一批 actor（AppState 的廉价克隆依赖这一点）。
impl Clone for GroupHub {
    fn clone(&self) -> Self {
        let actors = self.actors.lock().expect("扇出中枢锁中毒").clone();
        Self {
            source: Arc::clone(&self.source),
            sessions: self.sessions.clone(),
            actors: Mutex::new(actors),
        }
    }
}

// 手动 Debug（不能 derive）：`Sessions` 无 Debug；中枢的调试价值只在
// 「有多少 actor 在工作」，报个数即可（store/sessions 无意义故不列，
// 刻意不满足「全字段」的 lint 要求）。
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for GroupHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.actors.lock().expect("扇出中枢锁中毒").len();
        f.debug_struct("GroupHub").field("actors", &count).finish()
    }
}

impl GroupHub {
    /// 创建中枢（DB 成员源；不起 actor——全部惰性：首条群消息才孵化）。
    #[must_use]
    pub fn new(store: GroupStore, sessions: Sessions) -> Self {
        Self::with_source(Arc::new(store), sessions)
    }

    /// 用自定义成员源创建中枢：压测与无 DB 测试的装配点
    /// （[`MemberSource`] 的存在理由，见 trait 文档）。
    #[must_use]
    pub fn with_source(source: Arc<dyn MemberSource>, sessions: Sessions) -> Self {
        Self { source, sessions, actors: Mutex::new(HashMap::new()) }
    }

    /// 取群 actor（克隆句柄、立刻放锁——与 `Router::get` 同一纪律）。
    fn actor(&self, group_id: u64) -> Option<GroupActor> {
        self.actors.lock().expect("扇出中枢锁中毒").get(&group_id).cloned()
    }

    /// 孵化一个群 actor 并登记（收件箱满负荷前的唯一写表点）。
    fn spawn_actor(&self, group_id: u64, members: Vec<u64>) -> GroupActor {
        let (tx, rx) = mpsc::channel(ACTOR_INBOX_CAPACITY);
        let stats = Arc::new(GroupStats::default());
        tokio::spawn(actor_loop(
            Arc::clone(&self.source),
            self.sessions.clone(),
            group_id,
            members,
            rx,
            Arc::clone(&stats),
        ));
        let actor = GroupActor { tx, stats };
        self.actors.lock().expect("扇出中枢锁中毒").insert(group_id, actor.clone());
        actor
    }

    /// 通知 actor 成员表已变更（快照置脏）。
    ///
    /// actor 不存在 = 该群从未有消息（无快照可失效），静默跳过；
    /// 收件箱满用 `try_send`——失效信号丢了也只是快照多脏一阵
    /// （下条消息前总会重载），不值得为它反压 REST 处理器。
    pub fn invalidate(&self, group_id: u64) {
        if let Some(actor) = self.actor(group_id) {
            let _ = actor.tx.try_send(Job::Invalidate);
        }
    }

    /// 单群统计句柄（测试与压测的读取口径；actor 未孵化则 `None`）。
    #[must_use]
    pub fn stats(&self, group_id: u64) -> Option<Arc<GroupStats>> {
        self.actor(group_id).map(|a| a.stats)
    }

    /// 已孵化的 actor 数（诊断）。
    ///
    /// # Panics
    ///
    /// 扇出中枢锁中毒时 panic（锁中毒属实现 bug，应立即暴露）。
    #[must_use]
    pub fn actor_count(&self) -> usize {
        self.actors.lock().expect("扇出中枢锁中毒").len()
    }

    /// 投递一条扇出工作给 actor。
    ///
    /// 用阻塞 `send` 而非 `try_send`：actor 收件箱打满说明它卡在
    /// 快照重载上——此刻让发送者**等**（反压传导给这一条连接），
    /// 也比丢掉一条将要被 Ack 的消息好。「不丢已接管的消息」
    /// 优先级高于「发送者永远不被阻塞」。
    async fn enqueue(&self, actor: GroupActor, msg: &Msg) -> bool {
        actor.tx.send(Job::Fanout { msg: msg.clone() }).await.is_ok()
    }
}

/// 会话核心的注入点：hub 以 [`GroupRouter`] 的身份回答「`to` 是不是群」。
impl GroupRouter for GroupHub {
    fn route(&self, to: u64, msg: &Msg) -> RouteFuture<'_> {
        // 消息先克隆进闭包：future 的借用只剩 &self——
        // 签名因此只有 'self 一个生命周期（两个省略生命周期会互相打架）
        let msg = msg.clone();
        Box::pin(async move {
            // 快路径：表里已有 actor = 一定是群（表项只由「查到成员」创建）
            if let Some(actor) = self.actor(to) {
                return self.enqueue(actor, &msg).await;
            }

            // 慢路径：首条群消息 → 查成员源装载快照，孵化 actor。
            // 空 = 不是群：建群事务保证群主必在成员表，空表即群不存在。
            // 装载失败也回落 false（单聊投递）——DB 故障时的降级
            // 比吞消息便宜（消息落离线队列，等库恢复）。
            let members = match self.source.list_members(to).await {
                Ok(members) if !members.is_empty() => members,
                _ => return false,
            };
            let actor = self.spawn_actor(to, members);
            self.enqueue(actor, &msg).await
        })
    }
}

/// 单群 actor：成员快照的唯一属主 + 群内投递顺序的串行化点。
///
/// 生命周期 = hub 的注册项存活期（本阶段不退役，见模块文档）；
/// 收件箱关闭（hub drop，即整个服务停机）时 `recv` 返回 `None` 自然退出。
async fn actor_loop(
    source: Arc<dyn MemberSource>,
    sessions: Sessions,
    group_id: u64,
    mut members: Vec<u64>,
    mut rx: mpsc::Receiver<Job>,
    stats: Arc<GroupStats>,
) {
    // 快照脏标记（`Invalidate` 置位；扇出前重载并复位）
    let mut dirty = false;
    while let Some(job) = rx.recv().await {
        match job {
            Job::Invalidate => dirty = true,
            Job::Fanout { msg } => {
                if dirty {
                    // 重载失败保旧快照：可用性优先（见模块文档的取舍）
                    if let Ok(fresh) = source.list_members(group_id).await {
                        members = fresh;
                    }
                    dirty = false;
                }
                stats.fanned.fetch_add(1, Ordering::Relaxed);
                // 同步快路径：一口气扇出全部成员，无一处 await
                //（见 Sessions::fanout_one）——顺带保证群内投递顺序
                for &member in &members {
                    // 发送者本人跳过：单聊路径本就不回显给自己
                    //（乐观插入已在），群路径保持同一语义
                    if member == msg.from {
                        continue;
                    }
                    match sessions.fanout_one(member, &msg) {
                        FanoutOutcome::Delivered => {
                            stats.delivered.fetch_add(1, Ordering::Relaxed);
                        }
                        FanoutOutcome::Skipped => {
                            stats.skipped.fetch_add(1, Ordering::Relaxed);
                        }
                        FanoutOutcome::Offline => {
                            stats.offline.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::{FrameSink, SendFuture, TrySendError};
    use crate::web::account::AccountStore;
    use crate::web::db::testing::{pool_or_skip, test_sessions};
    use bytes::Bytes;
    use im_protocol::{Frame, Payload};
    use im_transport::TransportError;
    use std::time::Duration;
    use tokio::time::{sleep, timeout};

    /// 测试等待上限。
    const WAIT: Duration = Duration::from_secs(2);

    /// 支持 `try_send` 的测试 sink（扇出投递的完整形状）。
    #[derive(Debug)]
    struct FanoutSink(mpsc::Sender<Frame>);

    impl FrameSink for FanoutSink {
        fn send(&self, frame: Frame) -> SendFuture<'_> {
            Box::pin(async move { self.0.send(frame).await.map_err(|_| TransportError::Closed) })
        }

        fn try_send(&self, frame: Frame) -> Result<(), TrySendError> {
            self.0.try_send(frame).map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => TrySendError::Full,
                mpsc::error::TrySendError::Closed(_) => TrySendError::Closed,
            })
        }
    }

    /// 测试装配：迁移到位的 PG + 会话中心 + hub + 账号/群仓储
    /// （PG 不可达则跳过——与 web 模块其余测试同一约定）。
    async fn hub_or_skip() -> Option<(GroupHub, GroupStore, AccountStore, Sessions)> {
        let pool = pool_or_skip().await?;
        let sessions = test_sessions();
        let store = GroupStore::new(pool.clone());
        let accounts = AccountStore::new(pool);
        let hub = GroupHub::new(store.clone(), sessions.clone());
        Some((hub, store, accounts, sessions))
    }

    /// 造一个测试用户（仓储直连，REST 不在本模块的考察范围）。
    async fn user(accounts: &AccountStore, sessions: &Sessions, name: &str) -> u64 {
        accounts
            .register(
                sessions,
                &format!("fanout_{name}_{}", uuid::Uuid::new_v4().simple()),
                "pw",
                name,
            )
            .await
            .expect("注册应成功")
            .id
    }

    /// 注册一个带 `try_send` sink 的「在线成员」，返回接收通道。
    fn online_member(sessions: &Sessions, user_id: u64, conn_id: u64) -> mpsc::Receiver<Frame> {
        let (tx, rx) = mpsc::channel(64);
        sessions.register(user_id, conn_id, Arc::new(FanoutSink(tx))).expect("首个注册不应冲突");
        rx
    }

    /// 轮询等统计满足条件（扇出是异步 actor，测试侧只能等它发生）。
    async fn wait_stats(
        hub: &GroupHub,
        group_id: u64,
        want: &str,
        check: impl Fn(&GroupStats) -> bool,
    ) {
        let stats = hub.stats(group_id).expect("actor 应已孵化");
        for _ in 0..200 {
            if check(&stats) {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "2s 内统计未达标（{want}）: fanned={} delivered={} skipped={} offline={}",
            stats.fanned.load(Ordering::Relaxed),
            stats.delivered.load(Ordering::Relaxed),
            stats.skipped.load(Ordering::Relaxed),
            stats.offline.load(Ordering::Relaxed)
        );
    }

    /// 主线：群消息扇出到全体在线成员（发送者本人除外——不回显），
    /// 载荷 `to` 保持群 ID。
    #[tokio::test]
    async fn fanout_reaches_members_not_sender() {
        let Some((hub, store, accounts, sessions)) = hub_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let owner = user(&accounts, &sessions, "owner").await;
        let a = user(&accounts, &sessions, "a").await;
        let b = user(&accounts, &sessions, "b").await;

        let group = store.create_group(&sessions, "fanout-main", owner).await.expect("建群应成功");
        store.add_member(group.id, owner, a).await.expect("拉人应成功");
        store.add_member(group.id, owner, b).await.expect("拉人应成功");

        let mut rx_a = online_member(&sessions, a, 1);
        let mut rx_b = online_member(&sessions, b, 2);
        let mut rx_owner = online_member(&sessions, owner, 3);

        let msg = Msg {
            from: owner,
            to: group.id,
            msg_id: 42,
            client_msg_id: 1,
            content: Bytes::from_static(b"hello group"),
        };
        assert!(hub.route(group.id, &msg).await, "群消息应被扇出路径接管");

        wait_stats(&hub, group.id, "delivered == 2", |s| s.delivered.load(Ordering::Relaxed) == 2)
            .await;

        for rx in [&mut rx_a, &mut rx_b] {
            let frame = timeout(WAIT, rx.recv()).await.expect("应收到扇出帧").expect("sink 存活");
            let decoded = Msg::decode_frame(&frame).expect("载荷应与命令字匹配");
            assert_eq!(decoded.to, group.id, "载荷 to 保持群 ID");
            assert_eq!(decoded.from, owner);
        }

        // 发送者本人零回显（乐观插入已在，扇出不补副本）
        assert!(
            timeout(Duration::from_millis(300), rx_owner.recv()).await.is_err(),
            "发送者不应收到自己的回显"
        );
    }

    /// 快照失效：加人 + `invalidate` 后，新成员在下条消息就能收到。
    #[tokio::test]
    async fn invalidate_refreshes_membership_snapshot() {
        let Some((hub, store, accounts, sessions)) = hub_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let owner = user(&accounts, &sessions, "owner").await;
        let a = user(&accounts, &sessions, "a").await;
        let newcomer = user(&accounts, &sessions, "new").await;

        let group =
            store.create_group(&sessions, "fanout-invalidate", owner).await.expect("建群应成功");
        store.add_member(group.id, owner, a).await.expect("拉人应成功");

        let mut rx_a = online_member(&sessions, a, 1);
        let first = Msg {
            from: owner,
            to: group.id,
            msg_id: 1,
            client_msg_id: 1,
            content: Bytes::from_static(b"m1"),
        };
        assert!(hub.route(group.id, &first).await);
        wait_stats(&hub, group.id, "首条扇出完成", |s| {
            s.fanned.load(Ordering::Relaxed) == 1 && s.delivered.load(Ordering::Relaxed) == 1
        })
        .await;
        let _ = timeout(WAIT, rx_a.recv()).await.expect("首条消息应到达");

        // REST 路径的两步：改库 + 打脏快照
        store.add_member(group.id, owner, newcomer).await.expect("拉人应成功");
        hub.invalidate(group.id);

        let mut rx_new = online_member(&sessions, newcomer, 2);
        let second = Msg {
            from: owner,
            to: group.id,
            msg_id: 2,
            client_msg_id: 2,
            content: Bytes::from_static(b"m2"),
        };
        assert!(hub.route(group.id, &second).await);
        wait_stats(&hub, group.id, "新成员收到", |s| {
            s.fanned.load(Ordering::Relaxed) == 2 && s.delivered.load(Ordering::Relaxed) == 3
        })
        .await;
        let got = timeout(WAIT, rx_new.recv()).await.expect("新成员应收到").expect("sink 存活");
        let decoded = Msg::decode_frame(&got).expect("载荷应与命令字匹配");
        assert_eq!(decoded.msg_id, 2);
    }

    /// 非群 ID：route 返回 false（回落单聊投递），不孵化 actor。
    #[tokio::test]
    async fn non_group_id_falls_back() {
        let Some((hub, _store, accounts, sessions)) = hub_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let someone = user(&accounts, &sessions, "user").await;
        let msg = Msg {
            from: someone,
            to: 999_999_999,
            msg_id: 1,
            client_msg_id: 1,
            content: Bytes::new(),
        };
        assert!(!hub.route(999_999_999, &msg).await, "不存在的 ID 不应被群路径接管");
        assert_eq!(hub.actor_count(), 0, "非群不应孵化 actor");
    }

    /// 慢消费者隔离：一个成员的通道打满后消息被跳过（Skipped），
    /// 正常成员的投递不受任何影响。
    #[tokio::test]
    async fn slow_consumer_is_isolated() {
        let Some((hub, store, accounts, sessions)) = hub_or_skip().await else {
            eprintln!("skip: PostgreSQL 不可达");
            return;
        };
        let owner = user(&accounts, &sessions, "owner").await;
        let fast = user(&accounts, &sessions, "fast").await;
        let slow = user(&accounts, &sessions, "slow").await;

        let group = store.create_group(&sessions, "fanout-slow", owner).await.expect("建群应成功");
        store.add_member(group.id, owner, fast).await.expect("拉人应成功");
        store.add_member(group.id, owner, slow).await.expect("拉人应成功");

        // fast：正常通道；slow：容量 1，收一条后再也不排空
        let mut rx_fast = online_member(&sessions, fast, 1);
        let (slow_tx, _slow_rx_kept) = mpsc::channel(1);
        sessions.register(slow, 2, Arc::new(FanoutSink(slow_tx))).expect("首个注册不应冲突");

        for i in 1..=3u64 {
            let msg = Msg {
                from: owner,
                to: group.id,
                msg_id: i,
                client_msg_id: i,
                content: Bytes::from_static(b"burst"),
            };
            assert!(hub.route(group.id, &msg).await);
        }

        // slow 收到 1 条（首条 Delivered），后 2 条 Skipped；
        // fast 三条全到——隔离的意义就是它毫发无损
        wait_stats(&hub, group.id, "隔离完成", |s| {
            s.skipped.load(Ordering::Relaxed) == 2 && s.delivered.load(Ordering::Relaxed) == 4 // fast×3 + slow×1
        })
        .await;
        for expected in 1..=3u64 {
            let frame =
                timeout(WAIT, rx_fast.recv()).await.expect("fast 应全收").expect("sink 存活");
            let decoded = Msg::decode_frame(&frame).expect("载荷应与命令字匹配");
            assert_eq!(decoded.msg_id, expected, "fast 的消息完整且有序");
        }
    }

    /// 内存成员源：固定成员表，无 DB 装配（`with_source` 路径的主线验证）。
    ///
    /// 上面四个测试都要 PG；这个测试在任何环境都跑得动——守住
    /// trait 装配不被改坏，也是 `im-bench` 同款装配的单元级影子。
    #[derive(Debug)]
    struct MemSource(Vec<u64>);

    impl MemberSource for MemSource {
        fn list_members(&self, _group_id: u64) -> MemberList<'_> {
            let members = self.0.clone();
            Box::pin(async move { Ok::<Vec<u64>, GroupError>(members) })
        }
    }

    /// 内存成员源 + `with_source` 装配：不连库也能孵化 actor 并扇出。
    #[tokio::test]
    async fn in_memory_source_fans_out_without_db() {
        let sessions = test_sessions();
        let hub = GroupHub::with_source(Arc::new(MemSource(vec![101, 102, 103])), sessions.clone());

        // 三名成员全部在线；发送者 999 不是成员：delivered 口径就是成员数
        let mut rxs = Vec::new();
        for (i, uid) in [101u64, 102, 103].into_iter().enumerate() {
            rxs.push(online_member(&sessions, uid, i as u64 + 1));
        }

        let msg = Msg {
            from: 999,
            to: 7,
            msg_id: 1,
            client_msg_id: 1,
            content: Bytes::from_static(b"mem"),
        };
        assert!(hub.route(7, &msg).await, "内存源非空，应被扇出路径接管");

        wait_stats(&hub, 7, "三成员送达", |s| s.delivered.load(Ordering::Relaxed) == 3).await;
        assert_eq!(hub.actor_count(), 1, "内存源孵化了一个 actor");

        for rx in &mut rxs {
            let frame = timeout(WAIT, rx.recv()).await.expect("应收到扇出帧").expect("sink 存活");
            let decoded = Msg::decode_frame(&frame).expect("载荷应与命令字匹配");
            assert_eq!(decoded.to, 7, "载荷 to 保持群 ID");
        }
    }
}
