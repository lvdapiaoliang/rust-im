//! C ABI 层：`#[no_mangle] extern "C"` 导出函数（门面模式的最外圈）。
//!
//! 本模块是**唯一**允许 `unsafe` 的地方（模块级豁免 `unsafe_code` 警告）：
//! unsafe 集中在边界上、每个块带 SAFETY 注释，是跨语言 SDK 的标准姿势——
//! 内核（`core.rs`）保持全安全代码，审查面就只剩这一个文件。
//!
//! # 调用方契约（头文件 `include/im_sdk.h` 同步这些条文）
//!
//! 1. **句柄**：所有 `im_sdk_client_t*` 只能来自 `im_sdk_client_create`；
//!    可跨线程传递，但 `destroy` 不得与其他调用并发（所有权语义）。
//! 2. **入参**：字符串/字节一律**立即拷贝**，指针寿命只到本次调用返回。
//! 3. **出参**：`poll` 拿到的事件必须 `im_sdk_event_free`（谁分配谁释放）；
//!    回调模式下 SDK 代劳（事件作用域限于回调内）。
//! 4. **错误**：返回码见 [`crate::error`]；说明文字来自 `im_sdk_error_string`
//!    （静态，不 free）。任何函数**不会**因入参问题 panic。
//! 5. **字符串编码**：UTF-8，无终止符依赖（长度显式传递；C 字符串入参
//!    以 `\0` 结尾，内容须为合法 UTF-8）。
//!
//! # panic 边界
//!
//! Rust 的 `extern "C"` 边界遇 panic 会 abort 进程（不是 UB）；本模块所有
//! 可能 panic 的转换（`CString::new`）都走 `Result` 显式判错。后台任务
//! 的 panic 被 tokio 吞掉、表现为事件通道关闭——调用方看到的是
//! `ERR_STOPPED`，不是进程猝死。

// SAFETY 豁免声明：本文件的全部 unsafe 都是对 C 调用方指针的边界检查
// 与裸指针重建，每处均有 SAFETY 注释说明成立条件。
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_void};

use crate::core::{EventCallback, ImSdkEvent, SdkClient};
use crate::error;

/// 供 C 用的版本串：`concat!` 在**编译期**拼上 NUL 终止符，
/// 不需要任何运行期构造（对照：`CString::new` 不是 const fn，静态化不了）。
const VERSION_CSTR: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

/// `user_data` 裸指针的 Send 包装：C 回调上下文要跟着事件泵线程走。
///
/// 裸指针天生 `!Send`（编译器不知道 C 侧的数据是否可跨线程访问），
/// 而 `std::thread::spawn` 要求闭包 `Send`——FFI SDK 的经典两难。
/// 包装并手动声明 `Send` 的成立条件由**调用方契约**`背书：user_data`
/// 指向的数据在客户端销毁前有效、且可从回调线程访问（C 侧保证）。
struct UserData(*mut c_void);
// SAFETY: 成立条件如上——契约由 im_sdk.h 的线程契约条文固定。
unsafe impl Send for UserData {}

/// SDK 版本（`CARGO_PKG_VERSION`，如 `"0.1.0"`）。
///
/// 返回**静态字符串**：进程生命周期内有效，调用方不要 free、也不要改。
/// 跨语言调用方应在启动时对照它做兼容性检查（大版本不符就拒绝加载）。
#[unsafe(no_mangle)]
pub extern "C" fn im_sdk_version() -> *const c_char {
    // SAFETY: 编译期常量，NUL 结尾、进程常驻
    VERSION_CSTR.as_ptr().cast()
}

