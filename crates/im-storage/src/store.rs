//! `LocalStore`：IM 语义层（阶段 4）。
//!
//! 在 [`crate::engine`] 的裸 KV 之上定义聊天数据的 key 布局与访问语义：
//!
//! ```text
//! m/{peer}/{msg_id:020}    正式消息（msg_id 零填充定宽 → 字典序 = 数值序）
//! p/{client_msg_id:020}    发送中（pending，未 Ack 的重发表）
//! c/sync_cursor            离线同步游标
//! c/client_seq             client_msg_id 分配计数器（重启不回退）
//! ```
//!
//! # 为什么要 key 前缀设计？
//!
//! LSM/Bigtable 系存储只有「按 key 有序扫描」一种批量读取原语——
//! 把同类数据的 key 设计成共享前缀，`scan_prefix` 就是一张「表」。
//! 对照关系型数据库的二级索引：`m/{peer}/...` 前缀天然就是
//! 「按会话查消息」的索引（写时冗余，读时免 join）。
//!
//! # 消息生命周期（状态机）
//!
//! ```text
//! enqueue_outgoing ──▶ p/…（发送中，UI 显示「转圈」）
//!        │  Ack 到达：ack_outgoing(client_msg_id, msg_id)
//!        ▼
//!      m/…（转正：删除 pending，写入正式消息）
//!
//! append_incoming ──▶ m/…（直接入库，去重由调用方完成）
//! ```

use bytes::Bytes;
use im_protocol::{Msg, Payload};

use crate::engine::Engine;
use crate::error::StorageError;

/// key 前缀：正式消息。
const PREFIX_MESSAGE: &[u8] = b"m/";
/// key 前缀：发送中的重发表。
const PREFIX_PENDING: &[u8] = b"p/";
/// key：离线同步游标。
const KEY_SYNC_CURSOR: &[u8] = b"c/sync_cursor";
/// key：client_msg_id 分配计数器。
const KEY_CLIENT_SEQ: &[u8] = b"c/client_seq";

/// 一条发送中的消息（重发表条目）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMsg {
    /// 发送方本地去重键（重发不变）。
    pub client_msg_id: u64,
    /// 接收者。
    pub to: u64,
    /// 消息内容。
    pub content: Bytes,
    /// 已重试次数（退避指数的输入）。
    pub attempts: u32,
}

impl PendingMsg {
    /// varint 序列化（与协议载荷同风格：小整数 1 字节）。
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        im_protocol::varint::encode_u64(self.client_msg_id, &mut out);
        im_protocol::varint::encode_u64(self.to, &mut out);
        im_protocol::varint::encode_u64(self.attempts as u64, &mut out);
        im_protocol::varint::encode_u64(self.content.len() as u64, &mut out);
        out.extend_from_slice(&self.content);
        out
    }

    fn decode(src: &[u8]) -> Result<Self, StorageError> {
        let mut cursor: &[u8] = src;
        // 闭包借住 cursor 逐段推进；返回类型显式标注（多个 From 实现使推断失效）
        let mut read_varint = || -> Result<u64, StorageError> {
            let (value, used) =
                im_protocol::varint::decode_u64(cursor).ok_or(StorageError::Corrupted)?;
            cursor = &cursor[used..];
            Ok(value)
        };
        let client_msg_id = read_varint()?;
        let to = read_varint()?;
        let attempts = u32::try_from(read_varint()?).map_err(|_| StorageError::Corrupted)?;
        let len = usize::try_from(read_varint()?).map_err(|_| StorageError::Corrupted)?;
        if cursor.len() != len {
            return Err(StorageError::Corrupted);
        }
        Ok(Self {
            client_msg_id,
            to,
            attempts,
            content: Bytes::copy_from_slice(cursor),
        })
    }
}

/// 客户端本地消息库。
///
/// 所有方法同步（非 async）：本地追加写在 SSD 上是微秒级，
/// 引入 `spawn_blocking` 的复杂度不值得——「不是所有 IO 都该 async」。
pub struct LocalStore {
    engine: Engine,
}

