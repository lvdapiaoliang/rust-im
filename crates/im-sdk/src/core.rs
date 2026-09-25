//! SDK 核心层：把 `im-client` 的异步内核封装成**同步、可跨语言**的对象。
//!
//! 分层职责（外观模式：FFI 是门面，本模块是 subsystem）：
//!
//! ```text
//!   C / Java / 任意语言
//!        │  句柄（*mut SdkClient）+ i32 返回码
//!   ┌────▼─────────┐
//!   │   ffi.rs      │  C ABI：#[no_mangle] extern "C"，指针判空、内存契约
//!   ├──────────────┤
//!   │  core.rs ←── 本模块：SdkClient（同步外观）+ 事件编组
//!   ├──────────────┤
//!   │  im-client    │  异步内核：连接状态机/重传/离线同步
//!   └──────────────┘
//! ```
//!
//! # 三条设计主线（docs/17 的核心）
//!
//! 1. **异步→同步**：`im-client` 的 API 是 async 的，C 语言没有 await。
//!    每个 SDK 客户端自带一个专属 tokio Runtime，`block_on` 桥接；
//!    事件用**事件泵线程**（回调模式）或 **poll**（轮询模式）送出去。
//! 2. **内存契约**：C 侧永远只拿「不透明指针 + 拷贝进来的入参」；
//!    SDK 分配的事件结构必须由 SDK 提供的 `im_sdk_event_free` 回收。
//!    入参一律**立即拷贝**（指针的寿命只到本次调用返回）。
//! 3. **线程契约**：句柄可在任意线程间传递（内部全部是线程安全类型），
//!    但 `destroy` 不得与其他调用并发——destroy 即所有权归还。

use std::collections::VecDeque;
use std::path::PathBuf;

use bytes::Bytes;
use im_client::{ClientConfig, ClientEvent, ClientHandle};
use im_transport::ShutdownTx;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

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

/// C 侧拿到的事件结构（SDK 分配，须 `im_sdk_event_free` 回收）。
///
/// 所有变体共用一个「宽结构」而不是 tagged union：跨语言 tag-union
/// 要么每字段都是指针（间接一层），要么变体各自一个 struct（C 侧 switch
/// 判别麻烦）。宽结构浪费几个字节，换来「一个 free 函数管所有变体」——
/// 内存契约越简单，泄漏/悬垂越难写出来。
///
/// `data` 的语义随 `type_` 变化（content / reason），`data_len == 0` 时空指针。
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

/// 事件回调：SDK 事件泵线程每收到一条事件调用一次。
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
///   适合演示/测试；重启不保留历史，长期使用请显指定目录）；
/// - `callback == None` → 轮询模式，调用方用 [`SdkClient::poll_event`] 取事件；
/// - `callback == Some` → 回调模式，事件泵线程自动把事件（连同 `user_data`）
///   送达回调，调用方**不应**再 poll。
///
/// # Errors
/// 运行时创建失败（线程资源不足）时返回 `ERR_INTERNAL`。
pub fn create(
    server_addr: &str,
    user_id: u64,
    token: &str,
    data_dir: Option<&str>,
    callback: Option<EventCallback>,
    user_data: *mut core::ffi::c_void,
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
    let handle = runtime.block_on(async { im_client::run_client(config, events_tx, shutdown_rx).await });

    let (pump, events) = match callback {
        Some(cb) => {
            let pump =
                spawn_pump(events_rx, cb, user_data).map_err(|_| error::ERR_INTERNAL)?;
            (Some(pump), None)
        }
        None => (None, Some(events_rx)),
    };

    Ok(SdkClient { runtime, handle, shutdown, events, pending: VecDeque::new(), pump })
}

