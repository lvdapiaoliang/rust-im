//! `Outbox`：消息级重传邮箱（阶段 4）。
//!
//! TCP 的可靠只到内核缓冲区：消息「写成功」不代表对端收到，对端收到
//! 不代表 Ack 回得来。「至少一次」发送要求每条消息在**本地持久化的
//! 重发表**里待到被 Ack 核销为止——期间按指数退避反复重发，配合接收端
//! 按 `client_msg_id` 去重（消息层）拼出「恰好一次」的用户体验。
//! 这正是 TCP 自身超时重传机制在应用层的翻版：端到端原则——
//! 连接层的可靠性承诺覆盖不了「服务端已收到但 Ack 丢失」的窗口。
//!
//! # 状态机
//!
//! ```text
//! enqueue ──发送──▶ [等 Ack，RTO 计时中]
//!                      │ 超时：attempts+1，指数退避后重发
//!                      ├─ Ack 到达：核销转正（pending → 正式消息）
//!                      └─ attempts 达上限：放弃，上报 SendFailed
//! 断线：RTO 冻结；重连后 resends() 全量补发（RTO 重置——新连接新起点）
//! ```
//!
//! # 为什么消息级退避不加抖动（jitter）？
//!
//! 连接级重连用 full jitter 防「重连风暴」；消息级重传在同一连接内
//! 逐条独立计时，风暴规模受在途消息数限制。且确定性的退避让
//! 超时行为可测试——可测性本身也是设计约束。
//!
//! # 算法/数据结构落点
//!
//! - **指数退避**：`rto × 2^(attempts-1)`，封顶 30s（`u32` 移位防溢出）；
//! - **线性表扫描**：在途消息量级是「用户手速」（个位数），O(n) 比堆
//!   （BinaryHeap 的删除是 O(n)）更简单且常数更小——数据结构选型
//!   跟着量级走，不跟「高级感」走。

use std::time::{Duration, Instant};

use bytes::Bytes;
use im_protocol::Msg;
use im_storage::{LocalStore, StorageError};

/// 单条消息重传延迟封顶（指数退避的天花板）。
const RETRY_DELAY_CAP: Duration = Duration::from_secs(30);

/// 一条在途消息（内存视图；持久化真身在 [`LocalStore`] 的 `p/` 表里）。
struct Inflight {
    /// 发送方本地去重键（跨重发稳定）。
    client_msg_id: u64,
    /// 接收者。
    to: u64,
    /// 消息内容。
    content: Bytes,
    /// 已发送次数（含首次）。
    attempts: u32,
    /// 下次重传时刻。
    deadline: Instant,
}

impl Inflight {
    /// 还原成上行 `Msg`（`from`/`msg_id` 由服务端裁决）。
    fn to_msg(&self) -> Msg {
        Msg {
            from: 0,
            to: self.to,
            msg_id: 0,
            client_msg_id: self.client_msg_id,
            content: self.content.clone(),
        }
    }
}

/// 重发邮箱：内存中的在途消息表 + RTO 定时器。
///
/// 不持有 [`LocalStore`]：库由连接状态机统一持有，收（入库/游标）
/// 发（重发表）两侧共用一个实例——每次调用按需借用。
pub(crate) struct Outbox {
    inflight: Vec<Inflight>,
    /// 首次重传等待（RTO）。
    retry_timeout: Duration,
    /// 最大发送次数（含首次；超过即放弃）。
    retry_max_attempts: u32,
}

impl Outbox {
    /// 从持久化重发表加载邮箱：上次退出时未 Ack 的消息，
    /// 会在下一次连接建立时被立即补发。
    ///
    /// # Errors
    ///
    /// 见 [`LocalStore::pending_all`]。
    pub(crate) fn load(
        store: &mut LocalStore,
        retry_timeout: Duration,
        retry_max_attempts: u32,
    ) -> Result<Self, StorageError> {
        let mut inflight = Vec::new();
        for pending in store.pending_all()? {
            inflight.push(Inflight {
                client_msg_id: pending.client_msg_id,
                to: pending.to,
                content: pending.content,
                attempts: pending.attempts,
                // deadline 已过：下次连接的 resends() 会立即补发
                deadline: Instant::now(),
            });
        }
        Ok(Self {
            inflight,
            retry_timeout,
            retry_max_attempts,
        })
    }

    /// 登记一条新消息（持久化 + 分配 `client_msg_id`），返回待发的
    /// 上行帧。首次发送计 attempt 1，RTO 从登记时刻起算。
    ///
    /// # Errors
    ///
    /// 磁盘写失败时返回 [`StorageError`]。
    pub(crate) fn enqueue(
        &mut self,
        store: &mut LocalStore,
        to: u64,
        content: Bytes,
    ) -> Result<Msg, StorageError> {
        let client_msg_id = store.enqueue_outgoing(to, &content)?;
        self.inflight.push(Inflight {
            client_msg_id,
            to,
            content,
            attempts: 1,
            deadline: Instant::now() + self.retry_timeout,
        });
        Ok(self.inflight.last().expect("刚 push 过").to_msg())
    }