/// 错误码 → 人读说明（静态字符串，不要 free）。
#[unsafe(no_mangle)]
pub extern "C" fn im_sdk_error_string(code: i32) -> *const c_char {
    // 表长 = 定义码数 + 1（末位是 unknown 兜底，任意越界码都落到它）。
    // `CString::new` 非 const，用 OnceLock 惰性建表后只读。
    static STRINGS: std::sync::OnceLock<Vec<CString>> = std::sync::OnceLock::new();
    let table = STRINGS.get_or_init(|| {
        (0..=error::ERR_INTERNAL)
            .chain(std::iter::once(error::ERR_INTERNAL + 1)) // 末位：unknown 兜底
            .map(|code| {
                CString::new(error::error_string(code))
                    .expect("错误说明不含内部 NUL——error_string 全是可读英文")
            })
            .collect()
    });
    let last = table.len() - 1;
    let idx = if (0..=error::ERR_INTERNAL).contains(&code) { code as usize } else { last };
    table[idx].as_ptr()
}

/// 创建客户端。返回不透明句柄；失败返回 null（入参非法 / 运行时起不来）。
///
/// `data_dir` 可为 null（临时目录）；`callback` 可为 null（轮询模式）。
/// 事件泵线程（回调模式）会以 `user_data` 为第二参调用回调。
#[unsafe(no_mangle)]
pub extern "C" fn im_sdk_client_create(
    server_addr: *const c_char,
    user_id: u64,
    token: *const c_char,
    data_dir: *const c_char,
    callback: Option<EventCallback>,
    user_data: *mut c_void,
) -> *mut SdkClient {
    // SAFETY: C 契约要求字符串入参非空且 NUL 结尾；违约时走错误路径而非解引用。
    // 取出即拷贝（str_from_c 返回 String）——入参指针的寿命到本次调用为止。
    let (Some(addr), Some(token)) =
        (unsafe { str_from_c(server_addr) }, unsafe { str_from_c(token) })
    else {
        return std::ptr::null_mut();
    };
    // SAFETY: data_dir 允许为 null（None → 临时目录）
    let data_dir = unsafe { str_from_c(data_dir) };

    let Ok(mut client) = crate::core::create(&addr, user_id, &token, data_dir.as_deref()) else {
        return std::ptr::null_mut();
    };

    // 回调模式装配：取走事件接收端（互斥保证 poll 再来必被拒）+ 起事件泵。
    // 泵起不来时把已建好的客户端销毁干净再报错——不留半启动的烂摊子。
    if let Some(cb) = callback {
        let events = client.take_events().expect("create 返回的客户端必然持有事件接收端");
        if let Ok(pump) = spawn_pump(events, cb, user_data) { client.attach_pump(pump) } else {
            client.destroy();
            return std::ptr::null_mut();
        }
    }
    Box::into_raw(Box::new(client))
}

/// 发一条消息。`IM_SDK_OK` = 已入队（送达以 `Ack` 事件为准）。
#[unsafe(no_mangle)]
pub extern "C" fn im_sdk_client_send(
    client: *mut SdkClient,
    to: u64,
    data: *const u8,
    len: usize,
) -> i32 {
    // SAFETY: 句柄契约——指针来自 create、未被销毁、本次调用无并发 destroy
    let Some(client) = (unsafe { client.as_ref() }) else {
        return error::ERR_INVALID_ARG;
    };
    // 空内容是合法消息（协议允许 0 字节）；null + len>0 才是矛盾入参
    if data.is_null() && len > 0 {
        return error::ERR_INVALID_ARG;
    }
    // SAFETY: (data, len) 是 C 契约中的有效只读区间；立即拷贝，指针寿命到此为止
    let content = if len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(data, len) }.to_vec()
    };
    match client.send(to, &content) {
        Ok(()) => error::OK,
        Err(code) => code,
    }
}

