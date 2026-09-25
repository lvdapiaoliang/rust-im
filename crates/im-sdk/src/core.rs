//! SDK 核心层：把 `im-client` 的异步内核封装成**同步、可跨语言**的对象。
//!
//! 分层职责（外观模式：FFI 是门面，本模块是 subsystem）：
//!
//! ```text
//!   C / Java / 任意语言
//!        │  句柄（*mut SdkClient）+ i32 返回码
//!   ┌────▼─────────┐
//!   │   ffi.rs      │  C ABI：#[no_mangle] extern "C"，指针判空、内存契约、
//!   │              │  事件泵线程（unsafe 的唯一集中区）
//!   ├──────────────┤
//!   │  core.rs ←── 本模块：SdkClient（同步外观）+ 事件编组 —— 全安全代码
//!   ├──────────────┤
//!   │  im-client    │  异步内核：连接状态机/重传/离线同步
//!   └──────────────┘
//! ```
//!
//! # 三条设计主线（docs/17 的核心）
//!
//! 1. **异步→同步**：`im-client` 的 API 是 async 的，C 语言没有 await。
//!    每个 SDK 客户端自带一个专属 tokio Runtime，`block_on` 桥接；
//!    事件经**事件泵线程**（回调模式，ffi.rs）或 **poll**（轮询模式）送出。
//! 2. **内存契约**：C 侧永远只拿「不透明指针 + 拷贝进来的入参」；
//!    SDK 分配的事件结构由 ffi 层的 `im_sdk_event_free` 回收。
//! 3. **线程契约**：句柄可在任意线程间传递（内部全部是线程安全类型），
//!    但 `destroy` 不得与其他调用并发——destroy 即所有权归还。

use std::collections::VecDeque;
use std::path::PathBuf;

use bytes::Bytes;
use im_client::{ClientConfig, ClientEvent, ClientHandle};
use im_transport::ShutdownTx;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use crate::error;

/// C 可见的事件类型码（`#[repr(C)]` 事件的判别字段）。
///
/// 数值稳定（跨语言 switch 依赖它）；`SyncBatch` **不在**其中——
/// 批量事件在 [`normalize`] 里展开成逐条 `Message`（见该函数注释）。
pub const EVENT_CONNECTED: i32 = 0;
pub const EVENT_DISCONNECTED: i32 = 1;
pub const EVENT_MESSAGE: i32 = 2;
pub const EVENT_MESSAGE_QUEUED: i32 = 3;
pub const EVENT_ACK: i32 = 4;
pub const EVENT_REJECTED: i32 = 5;
pub const EVENT_SEND_FAILED: i32 = 6;

/// C 侧拿到的事件结构（SDK 分配，ffi 层负责转裸指针与回收）。
///
/// 所有变体共用一个「宽结构」而不是 tagged union：跨语言 tag-union
/// 要么每字段都是指针（间接一层），要么变体各自一个 struct（C 侧 switch
/// 判别麻烦）。宽结构浪费几个字节，换来「一个 free 函数管所有变体」——
/// 内存契约越简单，泄漏/悬垂越难写出来。
///
/// `data` 的语义随 `type_` 变化（content / reason），`data_len == 0` 时空指针。
/// **注意**：`data` 是裸指针，`ImSdkEvent` 被 drop 时**不会**自动回收它——
/// 唯一的回收路径是 ffi 层的 `free_event`（重建 `Box<[u8]>`）。
#[repr(C)]
pub struct ImSdkEvent {
    /// 事件类型码（[`EVENT_CONNECTED`] 等）。
    pub type_: i32,
    /// 服务端会话 ID（`Connected`）。
    pub session_id: u64,
    /// 服务端全局消息 ID（`Message` / `Ack`）。
    pub msg_id: u64,
    /// 客户端去重键（`Message` / `MessageQueued` / `Ack` / `SendFailed`）。
    pub client_msg_id: u64,
    /// 发送者用户 ID（`Message`）。
    pub from: u64,
    /// 接收者用户 ID（`Message` / `MessageQueued`）。
    pub to: u64,
    /// 变体数据（SDK 分配；空内容为 null）：`Message`/`MessageQueued` 的
    /// 消息内容、`Rejected` 的拒绝原因（UTF-8 字节）。
    pub data: *mut u8,
    /// `data` 的字节数。
    pub data_len: usize,
}