    /// Ack 核销：从重发表移除并转正为正式消息（挂到与 `to` 的会话里）。
    ///
    /// 重复 Ack（重发后旧 Ack 迟到）无害：早已核销，直接返回。
    ///
    /// # Errors
    ///
    /// 磁盘写失败时返回 [`StorageError`]。
    pub(crate) fn ack(
        &mut self,
        store: &mut LocalStore,
        client_msg_id: u64,
        msg_id: u64,
        from_me: u64,
    ) -> Result<(), StorageError> {
        let Some(pos) = self
            .inflight
            .iter()
            .position(|m| m.client_msg_id == client_msg_id)
        else {
            return Ok(()); // 迟到的重复 Ack：早已核销
        };
        let confirmed = self.inflight.remove(pos);
        store.ack_outgoing(
            client_msg_id,
            msg_id,
            from_me,
            confirmed.to,
            &confirmed.content,
        )
    }

    /// 重连补发：全部在途消息各发一遍，RTO 全部重置。
    ///
    /// 重复投递对消息层无害（接收端按 `client_msg_id` 去重），
    /// 而新连接上立即补发能把「断线期间的消息延迟」压到一次 RTT。
    pub(crate) fn resends(&mut self) -> Vec<Msg> {
        let now = Instant::now();
        for msg in &mut self.inflight {
            msg.deadline = now + self.retry_timeout;
        }
        self.inflight.iter().map(Inflight::to_msg).collect()
    }

    /// 到期重传：遍历在途表，返回（待重发的消息, 已放弃的 `client_msg_id`）。
    ///
    /// 每条到期消息 attempts+1：超过上限的放弃（落盘删除，否则重启后
    /// 又冒出来），未超的按指数退避排下一次并持久化 attempts。
    ///
    /// # Errors
    ///
    /// 磁盘读写失败时返回 [`StorageError`]。
    pub(crate) fn due(
        &mut self,
        store: &mut LocalStore,
        now: Instant,
    ) -> Result<(Vec<Msg>, Vec<u64>), StorageError> {
        let mut resends = Vec::new();
        let mut failed = Vec::new();
        let mut i = 0;
        while i < self.inflight.len() {
            let msg = &mut self.inflight[i];
            if msg.deadline > now {
                i += 1;
                continue;
            }
            msg.attempts += 1;
            if msg.attempts > self.retry_max_attempts {
                let client_msg_id = msg.client_msg_id;
                self.inflight.remove(i);
                store.drop_outgoing(client_msg_id)?;
                failed.push(client_msg_id);
                continue;
            }
            msg.deadline = now + backoff_delay(self.retry_timeout, msg.attempts);
            let (client_msg_id, attempts) = (msg.client_msg_id, msg.attempts);
            store.pending_set_attempts(client_msg_id, attempts)?;
            resends.push(msg.to_msg());
            i += 1;
        }
        Ok((resends, failed))
    }

    /// 最早的重传时刻（无在途消息返回 `None`——select 分支将永久挂起）。
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.inflight.iter().map(|m| m.deadline).min()
    }

    /// 在途消息数。
    pub(crate) fn len(&self) -> usize {
        self.inflight.len()
    }

    /// 是否没有在途消息。
    pub(crate) fn is_empty(&self) -> bool {
        self.inflight.is_empty()
    }
}

