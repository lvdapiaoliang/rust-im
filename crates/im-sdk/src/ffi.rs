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

// SAFETY 豁免声明：本文件的全部 unsafe 都是对 C 调用方指针的边界检查，
// 每处均有 SAFETY 注释说明成立条件。
#![allow(unsafe_code)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

use crate::core::{ImSdkEvent, SdkClient, EventCallback};
use crate::error;

/// 供 C 用的版本串：`concat!` 在**编译期**拼上 NUL 终止符，
/// 不需要任何运行期构造（对照：`CString::new` 不是 const fn，静态化不了）。
const VERSION_CSTR: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

/// SDK 版本（`CARGO_PKG_VERSION`，如 `"0.1.0"`）。
///
/// 返回**静态字符串**：进程生命周期内有效，调用方不要 free、也不要改。
/// 跨语言调用方应在启动时对照它做兼容性检查（大版本不符就拒绝加载）。
#[no_mangle]
pub extern "C" fn im_sdk_version() -> *const c_char {
    // SAFETY: 编译期常量，NUL 结尾、进程常驻
    VERSION_CSTR.as_ptr().cast()
}

/// 错误码 → 人读说明（静态字符串，不要 free）。
#[no_mangle]
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
    let idx = if (0..=error::ERR_INTERNAL).contains(&code) {
        code as usize
    } else {
        last
    };
    table[idx].as_ptr()
}

/// 创建客户端。返回不透明句柄；失败返回 null（入参非法 / 运行时起不来）。
///
/// `data_dir` 可为 null（临时目录）；`callback` 可为 null（轮询模式）。
/// 事件泵线程（回调模式）会以 `user_data` 为第二参调用回调。
#[no_mangle]
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
    if let Ok(client) =
        crate::core::create(&addr, user_id, &token, data_dir.as_deref(), callback, user_data)
    {
        Box::into_raw(Box::new(client))
    } else {
        std::ptr::null_mut()
    }
}

/// 发一条消息。`IM_SDK_OK` = 已入队（送达以 `Ack` 事件为准）。
#[no_mangle]
pub extern "C" fn im_sdk_client_send(
    client: *mut SdkClient,
    to: u64,
    data: *const u8,
    len: usize,
) -> i32 {
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
#[no_mangle]
pub extern "C" fn im_sdk_client_poll_event(
    client: *mut SdkClient,
    out: *mut *mut ImSdkEvent,
    timeout_ms: u32,
) -> i32 {
    // 出参指针必须可用（null 出参 = 调用方错误，直接拒）
    if out.is_null() {
        return error::ERR_INVALID_ARG;
    }
    let Some(client) = (unsafe { client.as_mut() }) else {
        return error::ERR_INVALID_ARG;
    };
    match client.poll_event(std::time::Duration::from_millis(u64::from(timeout_ms))) {
        Ok(Some(event)) => {
            // SAFETY: out 已判空；事件指针由 alloc_event 刚分配
            unsafe { *out = event };
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
#[no_mangle]
pub unsafe extern "C" fn im_sdk_event_free(event: *mut ImSdkEvent) {
    // SAFETY: 转交给 core::free_event，其 Safety 契约与本函数一致
    unsafe { crate::core::free_event(event) };
}

/// 销毁客户端（收回全部资源：运行时、线程、句柄）。
///
/// 调用后句柄立即作废；不得与其他调用并发。
#[no_mangle]
pub extern "C" fn im_sdk_client_destroy(client: *mut SdkClient) {
    if client.is_null() {
        return;
    }
    // SAFETY: 调用方契约——destroy 时无并发访问、指针来自 create 且未销毁
    unsafe { drop(Box::from_raw(client)) };
}

/// C 字符串入参 → Rust `String`：NUL 结尾 + UTF-8 校验，缺一即 None。
///
/// 返回**拷贝**——「立即拷贝」是本 SDK 的内存契约之一：入参指针的寿命
/// 只到本次调用返回，SDK 内部不保存任何指向调用方内存的指针。
///
/// # Safety
/// `ptr` 须为 null 或指向 NUL 结尾的有效内存。
unsafe fn str_from_c(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: 契约保证 NUL 结尾；CStr 只读借用，拷贝发生在返回前
    let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
    std::str::from_utf8(bytes).ok().map(ToOwned::to_owned)
}