impl LocalStore {
    /// 打开（或创建）位于 `dir` 的本地库。
    ///
    /// # Errors
    ///
    /// 见 [`Engine::open`]。
    pub fn open(dir: impl AsRef<std::path::Path>) -> Result<Self, StorageError> {
        Ok(Self {
            engine: Engine::open(dir)?,
        })
    }

    /// 收到的消息入库（去重由调用方负责——存储层不解释业务键的重复）。
    ///
    /// 同一 `msg_id` 重复入库是幂等的：同 key 覆盖写，LSM 天然去重。
    ///
    /// # Errors
    ///
    /// 磁盘写失败时返回 [`StorageError::Io`]。
    pub fn append_incoming(&mut self, msg: &Msg) -> Result<(), StorageError> {
        let key = message_key(msg.from, msg.msg_id);
        self.engine.put(&key, &msg.encode())
    }

    /// 登记一条待发送消息（分配新的 `client_msg_id` 并持久化计数器——
    /// 重启后 ID 继续递增，绝不复用，接收端去重才可靠）。
    ///
    /// # Errors
    ///
    /// 磁盘写失败时返回 [`StorageError::Io`]。
    pub fn enqueue_outgoing(&mut self, to: u64, content: &[u8]) -> Result<u64, StorageError> {
        let client_msg_id = self.next_client_msg_id()?;
        let pending = PendingMsg {
            client_msg_id,
            to,
            content: Bytes::copy_from_slice(content),
            attempts: 0,
        };
        self.engine.put(&pending_key(client_msg_id), &pending.encode())?;
        Ok(client_msg_id)
    }

    /// Ack 到达：pending 转正——删重发表条目，写正式消息。
    ///
    /// `content` 由调用方携带（发送方本地还有原文；存储层也可回读 pending，
    /// 把内容传进来是为了让「发送→转正」的数据流在调用方一目了然）。
    ///
    /// # Errors
    ///
    /// 磁盘写失败时返回 [`StorageError::Io`]。
    pub fn ack_outgoing(
        &mut self,
        client_msg_id: u64,
        msg_id: u64,
        from_me: u64,
        to: u64,
        content: &[u8],
    ) -> Result<(), StorageError> {
        self.engine.delete(&pending_key(client_msg_id))?;
        let msg = Msg {
            from: from_me,
            to,
            msg_id,
            client_msg_id,
            content: Bytes::copy_from_slice(content),
        };
        // 我发的消息挂在对方的会话里（history(peer) 按对端聚合）
        self.engine.put(&message_key(to, msg_id), &msg.encode())
    }

    /// 全部待发送消息（重启后恢复重发表）。
    ///
    /// # Errors
    ///
    /// 段文件读取失败时返回 [`StorageError::Io`]。
    pub fn pending_all(&mut self) -> Result<Vec<PendingMsg>, StorageError> {
        let mut out: Vec<PendingMsg> = Vec::new();
        for (_, value) in self.engine.scan_prefix(PREFIX_PENDING)? {
            out.push(PendingMsg::decode(&value)?);
        }
        Ok(out)
    }

    /// 更新重试次数（持久化，重启后退避不从零开始）。
    ///
    /// # Errors
    ///
    /// 磁盘写失败时返回 [`StorageError::Io`]。
    pub fn pending_set_attempts(
        &mut self,
        client_msg_id: u64,
        attempts: u32,
    ) -> Result<(), StorageError> {
        let key = pending_key(client_msg_id);
        let Some(value) = self.engine.get(&key)? else {
            return Ok(()); // 已被 Ack 核销：无更新对象，静默成功
        };
        let mut pending = PendingMsg::decode(&value)?;
        pending.attempts = attempts;
        self.engine.put(&key, &pending.encode())
    }