/// 指数退避：`rto × 2^(attempts-1)`，封顶 [`RETRY_DELAY_CAP`]。
fn backoff_delay(rto: Duration, attempts: u32) -> Duration {
    // 移位上限 16：再大也是 saturating_mul 的饱和区，纯防溢出
    let shift = attempts.saturating_sub(1).min(16);
    rto.saturating_mul(1 << shift).min(RETRY_DELAY_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试目录 guard（与 engine/store 测试同款）。
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "im-outbox-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("系统时钟正常")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).expect("建临时目录");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn outbox(dir: &TempDir, rto_ms: u64, max_attempts: u32) -> (LocalStore, Outbox) {
        let mut store = LocalStore::open(dir.path()).expect("本地库应能打开");
        let outbox = Outbox::load(
            &mut store,
            Duration::from_millis(rto_ms),
            max_attempts,
        )
        .expect("邮箱应能加载");
        (store, outbox)
    }

    /// 生命周期主线：enqueue → Ack 核销 → 转正落盘可查。
    #[test]
    fn ack_confirms_and_persists_message() {
        let dir = TempDir::new();
        let (mut store, mut outbox) = outbox(&dir, 100, 3);
        let msg = outbox.enqueue(&mut store, 2, Bytes::from_static(b"hi")).unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(msg.client_msg_id, 1, "首个本地 ID 从 1 开始");

        outbox
            .ack(&mut store, msg.client_msg_id, 777, 1)
            .unwrap();
        assert!(outbox.is_empty(), "Ack 后核销");

        // 转正落盘：直接重开底层库验证（绕过邮箱，防自说自话）
        let mut store = LocalStore::open(dir.path()).unwrap();
        let history = store.history(2, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].msg_id, 777);
        assert_eq!(history[0].client_msg_id, msg.client_msg_id);
        assert!(store.pending_all().unwrap().is_empty());
    }

    /// 重复 Ack（重发后旧 Ack 迟到）是幂等的 no-op。
    #[test]
    fn duplicate_ack_is_harmless() {
        let dir = TempDir::new();
        let (mut store, mut outbox) = outbox(&dir, 100, 3);
        let msg = outbox.enqueue(&mut store, 2, Bytes::from_static(b"once")).unwrap();
        outbox.ack(&mut store, msg.client_msg_id, 100, 1).unwrap();
        outbox.ack(&mut store, msg.client_msg_id, 100, 1).unwrap(); // 迟到的重复 Ack
        assert!(outbox.is_empty());
    }

    /// 超时重发：RTO 到期 due 返回消息，且退避翻倍排下一次。
    #[test]
    fn due_resends_then_reschedules_with_backoff() {
        let dir = TempDir::new();
        let (mut store, mut outbox) = outbox(&dir, 20, 5);
        let msg = outbox
            .enqueue(&mut store, 2, Bytes::from_static(b"retry me"))
            .unwrap();

        std::thread::sleep(Duration::from_millis(30)); // 越过 RTO
        let (resends, failed) = outbox.due(&mut store, Instant::now()).unwrap();
        assert_eq!(resends.len(), 1, "到期应重发");
        assert!(failed.is_empty());
        assert_eq!(resends[0].client_msg_id, msg.client_msg_id);
        assert_eq!(resends[0].content, Bytes::from_static(b"retry me"));
        assert_eq!(outbox.len(), 1, "重发后仍在途（等 Ack）");

        // 指数退避：第二轮 deadline 至少在一倍 RTO 之后（2^1 = 2 倍）
        let next = outbox.next_deadline().expect("仍有在途消息");
        assert!(
            next > Instant::now() + Duration::from_millis(20),
            "退避应翻倍，实际下次 {next:?}"
        );
    }

    /// 超过最大次数：放弃并从重发表落盘删除（重启不再复活）。
    #[test]
    fn gives_up_after_max_attempts() {
        let dir = TempDir::new();
        let (mut store, mut outbox) = outbox(&dir, 5, 2);
        let msg = outbox
            .enqueue(&mut store, 2, Bytes::from_static(b"hopeless"))
            .unwrap();

        std::thread::sleep(Duration::from_millis(10));
        let (resends, failed) = outbox.due(&mut store, Instant::now()).unwrap();
        assert_eq!(resends.len(), 1, "第一次到期：attempt 2，未超上限");
        assert!(failed.is_empty());

        std::thread::sleep(Duration::from_millis(20));
        let (resends, failed) = outbox.due(&mut store, Instant::now()).unwrap();
        assert!(resends.is_empty(), "attempt 3 超上限：不再重发");
        assert_eq!(failed, vec![msg.client_msg_id]);
        assert!(outbox.is_empty());

        // 放弃也落盘：重开底层库，pending 应为空
        let mut store = LocalStore::open(dir.path()).unwrap();
        assert!(store.pending_all().unwrap().is_empty());
    }

    /// 重启恢复：未 Ack 的在途消息重开后仍在邮箱里，可被补发。
    #[test]
    fn reopen_restores_inflight() {
        let dir = TempDir::new();
        let client_msg_id = {
            let (mut store, mut outbox) = outbox(&dir, 50, 3);
            let msg = outbox
                .enqueue(&mut store, 2, Bytes::from_static(b"crash survivor"))
                .unwrap();
            drop(outbox); // 模拟进程退出（消息在途、未 Ack）
            msg.client_msg_id
        };

        let (mut store, mut outbox) = outbox(&dir, 50, 3);
        assert_eq!(outbox.len(), 1, "在途消息重启不丢");
        let resends = outbox.resends();
        assert_eq!(resends[0].client_msg_id, client_msg_id);
        assert_eq!(resends[0].content, Bytes::from_static(b"crash survivor"));
        // 顺手验证 store 借用没有被 drop 影响（借用式 API 无所有权陷阱）
        assert_eq!(store.sync_cursor().unwrap(), 0);
    }

    /// 退避封顶：attempts 很大时延迟不超过 CAP。
    #[test]
    fn backoff_delay_is_capped() {
        let rto = Duration::from_secs(5);
        assert_eq!(backoff_delay(rto, 1), Duration::from_secs(5));
        assert_eq!(backoff_delay(rto, 2), Duration::from_secs(10));
        // 2^16 × 5s 远超 30s 封顶
        assert_eq!(backoff_delay(rto, 30), RETRY_DELAY_CAP);
    }
}
