//! 错误码模型：FFI 边界上**不抛异常、不 panic、不返回 Rust 的枚举**，
//! 只有 `i32` 返回码 + 配套的字符串说明函数。
//!
//! # 为什么是数字而不是字符串
//!
//! - C 语言的错误处理惯例就是返回码（Java 对照：`errno` / `GetLastError`
//!   / `HRESULT`——所有成熟 C ABI 都走这条路）；
//! - 数字可以 `match`，可以跨语言比较，可以写进日志不丢失结构；
//! - 字符串只做**人读的补充**：[`error_string`]，永远不参与控制流。
//!
//! # 约定
//!
//! - `0` 恒为成功（[`OK`]）——调用方可以 `if (rc != IM_SDK_OK)` 一条判断；
//! - 非 0 码**稳定**：一旦发布不再改动数值（外部代码可能 switch 它），
//!   新增错误只追加新码；
//! - 未定义的码（含负数）返回 `"unknown error code"`——向前兼容的兜底。

/// 0：成功。
pub const OK: i32 = 0;
/// 1：入参非法（空指针、非 UTF-8 字符串、非法长度）。
pub const ERR_INVALID_ARG: i32 = 1;
/// 2：客户端已退出（握手被拒/destroy 之后），命令不再被处理。
pub const ERR_STOPPED: i32 = 2;
/// 3：等待事件超时（[`crate::ffi::im_sdk_client_poll_event`] 的正常路径之一）。
pub const ERR_TIMEOUT: i32 = 3;
/// 4：回调模式下调用 poll（事件归事件泵线程，调用方不该抢）。
pub const ERR_POLL_WITH_CALLBACK: i32 = 4;
/// 5：内部故障（兜底：理论上不可达，保留给防御性路径）。
pub const ERR_INTERNAL: i32 = 5;

/// 错误码 → 人读说明。
///
/// 返回的是**静态字符串**（进程生命周期内有效），调用方绝不能 `free`——
/// 这是「谁分配谁释放」契约的第一课：静态数据的所有权根本不在调用方。
///
/// 索引安全：码值即下标（`0..=5`），数组长度与码值上限同步维护。
#[must_use]
pub const fn error_string(code: i32) -> &'static str {
    const STRINGS: [&str; 6] = [
        "ok",
        "invalid argument (null pointer / non-utf8 string / bad length)",
        "client stopped (rejected or destroyed)",
        "timed out waiting for event",
        "poll is unavailable when a callback is registered",
        "internal error",
    ];
    if code >= 0 && (code as usize) < STRINGS.len() {
        STRINGS[code as usize]
    } else {
        "unknown error code"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个定义过的码都必须有说明文字（非空、非占位）。
    #[test]
    fn every_defined_code_has_a_string() {
        for code in
            [OK, ERR_INVALID_ARG, ERR_STOPPED, ERR_TIMEOUT, ERR_POLL_WITH_CALLBACK, ERR_INTERNAL]
        {
            assert!(!error_string(code).is_empty());
        }
    }

    /// 未定义码（含负数）落到统一兜底，绝不 panic 越界。
    #[test]
    fn unknown_codes_fall_back() {
        assert_eq!(error_string(6), "unknown error code");
        assert_eq!(error_string(-1), "unknown error code");
        assert_eq!(error_string(i32::MAX), "unknown error code");
    }
}