/// 轮询模式下取一条事件。`IM_SDK_OK` 时 `*out` 为事件指针（须 free）；
/// 超时返回 `IM_SDK_ERR_TIMEOUT`（`*out` 未动）。
#[unsafe(no_mangle)]
pub extern "C" fn im_sdk_client_poll_event(
    client: *mut SdkClient,
    out: *mut *mut ImSdkEvent,
    timeout_ms: u32,
) -> i32 {
    // 出参指针必须可用（null 出参 = 调用方错误，直接拒）
    if out.is_null() {
        return error::ERR_INVALID_ARG;
    }
    // SAFETY: 句柄契约同 send；poll 需要 &mut（pending 队列出队）
    let Some(client) = (unsafe { client.as_mut() }) else {
        return error::ERR_INVALID_ARG;
    };
    match client.poll_event(std::time::Duration::from_millis(u64::from(timeout_ms))) {
        Ok(Some(event)) => {
            // SAFETY: out 已判空；Box::into_raw 是安全装箱，指针所有权移交 C
            unsafe { *out = Box::into_raw(Box::new(event)) };
            error::OK
        }
        Ok(None) => error::ERR_TIMEOUT,
        Err(code) => code,
    }
}

/// 回收一条事件（poll 路径的调用方义务；回调路径 SDK 已代劳）。
///
/// # Safety
/// 指针须来自 `im_sdk_client_poll_event`，且只 free 一次；null 是允许的空操作。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn im_sdk_event_free(event: *mut ImSdkEvent) {
    // SAFETY: 转交给 free_event_boxed，其 Safety 契约与本函数一致
    unsafe { free_event_boxed(event) };
}

/// 销毁客户端（收回全部资源：运行时、线程、句柄）。
///
/// 调用后句柄立即作废；不得与其他调用并发。
#[unsafe(no_mangle)]
pub extern "C" fn im_sdk_client_destroy(client: *mut SdkClient) {
    if client.is_null() {
        return;
    }
    // SAFETY: 调用方契约——destroy 时无并发访问、指针来自 create 且未销毁
    let client = unsafe { Box::from_raw(client) };
    (*client).destroy();
}

/// 事件泵主循环：`blocking_recv`（非运行时线程的同步桥）→ 编组 → 回调 → 回收。
///
/// 独立成函数而不写在闭包里，除了可读性还有一个硬理由：闭包捕获分析
/// （RFC 2229）会把「字段访问/模式解构」都归一化成**字段路径**捕获——
/// 写在闭包体里就只捕获那个裸指针字段（!Send），Send 包装白做。
/// 整个结构体**按值传参**给函数，闭包捕获的才是完整的 `UserData`。
///
/// 线程退出条件唯一：事件通道关闭 = 连接状态机已落幕（destroy 触发的
/// shutdown 会让它退出并 drop 发送端）。通道关闭前线程不空转（recv 阻塞）。
fn pump_loop(
    mut events: tokio::sync::mpsc::Receiver<im_client::ClientEvent>,
    callback: EventCallback,
    user_data: UserData,
) {
    while let Some(event) = events.blocking_recv() {
        for one in crate::core::normalize(event) {
            // 回调作用域契约：装箱 → 回调 → 回收，一事件一闭环
            let raw = Box::into_raw(Box::new(crate::core::alloc_event(one)));
            callback(raw, user_data.0);
            // SAFETY: raw 是上一行刚装箱的活指针；回调若想保留内容，
            // 契约要求它已自行拷贝（im_sdk.h 回调条文）。
            unsafe { free_event_boxed(raw) };
        }
    }
}

fn spawn_pump(
    events: tokio::sync::mpsc::Receiver<im_client::ClientEvent>,
    callback: EventCallback,
    user_data: *mut c_void,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    // 裸指针进闭包前先过 Send 包装（见 UserData 的 SAFETY 论证）
    let user_data = UserData(user_data);
    std::thread::Builder::new()
        .name("im-sdk-event-pump".into())
        // 整体按值传参：闭包只能捕获完整的 UserData（连带 Send 资格）
        .spawn(move || pump_loop(events, callback, user_data))
}