    /// 放弃一条在途消息（超过最大重试次数）：从重发表彻底移除，
    /// 否则重启后它又会被补发一遍。
    ///
    /// # Errors
    ///
    /// 磁盘写失败时返回 [`StorageError::Io`]。
    pub fn drop_outgoing(&mut self, client_msg_id: u64) -> Result<(), StorageError> {
        self.engine.delete(&pending_key(client_msg_id))
    }

    /// 某会话的最近 `limit` 条消息（`msg_id` 升序）。
    ///
    /// 当前实现取「scan 全量 + 尾部截取」：本地单会话万级消息下
    /// 完全够用；压实段有序后可演进为二分定位（见 engine 模块文档）。
    ///
    /// # Errors
    ///
    /// 段文件读取失败或消息解码失败时返回 [`StorageError`]。
    pub fn history(&mut self, peer: u64, limit: usize) -> Result<Vec<Msg>, StorageError> {
        let prefix = history_prefix(peer);
        let all = self.engine.scan_prefix(&prefix)?;
        let start = all.len().saturating_sub(limit);
        all[start..]
            .iter()
            .map(|(_, value)| Msg::decode(value).map_err(StorageError::from))
            .collect()
    }

    /// 离线同步游标（上次收到的最大 `msg_id`，0 = 从头拉）。
    ///
    /// # Errors
    ///
    /// 存储读取失败时返回 [`StorageError::Io`]。
    pub fn sync_cursor(&mut self) -> Result<u64, StorageError> {
        Ok(self
            .engine
            .get(KEY_SYNC_CURSOR)?
            .map_or(0, decode_u64_value))
    }

    /// 更新同步游标（单调：只增不减）。
    ///
    /// # Errors
    ///
    /// 磁盘写失败时返回 [`StorageError::Io`]。
    pub fn set_sync_cursor(&mut self, cursor: u64) -> Result<(), StorageError> {
        self.engine.put(KEY_SYNC_CURSOR, &cursor.to_be_bytes())
    }

    /// 压实存储（见 [`Engine::compact`]）。
    ///
    /// # Errors
    ///
    /// 见 [`Engine::compact`]。
    pub fn compact(&mut self) -> Result<(), StorageError> {
        self.engine.compact()
    }

    /// 分配并持久化下一个 `client_msg_id`。
    fn next_client_msg_id(&mut self) -> Result<u64, StorageError> {
        let prev = self.engine.get(KEY_CLIENT_SEQ)?.map_or(0, decode_u64_value);
        let next = prev + 1;
        self.engine.put(KEY_CLIENT_SEQ, &next.to_be_bytes())?;
        Ok(next)
    }
}

/// 正式消息 key：`m/{peer}/{msg_id:020}`。
fn message_key(peer: u64, msg_id: u64) -> Vec<u8> {
    let mut key = history_prefix(peer);
    key.extend_from_slice(format!("{msg_id:020}").as_bytes());
    key
}

/// 会话历史前缀：`m/{peer}/`。
fn history_prefix(peer: u64) -> Vec<u8> {
    let mut key = PREFIX_MESSAGE.to_vec();
    key.extend_from_slice(peer.to_string().as_bytes());
    key.push(b'/');
    key
}

/// pending key：`p/{client_msg_id:020}`。
fn pending_key(client_msg_id: u64) -> Vec<u8> {
    let mut key = PREFIX_PENDING.to_vec();
    key.extend_from_slice(format!("{client_msg_id:020}").as_bytes());
    key
}