/// 事件回调：SDK 事件泵线程每收到一条事件调用一次（类型定义在 core，
/// 喂给它的线程与回收逻辑在 ffi——回调语义天然属于边界层）。
///
/// `event` 指针及其 `data` **只在回调返回前有效**（回调返回后 SDK 立即
/// 回收）——要保留就当场拷贝。这个「作用域即契约」把 C 侧的 free 义务
/// 从回调路径上完全拿掉；poll 路径则必须显式 `im_sdk_event_free`。
pub type EventCallback = extern "C" fn(event: *mut ImSdkEvent, user_data: *mut core::ffi::c_void);

/// SDK 客户端：`im-client` 的同步外观。
///
/// 字段全部是线程安全类型（`Runtime`/`ClientHandle`/`ShutdownTx` 都可跨线程），
/// 因此**句柄可跨线程传递**；但「`destroy` 与其他调用不得并发」是调用方义务
/// （见模块文档线程契约）。
pub struct SdkClient {
    /// 专属运行时：destroy 时 drop，等所有 task 落幕。
    runtime: Runtime,
    /// 发送句柄（克隆廉价；drop 后命令通道关闭）。
    handle: ClientHandle,
    /// 关停信号：destroy 的第一步是 trigger，让连接状态机自己收尾。
    shutdown: ShutdownTx,
    /// 事件接收端：回调模式下被事件泵线程**移走**（`None`）。
    events: Option<mpsc::Receiver<ClientEvent>>,
    /// 已从通道取出但还没交付的事件（`SyncBatch` 展开后的尾巴）。
    pending: VecDeque<ClientEvent>,
    /// 回调模式的事件泵线程（destroy 时先 join 再回收其他资源）。
    pump: Option<std::thread::JoinHandle<()>>,
}

/// `ClientEvent::SyncBatch(Vec<Msg>)` → 逐条 `Message`。
///
/// C 的事件模型是「一次一个」，批量对 C 只能是「连续多次回调/poll」。
/// 在核心层展开（而不是给 C 一个 batch 变体）：编组、free、文档口径
/// 全都只维护一种事件形状。顺序保持服务端给的 `msg_id` 升序。
#[must_use]
pub fn normalize(event: ClientEvent) -> VecDeque<ClientEvent> {
    match event {
        ClientEvent::SyncBatch(messages) => {
            messages.into_iter().map(ClientEvent::Message).collect()
        }
        other => VecDeque::from([other]),
    }
}

/// 创建 SDK 客户端：起专属运行时 + 启动连接状态机，立刻返回。
///
/// - `data_dir == None` → 进程唯一临时目录（每次 create 都换新目录：
///   适合演示/测试；重启不保留历史，长期使用请显指定目录）。
///
/// 回调模式的装配（取走事件接收端 + 起泵线程）在 ffi 层完成——
/// `take_events`/`attach_pump` 是它的两个装配点。
///
/// # Errors
/// 运行时创建失败（线程资源不足）时返回 [`error::ERR_INTERNAL`]。
pub fn create(
    server_addr: &str,
    user_id: u64,
    token: &str,
    data_dir: Option<&str>,
) -> Result<SdkClient, i32> {
    // 编组在先、启动在后：任何入参问题都不留半启动的烂摊子
    let config = ClientConfig {
        server_addr: server_addr.to_owned(),
        user_id,
        token: token.to_owned(),
        data_dir: data_dir.map(PathBuf::from),
        ..ClientConfig::new("", 0, "")
    };

    let runtime = Runtime::new().map_err(|_| error::ERR_INTERNAL)?;
    let (events_tx, events_rx) = mpsc::channel::<ClientEvent>(64);
    let (shutdown_tx, shutdown_rx) = im_transport::shutdown_channel();

    // run_client 内部 tokio::spawn：必须在运行时上下文里调用
    let handle =
        runtime.block_on(async { im_client::run_client(config, events_tx, shutdown_rx).await });

    Ok(SdkClient {
        runtime,
        handle,
        shutdown: shutdown_tx,
        events: Some(events_rx),
        pending: VecDeque::new(),
        pump: None,
    })
}