/// 回收：事件结构体与内部的 data 一并重建为 Box 后 drop。
///
/// # Safety
/// `event` 须为 `Box::into_raw(Box::new(alloc_event(..)))` 的产物且只回收一次。
unsafe fn free_event_boxed(event: *mut ImSdkEvent) {
    if event.is_null() {
        return;
    }
    // SAFETY: 调用方契约保证指针来源与唯一性（见函数 Safety 段）
    let event = unsafe { Box::from_raw(event) };
    if !event.data.is_null() {
        // SAFETY: data 由 alloc_event 的 Box<[u8]> 而来，
        // (data, data_len) 即 (into_raw 时的指针, 切片长度)——无损重建
        // （slice_from_raw_parts_mut 本身是安全函数，Box::from_raw 才是 unsafe）
        let slice = std::ptr::slice_from_raw_parts_mut(event.data, event.data_len);
        drop(unsafe { Box::from_raw(slice) });
    }
}

/// C 字符串入参 → Rust `String`：NUL 结尾 + UTF-8 校验，缺一即 None。
///
/// 返回**拷贝**——「立即拷贝」是本 SDK 的内存契约之一：入参指针的寿命
/// 只到本次调用返回，SDK 内部不保存任何指向调用方内存的指针。
///
/// # Safety
/// `ptr` 须为 null 或指向 NUL 结尾的有效内存。
pub(crate) unsafe fn str_from_c(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: 契约保证 NUL 结尾；CStr 只读借用，拷贝发生在返回前
    let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
    std::str::from_utf8(bytes).ok().map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::EVENT_CONNECTED;
    use im_server::SessionConfig;
    use std::sync::mpsc::channel as std_channel;
    use std::time::Duration;

    /// 版本串与错误串都是静态的：同码两次调用返回同一指针（无隐式分配）。
    #[test]
    fn static_strings_return_stable_pointers() {
        assert_eq!(im_sdk_version(), im_sdk_version());
        assert_eq!(im_sdk_error_string(0), im_sdk_error_string(0));
        // SAFETY: 版本串按 C 契约 NUL 结尾
        let version = unsafe { CStr::from_ptr(im_sdk_version()) };
        assert_eq!(version.to_bytes(), crate::SDK_VERSION.as_bytes());
        // SAFETY: 错误说明同上
        let unknown = unsafe { CStr::from_ptr(im_sdk_error_string(404)) };
        assert_eq!(unknown.to_bytes(), b"unknown error code");
    }

    /// 入参防御：null 字符串 → null 句柄；null 句柄 send/poll → `INVALID_ARG`。
    #[test]
    fn invalid_arguments_are_rejected_not_dereferenced() {
        assert!(
            im_sdk_client_create(
                std::ptr::null(),
                1,
                std::ptr::null(),
                std::ptr::null(),
                None,
                std::ptr::null_mut(),
            )
            .is_null()
        );

        let rc = im_sdk_client_send(std::ptr::null_mut(), 1, b"x".as_ptr(), 1);
        assert_eq!(rc, error::ERR_INVALID_ARG);

        let mut out: *mut ImSdkEvent = std::ptr::null_mut();
        let rc = im_sdk_client_poll_event(std::ptr::null_mut(), &raw mut out, 1);
        assert_eq!(rc, error::ERR_INVALID_ARG);
        // null 出参也是调用方错误
        let rc = im_sdk_client_poll_event(std::ptr::null_mut(), std::ptr::null_mut(), 1);
        assert_eq!(rc, error::ERR_INVALID_ARG);

        // destroy/free 对 null 是合法空操作（防御式收尾）
        im_sdk_client_destroy(std::ptr::null_mut());
        // SAFETY: null 是 free_event 契约允许的空操作
        unsafe { im_sdk_event_free(std::ptr::null_mut()) };
    }

    /// 回调模式端到端（C 形态的回调 + `user_data` 裸指针过 Send 包装）：
    /// 泵线程把 Connected/Message 送达回调，事件作用域契约由拷贝履行。
    ///
    /// 普通 #[test]：SDK 同步入口内部 `block_on，不能在` tokio 上下文里调
    /// （与 core.rs 测试同一套口径，详见那边的 `TestServer` 注释）。
    #[test]
    fn callback_pump_delivers_events_end_to_end() {
        // 服务端挂在独立 runtime 上（drop 即停）
        let rt = tokio::runtime::Runtime::new().expect("测试服务端运行时");
        let (addr, _sessions, _shutdown) = rt
            .block_on(async { im_server::spawn_server(SessionConfig::default()).await })
            .expect("测试服务端应能启动");
        let addr = CString::new(addr.to_string()).unwrap();
        let token = CString::new("demo").unwrap(); // SessionConfig::default 的静态口令

        // C 形态的回调：extern "C" + user_data——把事件拷贝进 std 通道
        // （回调里立即拷贝，正是「作用域契约」的模范履行）
        extern "C" fn on_event(ev: *mut ImSdkEvent, user: *mut c_void) {
            // SAFETY: user_data 来自 Box::leak 的 Sender，泵线程存活期间有效
            let tx = unsafe { &*(user as *const std::sync::mpsc::Sender<(i32, Vec<u8>)>) };
            // SAFETY: ev 由泵线程刚装箱、回调返回前有效（作用域契约）
            let e = unsafe { &*ev };
            let data = if e.data.is_null() {
                Vec::new()
            } else {
                // SAFETY: (data, data_len) 是 alloc_event 的配对区间
                unsafe { std::slice::from_raw_parts(e.data, e.data_len) }.to_vec()
            };
            let _ = tx.send((e.type_, data));
        }

        let (tx, rx) = std_channel::<(i32, Vec<u8>)>();
        let tx = std::ptr::from_mut(Box::leak(Box::new(tx))).cast::<c_void>();

        let alice = im_sdk_client_create(
            addr.as_ptr(),
            1,
            token.as_ptr(),
            std::ptr::null(),
            Some(on_event),
            tx,
        );
        assert!(!alice.is_null(), "创建应成功");

        // 事件泵先送 Connected
        let (ty, _) = rx.recv_timeout(Duration::from_secs(10)).expect("应收到 Connected");
        assert_eq!(ty, EVENT_CONNECTED);

        // Bob（轮询模式）发给 Alice：Alice 的泵应送出 Message
        let bob = im_sdk_client_create(
            addr.as_ptr(),
            2,
            token.as_ptr(),
            std::ptr::null(),
            None,
            std::ptr::null_mut(),
        );
        assert!(!bob.is_null());
        let mut ev: *mut ImSdkEvent = std::ptr::null_mut();
        let rc = im_sdk_client_poll_event(bob, &raw mut ev, 5_000);
        assert_eq!(rc, error::OK);
        // SAFETY: poll 成功即移交所有权
        assert_eq!(unsafe { (*ev).type_ }, EVENT_CONNECTED);
        // SAFETY: poll 的所有权契约
        unsafe { im_sdk_event_free(ev) };

        let payload = b"via-pump".to_vec();
        let rc = im_sdk_client_send(bob, 1, payload.as_ptr(), payload.len());
        assert_eq!(rc, error::OK);

        let (ty, data) = rx.recv_timeout(Duration::from_secs(10)).expect("应收到 Message");
        assert_eq!(ty, crate::core::EVENT_MESSAGE);
        assert_eq!(data, b"via-pump");

        // 回调模式下 poll 必须被拒（事件接收端已被泵移走——互斥的物化）
        let rc = im_sdk_client_poll_event(alice, &raw mut ev, 1);
        assert_eq!(rc, error::ERR_POLL_WITH_CALLBACK);

        im_sdk_client_destroy(alice);
        im_sdk_client_destroy(bob);
        // SAFETY: tx 由 Box::leak 而来，此处收回（泵线程已 join，无并发访问）
        drop(unsafe { Box::from_raw(tx.cast::<std::sync::mpsc::Sender<(i32, Vec<u8>)>>()) });
    }
}