/// 存储里的 u64 值（大端定宽 8 字节）。
fn decode_u64_value(value: Vec<u8>) -> u64 {
    let bytes: [u8; 8] = value.as_slice().try_into().expect("游标值定宽 8 字节");
    u64::from_be_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试目录 guard（与 engine 测试同款）。
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "im-store-test-{}-{}",
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

    /// 收到的消息入库 → 历史按 msg_id 升序、按会话隔离。
    #[test]
    fn incoming_history_is_per_peer_and_ordered() {
        let dir = TempDir::new();
        let mut store = LocalStore::open(dir.path()).unwrap();
        for msg_id in [30u64, 10, 20] {
            store
                .append_incoming(&Msg {
                    from: 7,
                    to: 1,
                    msg_id,
                    client_msg_id: msg_id,
                    content: Bytes::from(format!("m{msg_id}")),
                })
                .unwrap();
        }
        store
            .append_incoming(&Msg {
                from: 8,
                to: 1,
                msg_id: 99,
                client_msg_id: 99,
                content: Bytes::from_static(b"other peer"),
            })
            .unwrap();

        let history = store.history(7, 10).unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].msg_id, 10, "升序");
        assert_eq!(history[2].msg_id, 30);
        // 只含 peer=7 的消息
        assert!(history.iter().all(|m| m.from == 7));

        // limit：最近 2 条
        let recent = store.history(7, 2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].msg_id, 20);
    }

    /// 发送生命周期：enqueue → pending 可见 → ack 转正 → history 可见。
    #[test]
    fn outgoing_lifecycle_pending_to_confirmed() {
        let dir = TempDir::new();
        let mut store = LocalStore::open(dir.path()).unwrap();

        let cid = store.enqueue_outgoing(2, b"hello").unwrap();
        assert_eq!(cid, 1, "首个本地 ID 从 1 开始");
        let pending = store.pending_all().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].content, Bytes::from_static(b"hello"));
        assert_eq!(pending[0].to, 2);

        store.ack_outgoing(cid, 500, 1, 2, b"hello").unwrap();
        assert!(store.pending_all().unwrap().is_empty(), "Ack 核销");

        let history = store.history(2, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].msg_id, 500);
        assert_eq!(history[0].client_msg_id, cid);
        assert_eq!(history[0].content, Bytes::from_static(b"hello"));
    }

    /// 重启恢复：pending、游标、client_seq 全部持久化。
    #[test]
    fn reopen_restores_pending_cursor_and_seq() {
        let dir = TempDir::new();
        {
            let mut store = LocalStore::open(dir.path()).unwrap();
            store.enqueue_outgoing(2, b"queued").unwrap();
            store.set_sync_cursor(12345).unwrap();
        }
        let mut store = LocalStore::open(dir.path()).unwrap();
        assert_eq!(store.sync_cursor().unwrap(), 12345);
        let pending = store.pending_all().unwrap();
        assert_eq!(pending.len(), 1, "断线排队的消息重启不丢");
        assert_eq!(pending[0].content, Bytes::from_static(b"queued"));

        // 重启后 client_msg_id 继续递增（不复用）
        let next = store.enqueue_outgoing(2, b"again").unwrap();
        assert_eq!(next, 2);
    }

    /// 重试计数持久化；核销后再 set_attempts 是无害的 no-op。
    #[test]
    fn attempts_persist_and_noop_after_ack() {
        let dir = TempDir::new();
        let mut store = LocalStore::open(dir.path()).unwrap();
        let cid = store.enqueue_outgoing(3, b"retry me").unwrap();

        store.pending_set_attempts(cid, 4).unwrap();
        assert_eq!(store.pending_all().unwrap()[0].attempts, 4);

        store.ack_outgoing(cid, 900, 1, 3, b"retry me").unwrap();
        store.pending_set_attempts(cid, 9).unwrap(); // 已核销：静默 no-op
        assert!(store.pending_all().unwrap().is_empty());
    }

    /// 压实后历史与游标完好。
    #[test]
    fn compact_preserves_semantic_data() {
        let dir = TempDir::new();
        let mut store = LocalStore::open(dir.path()).unwrap();
        for i in 0..10u64 {
            store
                .append_incoming(&Msg {
                    from: 5,
                    to: 1,
                    msg_id: i,
                    client_msg_id: i,
                    content: Bytes::from(format!("c{i}")),
                })
                .unwrap();
        }
        store.set_sync_cursor(9).unwrap();
        store.compact().unwrap();

        assert_eq!(store.history(5, 100).unwrap().len(), 10);
        assert_eq!(store.sync_cursor().unwrap(), 9);
    }
}