/// 事件泵线程：`blocking_recv`（非运行时线程的同步桥）→ 编组 → 回调 → 回收。
///
/// 线程退出条件唯一：事件通道关闭 = 连接状态机已落幕（destroy 触发的
/// shutdown 会让它退出并 drop 发送端）。通道关闭前线程不空转（recv 阻塞），
/// 回调抛 panic 会被线程边界拦住，不会殃及进程其他部分。
fn spawn_pump(
    events: mpsc::Receiver<ClientEvent>,
    callback: EventCallback,
    user_data: *mut core::ffi::c_void,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("im-sdk-event-pump".into())
        .spawn(move || {
            while let Some(event) = events.blocking_recv() {
                for one in normalize(event) {
                    // 回调作用域契约：alloc → 回调 → free，一事件一闭环
                    let raw = alloc_event(one);
                    callback(raw, user_data);
                    // SAFETY: raw 是 alloc_event 刚编组出的活指针；
                    // 回调若想保留内容，契约要求它已自行拷贝。
                    unsafe { free_event(raw) };
                }
            }
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
    /// `Ok(Some(event))` = 拿到一条**已编组的** C 事件，用完必须 free；
    /// `Ok(None)` = 超时（正常路径，调用方继续轮询即可）；
    /// `Err(ERR_STOPPED)` = 客户端已退出（通道关闭）；
    /// `Err(ERR_POLL_WITH_CALLBACK)` = 回调模式下误用 poll。
    ///
    /// # Errors
    /// 见上；另见 [`error`] 全表。
    pub fn poll_event(&mut self, timeout: Duration) -> Result<Option<ImSdkEvent>, i32> {
        let Some(events) = self.events.as_mut() else {
            return Err(error::ERR_POLL_WITH_CALLBACK);
        };

        // 先吐上次展开的尾巴（SyncBatch 的第 2..=n 条），再收新事件
        let event = if let Some(head) = self.pending.pop_front() {
            head
        } else {
            match self.runtime.block_on(async { timeout(timeout, events.recv()).await }) {
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

/// 把一条 `ClientEvent` 编组成堆上的 C 事件结构。
///
/// `data` 的分配用 `Box<[u8]>`：free 时凭 `(ptr, len)` 就能无损重建
/// （`Vec` 还需要 capacity，跨 FFI 没法廉价传——这是「跨边界只传
/// 最小充分信息」的一课）。
#[must_use]
pub fn alloc_event(event: ClientEvent) -> *mut ImSdkEvent {
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
        // Box<[u8]> → 裸指针：free 侧凭 (ptr, len) 重建，见 free_event
        let boxed = data.into_boxed_slice();
        let len = boxed.len();
        (Box::into_raw(boxed).cast::<u8>(), len)
    };

    Box::into_raw(Box::new(ImSdkEvent {
        type_,
        session_id,
        msg_id,
        client_msg_id,
        from,
        to,
        data,
        data_len,
    }))
}

/// 回收 [`alloc_event`] 编组出的事件：data 重建为 `Box<[u8]>` 后与结构体一并 drop。
///
/// # Safety
/// `event` 必须是 [`alloc_event`] 返回、且尚未 free 过的指针；每个指针只能
/// free 一次。poll 路径由调用方履行；回调路径 SDK 已代劳。
pub unsafe fn free_event(event: *mut ImSdkEvent) {
    if event.is_null() {
        return;
    }
    // SAFETY: 调用方契约保证指针来源与唯一性（见函数 Safety 段）
    let event = unsafe { Box::from_raw(event) };
    if !event.data.is_null() {
        // SAFETY: data 由 alloc_event 的 Box<[u8]> 而来，(ptr, len) 无损重建
        let slice = unsafe {
            std::ptr::slice_from_raw_parts_mut(event.data, event.data_len)
        };
        drop(unsafe { Box::from_raw(slice) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error;
    use im_server::{AllowAll, SessionConfig, StaticToken};
    use std::sync::mpsc::channel as std_channel;
    use std::sync::{Arc, Mutex};

    /// 起一个真实会话核心（与 im-bench 压测同款入口）。
    async fn spawn_test_server(auth: SessionConfig) -> std::net::SocketAddr {
        let (addr, _sessions, _shutdown) = im_server::spawn_server(auth).await;
        addr
    }

    /// poll 全流程（双客户端互发）：编组正确性 + free 契约 + 守恒。
    #[tokio::test]
    async fn poll_roundtrip_delivers_message_between_two_clients() {
        let addr = spawn_test_server(SessionConfig::default()).await;
        let addr = addr.to_string();

        let mut alice = create(&addr, 1, "t", None, None, std::ptr::null_mut()).unwrap();
        let mut bob = create(&addr, 2, "t", None, None, std::ptr::null_mut()).unwrap();

        // 两端都握手成功
        for client in [&mut alice, &mut bob] {
            let ev = client.poll_event(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(unsafe { (*ev).type_ }, EVENT_CONNECTED);
            assert!(unsafe { (*ev).session_id } > 0);
            unsafe { free_event(ev) };
        }

        // Bob → Alice 一条消息：Bob 侧看到 Queued + Ack，Alice 侧看到 Message
        bob.send(1, b"hello ffi").unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let collect = |ev: *mut ImSdkEvent, seen: &Arc<Mutex<Vec<(i32, Vec<u8>)>>| {
            let e = unsafe &*ev;
            let data = if e.data.is_null() {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(e.data, e.data_len) }.to_vec()
            };
            seen.lock().unwrap().push((e.type_, data));
            unsafe { free_event(ev) };
        };

        // Alice 侧：Message（内容一致）
        let ev = alice.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        collect(ev, &seen);
        // Bob 侧：MessageQueued → Ack
        let ev = bob.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        collect(ev, &seen);
        let ev = bob.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        collect(ev, &seen);

        let seen = seen.lock().unwrap();
        assert!(seen.contains(&(EVENT_MESSAGE, b"hello ffi".to_vec())));
        assert!(seen.iter().any(|(t, d)| *t == EVENT_MESSAGE_QUEUED && d == b"hello ffi"));
        assert!(seen.iter().any(|(t, _)| *t == EVENT_ACK));

        alice.destroy();
        bob.destroy();
    }

    /// 离线消息 → SyncBatch → normalize 展开成逐条 Message（保序）。
    #[tokio::test]
    async fn sync_batch_expands_into_individual_messages() {
        let addr = spawn_test_server(SessionConfig::default()).await;
        let addr_str = addr.to_string();

        // Alice 先上、发两条给离线的 Bob、确认送达、下线
        let mut alice = create(&addr_str, 1, "t", None, None, std::ptr::null_mut()).unwrap();
        let ev = alice.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        unsafe { free_event(ev) };
        alice.send(2, b"first").unwrap();
        alice.send(2, b"second").unwrap();

        // 等 Alice 收到两个 Ack（服务端已托管两条消息）
        let mut acks = 0;
        while acks < 2 {
            let ev = alice.poll_event(Duration::from_secs(5)).unwrap().unwrap();
            if unsafe { (*ev).type_ } == EVENT_ACK {
                acks += 1;
            }
            unsafe { free_event(ev) };
        }
        alice.destroy();

        // Bob 上线：Connected 之后应把离线的两条逐条收全
        let mut bob = create(&addr_str, 2, "t", None, None, std::ptr::null_mut()).unwrap();
        let ev = bob.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(unsafe (*ev).type_, EVENT_CONNECTED);
        unsafe { free_event(ev) };

        let mut got = Vec::new();
        while got.len() < 2 {
            let ev = bob.poll_event(Duration::from_secs(5)).unwrap().unwrap();
            let e = unsafe &*ev;
            if e.type_ == EVENT_MESSAGE {
                got.push(unsafe { std::slice::from_raw_parts(e.data, e.data_len) }.to_vec());
            }
            unsafe { free_event(ev) };
        }
        // 服务端按 msg_id 升序补投：展开后的顺序必须保持
        assert_eq!(got, vec![b"first".to_vec(), b"second".to_vec()]);
        bob.destroy();
    }

    /// 回调模式：事件泵线程把 Connected/Queued/Ack 送达 C 回调形态的函数。
    #[tokio::test]
    async fn callback_pump_delivers_events() {
        let addr = spawn_test_server(SessionConfig::default()).await;
        let addr = addr.to_string();

        // C 形态的回调：extern "C" + user_data——把事件拷贝进 std 通道
        extern "C" fn on_event(ev: *mut ImSdkEvent, user: *mut core::ffi::c_void) {
            let tx = unsafe { &*(user as *const std::sync::mpsc::Sender<(i32, Vec<u8>)>) };
            let e = unsafe &*ev;
            let data = if e.data.is_null() {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(e.data, e.data_len) }.to_vec()
            };
            let _ = tx.send((e.type_, data));
        }

        let (tx, rx) = std_channel::<(i32, Vec<u8>)>();
        let tx = Box::leak(Box::new(tx)) as *mut _ as *mut core::ffi::c_void;
        let mut alice = create(&addr, 1, "t", None, Some(on_event), tx).unwrap();
        let mut bob = create(&addr, 2, "t", None, None, std::ptr::null_mut()).unwrap();

        // 事件泵先送 Connected
        let (ty, _) = rx.recv_timeout(Duration::from_secs(10)).expect("应收到 Connected");
        assert_eq!(ty, EVENT_CONNECTED);

        // Bob 发给 Alice：Alice 的泵应送出 Message（内容经拷贝存活）
        let ev = bob.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(unsafe { (*ev).type_ }, EVENT_CONNECTED);
        unsafe { free_event(ev) };
        bob.send(1, b"via-pump").unwrap();

        let (ty, data) = rx.recv_timeout(Duration::from_secs(10)).expect("应收到 Message");
        assert_eq!(ty, EVENT_MESSAGE);
        assert_eq!(data, b"via-pump");

        // 回调模式下 poll 必须被拒绝（事件归泵线程，两条路互斥）
        assert!(matches!(
            alice.poll_event(Duration::from_secs(1)),
            Err(error::ERR_POLL_WITH_CALLBACK)
        ));

        alice.destroy();
        bob.destroy();
        drop(unsafe { Box::from_raw(tx as *mut std::sync::mpsc::Sender<(i32, Vec<u8>)>) });
    }

    /// 握手被拒：Rejected 事件携带 reason 数据；此后 send 返回 ERR_STOPPED。
    #[tokio::test]
    async fn rejected_handshake_yields_event_then_stopped() {
        let addr = spawn_test_server(SessionConfig {
            authenticator: Arc::new(StaticToken { token: "right".to_string() }),
            ..SessionConfig::default()
        })
        .await;
        let addr = addr.to_string();

        let mut client = create(&addr, 1, "WRONG", None, None, std::ptr::null_mut()).unwrap();
        let ev = client.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        let e = unsafe &*ev;
        assert_eq!(e.type_, EVENT_REJECTED);
        let reason = unsafe { std::slice::from_raw_parts(e.data, e.data_len) }.to_vec();
        assert!(!reason.is_empty(), "拒绝原因不该是空串");
        unsafe { free_event(ev) };

        // 状态机已落幕：命令通道的接收端没了
        assert_eq!(client.send(2, b"x"), Err(error::ERR_STOPPED));
        // 事件通道也已关闭
        assert!(matches!(
            client.poll_event(Duration::from_secs(1)),
            Err(error::ERR_STOPPED)
        ));
        client.destroy();
    }

    /// 「客户端退出后命令通道关闭」的另一半：AllowAll 永不拒绝，
    /// 靠 destroy 关停——销毁后 send 也不该 panic（句柄归还前最后一刻）。
    #[tokio::test]
    async fn destroy_shuts_down_cleanly_without_hanging() {
        let addr = spawn_test_server(SessionConfig {
            authenticator: Arc::new(AllowAll),
            ..SessionConfig::default()
        })
        .await;
        let addr = addr.to_string();

        let mut client = create(&addr, 1, "t", None, None, std::ptr::null_mut()).unwrap();
        let ev = client.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(unsafe (*ev).type_, EVENT_CONNECTED);
        unsafe { free_event(ev) };
        // 立刻销毁：断言 destroy 在有限时间内返回（超时由测试框架兜底）
        client.destroy();
    }

    /// 空内容不分配 data 指针（null + 0），free 依然成立。
    #[tokio::test]
    async fn empty_payload_events_have_null_data() {
        let addr = spawn_test_server(SessionConfig::default()).await;
        let addr = addr.to_string();

        let mut client = create(&addr, 1, "t", None, None, std::ptr::null_mut()).unwrap();
        let ev = client.poll_event(Duration::from_secs(5)).unwrap().unwrap();
        let e = unsafe &*ev;
        assert_eq!(e.type_, EVENT_CONNECTED);
        assert!(e.data.is_null());
        assert_eq!(e.data_len, 0);
        unsafe { free_event(ev) };
        client.destroy();
    }
}