impl SdkClient {
    /// 发一条消息：`Ok` 只表示命令已入队（断线排队、送达以 `Ack` 事件为准）。
    ///
    /// # Errors
    /// 客户端已退出（握手被拒后/destroy 后）返回 [`error::ERR_STOPPED`]。
    pub fn send(&self, to: u64, content: &[u8]) -> Result<(), i32> {
        // 入参立即拷贝进 Bytes：C 指针的寿命到本函数返回为止
        self.runtime
            .block_on(self.handle.send_msg(to, Bytes::copy_from_slice(content)))
            .map_err(|_| error::ERR_STOPPED)
    }

    /// 等待下一条事件（轮询模式专用）。
    ///
    /// `Ok(Some(event))` = 拿到一条**已编组的**事件（按值返回；交给 C 前
    /// 由 ffi 层装箱成裸指针）；
    /// `Ok(None)` = 超时（正常路径，调用方继续轮询即可）；
    /// `Err(ERR_STOPPED)` = 客户端已退出（通道关闭）；
    /// `Err(ERR_POLL_WITH_CALLBACK)` = 回调模式下误用 poll。
    ///
    /// # Errors
    /// 见上；另见 [`crate::error`] 全表。
    pub fn poll_event(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<ImSdkEvent>, i32> {
        let Some(events) = self.events.as_mut() else {
            return Err(error::ERR_POLL_WITH_CALLBACK);
        };

        // 先吐上次展开的尾巴（SyncBatch 的第 2..=n 条），再收新事件
        let event = if let Some(head) = self.pending.pop_front() {
            head
        } else {
            // 参数名 timeout 遮蔽了 tokio::time::timeout——全限定调用消歧
            match self
                .runtime
                .block_on(async { tokio::time::timeout(timeout, events.recv()).await })
            {
                // 通道里来了新事件：先展开再交付第一条
                Ok(Some(event)) => {
                    let mut expanded = normalize(event);
                    // VecDeque 尾部进、头部出：保序
                    self.pending.append(&mut expanded);
                    self.pending
                        .pop_front()
                        .expect("normalize 对非空输入至少产出一条")
                }
                // 超时：不是错误，是「暂时没有事件」
                Ok(None) => return Ok(None),
                // 通道关闭：状态机已落幕
                Err(_) => return Err(error::ERR_STOPPED),
            }
        };
        Ok(Some(alloc_event(event)))
    }

    /// 回调模式装配点 1：把事件接收端**移交**给事件泵线程。
    ///
    /// 只能取一次（第二次返回 None——接收端不可克隆，两条消费路径互斥
    /// 正是 poll/回调互斥的物化）。
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<ClientEvent>> {
        self.events.take()
    }

    /// 回调模式装配点 2：挂载事件泵线程句柄（destroy 时负责 join）。
    pub fn attach_pump(&mut self, pump: std::thread::JoinHandle<()>) {
        self.pump = Some(pump);
    }

    /// 关停：trigger shutdown → join 事件泵 → drop 运行时（等 task 落幕）。
    ///
    /// 顺序是死规矩：**先让状态机自己收尾**（它 drop 事件发送端，事件泵
    /// 才会退出），再 join 泵线程，最后才 drop 运行时——运行时还活着，
    /// 状态机才有地方跑完收尾逻辑。颠倒顺序 = 死锁或半死状态。
    pub fn destroy(mut self) {
        self.shutdown.trigger();
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
        // 顺序到此归一：无人再使用运行时，drop 只是等 task 结束
        drop(self.runtime);
    }
}

/// 把一条 `ClientEvent` 编组成 C 事件结构（**按值**返回；裸指针化在 ffi 层）。
///
/// `data` 的分配用 `Box<[u8]>` → 裸指针：回收时凭 `(ptr, len)` 就能无损重建
/// （`Vec` 还需要 capacity，跨 FFI 没法廉价传——这是「跨边界只传最小充分
/// 信息」的一课）。`Box::into_raw` 本身是安全操作，所以编组是全安全代码；
/// 不安全的部分（凭裸指针重建）集中在 ffi 层的 `free_event`。
#[must_use]
pub fn alloc_event(event: ClientEvent) -> ImSdkEvent {
    let (type_, session_id, msg_id, client_msg_id, from, to, data) = match event {
        ClientEvent::Connected { session_id } => {
            (EVENT_CONNECTED, session_id, 0, 0, 0, 0, Vec::new())
        }
        ClientEvent::Disconnected => {
            (EVENT_DISCONNECTED, 0, 0, 0, 0, 0, Vec::new())
        }
        ClientEvent::Message(im_protocol::Msg { from, to, msg_id, client_msg_id, content }) => {
            (EVENT_MESSAGE, 0, msg_id, client_msg_id, from, to, content.to_vec())
        }
        ClientEvent::MessageQueued { client_msg_id, to, content } => {
            (EVENT_MESSAGE_QUEUED, 0, 0, client_msg_id, 0, to, content.to_vec())
        }
        ClientEvent::Ack { msg_id, client_msg_id } => {
            (EVENT_ACK, 0, msg_id, client_msg_id, 0, 0, Vec::new())
        }
        ClientEvent::SyncBatch(_) => {
            // normalize 已在所有入口展开；直接编组等于程序性错误，宁可显式炸
            unreachable!("SyncBatch 必须先经 normalize 展开")
        }
        ClientEvent::Rejected { reason } => {
            (EVENT_REJECTED, 0, 0, 0, 0, 0, reason.into_bytes())
        }
        ClientEvent::SendFailed { client_msg_id } => {
            (EVENT_SEND_FAILED, 0, 0, client_msg_id, 0, 0, Vec::new())
        }
    };

    let (data, data_len) = if data.is_empty() {
        (std::ptr::null_mut(), 0)
    } else {
        // Box<[u8]> → 裸指针：回收侧凭 (ptr, len) 重建，见 ffi::free_event
        let boxed = data.into_boxed_slice();
        let len = boxed.len();
        (Box::into_raw(boxed).cast::<u8>(), len)
    };

    ImSdkEvent {
        type_,
        session_id,
        msg_id,
        client_msg_id,
        from,
        to,
        data,
        data_len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error;
    use im_server::{AllowAll, SessionConfig, StaticToken};
    use std::sync::Arc;
    use std::time::Duration;

    /// 起一个真实会话核心（与 im-bench 压测同款入口）。
    async fn spawn_test_server(config: SessionConfig) -> String {
        let (addr, _sessions, _shutdown) =
            im_server::spawn_server(config).await.expect("测试服务端应能启动");
        addr.to_string()
    }

    /// 拿到事件后安全地读字段（测试侧的「回调」就是它）。
    fn borrow(ev: &ImSdkEvent) -> (i32, Vec<u8>) {
        let data = if ev.data.is_null() {
            Vec::new()
        } else {
            // SAFETY: alloc_event 契约——(data, data_len) 是有效的配对区间
            unsafe { std::slice::from_raw_parts(ev.data, ev.data_len) }.to_vec()
        };
        (ev.type_, data)
    }

    /// poll 全流程（双客户端互发）：编组正确性 + 守恒。
    #[tokio::test]
    async fn poll_roundtrip_delivers_message_between_two_clients() {
        let addr = spawn_test_server(SessionConfig::default()).await;

        let mut alice = create(&addr, 1, "t", None).unwrap();
        let mut bob = create(&addr, 2, "t", None).unwrap();

        // 两端都握手成功
        for client in [&mut alice, &mut bob] {
            let ev = client.poll_event(Duration::from_secs(5)).unwrap().unwrap();
            let (ty, _) = borrow(&ev);
            assert_eq!(ty, EVENT_CONNECTED);
            assert!(ev.session_id > 0);
        }

        // Bob → Alice 一条消息：Bob 侧看到 Queued + Ack，Alice 侧看到 Message
        bob.send(1, b"hello ffi").unwrap();

        let alice_ev = alice.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        let bob_queued = bob.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        let bob_ack = bob.poll_event(Duration::from_secs(5)).unwrap().unwrap();

        assert_eq!(borrow(&alice_ev), (EVENT_MESSAGE, b"hello ffi".to_vec()));
        assert_eq!(borrow(&bob_queued), (EVENT_MESSAGE_QUEUED, b"hello ffi".to_vec()));
        assert_eq!(borrow(&bob_ack).0, EVENT_ACK);

        alice.destroy();
        bob.destroy();
    }

    /// 离线消息 → SyncBatch → normalize 展开成逐条 Message（保序）。
    #[tokio::test]
    async fn sync_batch_expands_into_individual_messages() {
        let addr = spawn_test_server(SessionConfig::default()).await;

        // Alice 先上、发两条给离线的 Bob、确认送达、下线
        let mut alice = create(&addr, 1, "t", None).unwrap();
        let (ty, _) = borrow(&alice.poll_event(Duration::from_secs(5)).unwrap().unwrap());
        assert_eq!(ty, EVENT_CONNECTED);
        alice.send(2, b"first").unwrap();
        alice.send(2, b"second").unwrap();

        // 等 Alice 收到两个 Ack（服务端已托管两条消息）
        let mut acks = 0;
        while acks < 2 {
            let ev = alice.poll_event(Duration::from_secs(5)).unwrap().unwrap();
            if borrow(&ev).0 == EVENT_ACK {
                acks += 1;
            }
        }
        alice.destroy();

        // Bob 上线：Connected 之后应把离线的两条逐条收全
        let mut bob = create(&addr, 2, "t", None).unwrap();
        let (ty, _) = borrow(&bob.poll_event(Duration::from_secs(5)).unwrap().unwrap());
        assert_eq!(ty, EVENT_CONNECTED);

        let mut got = Vec::new();
        while got.len() < 2 {
            let ev = bob.poll_event(Duration::from_secs(5)).unwrap().unwrap();
            let (ty, data) = borrow(&ev);
            if ty == EVENT_MESSAGE {
                got.push(data);
            }
        }
        // 服务端按 msg_id 升序补投：展开后的顺序必须保持
        assert_eq!(got, vec![b"first".to_vec(), b"second".to_vec()]);
        bob.destroy();
    }

    /// 握手被拒：Rejected 事件携带 reason 数据；此后 send 返回 ERR_STOPPED。
    #[tokio::test]
    async fn rejected_handshake_yields_event_then_stopped() {
        let addr = spawn_test_server(SessionConfig {
            authenticator: Arc::new(StaticToken { token: "right".to_string() }),
            ..SessionConfig::default()
        })
        .await;

        let mut client = create(&addr, 1, "WRONG", None).unwrap();
        let ev = client.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        let (ty, reason) = borrow(&ev);
        assert_eq!(ty, EVENT_REJECTED);
        assert!(!reason.is_empty(), "拒绝原因不该是空串");

        // 状态机已落幕：命令通道的接收端没了
        assert_eq!(client.send(2, b"x"), Err(error::ERR_STOPPED));
        // 事件通道也已关闭
        assert!(matches!(
            client.poll_event(Duration::from_secs(1)),
            Err(error::ERR_STOPPED)
        ));
        client.destroy();
    }

    /// 「上线后立刻销毁」：destroy 必须在有限时间内返回（不挂死）。
    #[tokio::test]
    async fn destroy_shuts_down_cleanly_without_hanging() {
        let addr = spawn_test_server(SessionConfig {
            authenticator: Arc::new(AllowAll),
            ..SessionConfig::default()
        })
        .await;

        let mut client = create(&addr, 1, "t", None).unwrap();
        let (ty, _) = borrow(&client.poll_event(Duration::from_secs(5)).unwrap().unwrap());
        assert_eq!(ty, EVENT_CONNECTED);
        // 立刻销毁：超时由测试框架兜底（挂死即失败）
        client.destroy();
    }

    /// 空内容事件不分配 data 指针（null + 0）。
    #[tokio::test]
    async fn empty_payload_events_have_null_data() {
        let addr = spawn_test_server(SessionConfig::default()).await;

        let mut client = create(&addr, 1, "t", None).unwrap();
        let ev = client.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(borrow(&ev).0, EVENT_CONNECTED);
        assert!(ev.data.is_null());
        assert_eq!(ev.data_len, 0);
        client.destroy();
    }
}
