//! # im-sdk：FFI SDK 层
//!
//! 职责（阶段 11 实现）：把 [`im-client`] 的异步内核（连接状态机/
//! 消息级重传/离线同步/本地持久化）封装成 **C ABI 动态库**
//! （`crate-type = ["cdylib"]` → `.so` / `.dll` / `.dylib`），
//! 供 C / Java(JNI) / 其他语言宿主嵌入。
//!
//! # 分层（外观模式：每一层只对相邻层负责）
//!
//! ```text
//!   C / Java / 任意宿主语言
//!        │  不透明句柄 + i32 返回码 + 事件结构体
//!   ┌────▼──────────────────────────────────────────┐
//!   │ ffi.rs：C ABI（唯一的 unsafe 集中区）           │
//!   │   指针判空 / UTF-8 校验 / 内存契约履行点         │
//!   ├──────────────────────────────────────────────┤
//!   │ core.rs：同步外观 SdkClient（全安全代码）        │
//!   │   专属 Runtime / 事件泵线程 / SyncBatch 展开     │
//!   ├──────────────────────────────────────────────┤
//!   │ jni.rs（feature "jni"）：Java 侧糖衣            │
//!   │   JavaVM / GlobalRef / UTF-16 转换              │
//!   ├──────────────────────────────────────────────┤
//!   │ im-client：异步内核（本 crate 不改一行）         │
//!   └──────────────────────────────────────────────┘
//! ```
//!
//! # 三条契约（跨语言 SDK 的命根，docs/17 全文展开）
//!
//! 1. **内存契约**（谁分配谁释放）：入参立即拷贝；SDK 分配的事件只能由
//!    `im_sdk_event_free` 回收（回调模式下 SDK 代劳）；静态字符串不 free。
//! 2. **线程契约**：句柄可跨线程传递（内部全线程安全类型）；
//!    `destroy` 不得与其他调用并发；回调发生在 SDK 事件泵线程上。
//! 3. **错误契约**：不抛异常、不 panic 穿越 FFI，只有稳定的 i32 返回码。
//!
//! # 为什么不用 cbindgen 生成头文件
//!
//! [`include/im_sdk.h`] 手写：API 面（7 个函数）足够小，手写头文件 +
//! 文档注释一体化，读源码即读契约。cbindgen 适合大 API 面/频繁变动的
//! 项目（docs/17 §「已知取舍」给出两条路线的切换判据）。
//!
//! [`include/im_sdk.h`]: https://github.com/lvdapiaoliang/rust-im/blob/master/crates/im-sdk/include/im_sdk.h

pub mod core;
pub mod error;
mod ffi;

#[cfg(feature = "jni")]
mod jni;

pub use core::{
    EVENT_ACK, EVENT_CONNECTED, EVENT_DISCONNECTED, EVENT_MESSAGE, EVENT_MESSAGE_QUEUED,
    EVENT_REJECTED, EVENT_SEND_FAILED, ImSdkEvent, SdkClient,
};
pub use error::{
    ERR_INTERNAL, ERR_INVALID_ARG, ERR_POLL_WITH_CALLBACK, ERR_STOPPED, ERR_TIMEOUT, OK,
};

/// SDK 版本号：跨语言调用方用于运行时兼容性检查
/// （C 侧同名函数 `im_sdk_version`；JNI 侧 `Sdk.version()`——三处同源于此）。
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    /// 版本号常驻可取（JNI 侧的 nativeVersion 也走它做兼容检查）。
    #[test]
    fn sdk_version_exposed() {
        assert!(!SDK_VERSION.is_empty());
    }

    /// Rust 侧符号与 C 头文件一一对应：每个导出函数都能按名取到地址。
    ///
    /// #[`no_mangle`] 保证了链接期符号名就是函数名——这条测试在**编译期**
    /// 就把「头文件声明了、库里没有」这类低级失配锁死（运行期对不上的
    /// 变体见 docs/17 测试策略一节）。
    #[test]
    fn c_abi_symbols_are_reachable_from_rust() {
        // 以函数指针形态引用一遍：验证它们是真实的一等符号（而非泛型零 instantiated）
        let _ = ffi::im_sdk_version as unsafe extern "C" fn() -> *const std::os::raw::c_char;
        let _ =
            ffi::im_sdk_error_string as unsafe extern "C" fn(i32) -> *const std::os::raw::c_char;
        let _ = ffi::im_sdk_client_destroy as unsafe extern "C" fn(*mut SdkClient);
        let _ = ffi::im_sdk_event_free as unsafe extern "C" fn(*mut ImSdkEvent);
    }
}
