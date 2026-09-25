//! 进程内存采样：连接风暴「内存账本」的数据来源。
//!
//! 三级里程碑（10 万 → 100 万 → 500 万连接）的核心指标不是连接数，
//! 而是**每连接内存成本**——它是横向扩容前唯一重要的纵向天花板。
//!
//! # 为什么手写 FFI 而不是引三方库
//!
//! - `sysinfo`/`windows` crate 都能做，但为了两个数（工作集/提交内存）
//!   拖进几 MB 依赖不值得；
//! - 这里只调一个函数：`GetProcessMemoryInfo`（kernel32，PSAPI 自 Win7
//!   起转发进 kernel32）——正好作为**阶段 11 FFI 的前置小菜**：
//!   extern 声明、`#[repr(C)]` 布局、`HANDLE` 的不可拷贝语义，
//!   都在 30 行内演练一遍（完整版见 `im-sdk` 与 docs/17）。
//!
//! # 平台矩阵
//!
//! - Windows：`GetProcessMemoryInfo` 工作集（RAM 占用，连接风暴的主口径）；
//! - Linux：`/proc/self/status` 的 `VmRSS`（同口径）；
//! - 其他平台：返回 `None`，报告如实标注"未采样"。

use std::time::Duration;

/// 一次内存快照（字节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemSnapshot {
    /// 工作集 / 常驻内存（RSS）：真实占用的物理内存。
    pub working_set: u64,
    /// 采样时刻距进程启动的时长（报告对齐用）。
    pub elapsed: Duration,
}

/// 进程启动基准（`Instant` 单调钟）。
fn boot() -> std::time::Instant {
    // 惰性初始化的安全来源：第一次调用即进程内首个采样点附近，
    // 误差是"模块加载到首次采样"的毫秒级，远小于内存变化的量级
    static BOOT: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *BOOT.get_or_init(std::time::Instant::now)
}

/// 采一次当前进程工作集（Windows: `WorkingSetSize`；Linux: `VmRSS`）。
///
/// 返回 `None` = 本平台未实现（报告侧如实标注，不伪造数字——
/// docs/20 §6.4 的诚实账单纪律）。
#[must_use]
pub fn snapshot() -> Option<MemSnapshot> {
    working_set_bytes().map(|working_set| MemSnapshot { working_set, elapsed: boot().elapsed() })
}

/// 平台分派的裸工作集字节数。
#[cfg(windows)]
fn working_set_bytes() -> Option<u64> {
    windows::working_set()
}

/// 平台分派的裸工作集字节数。
#[cfg(target_os = "linux")]
fn working_set_bytes() -> Option<u64> {
    linux::vm_rss()
}

/// 平台分派的裸工作集字节数。
#[cfg(not(any(windows, target_os = "linux")))]
fn working_set_bytes() -> Option<u64> {
    None
}

/// 字节数的人读格式（MiB 两位小数——连接风暴报告的主单位）。
#[must_use]
pub fn fmt_mib(bytes: u64) -> String {
    // KiB 粒度对内存报告足够；整数算术不掺浮点
    let kib = bytes / 1024;
    format!("{}.{:02} MiB", kib / 1024, (kib % 1024) * 100 / 1024)
}

// ────────────────────────────────────────────────────────────────
// Windows：GetProcessMemoryInfo（PSAPI → kernel32 转发）
// ────────────────────────────────────────────────────────────────
#[cfg(windows)]
mod windows {
    //! 手写 kernel32 FFI（安全论证见模块文档）。
    //!
    //! SAFETY 论证（为什么这 30 行 unsafe 是安全的）：
    //! - `GetCurrentProcess` 返回伪句柄（常量 -1，非真实内核对象），
    //!   不需要 CloseHandle，不可能泄漏；
    //! - `GetProcessMemoryInfo` 只**读**统计值并写入调用方提供的缓冲，
    //!   缓冲布局与 `#[repr(C)]` 结构一一对应（字段顺序/宽度来自
    //!   Win32 SDK 头文件），`cb` 字段就是 `size_of` 防线；
    //! - 调用线程无重入、无并发危害：函数是纯查询。
    #![allow(unsafe_code, non_camel_case_types, non_snake_case, clippy::upper_case_acronyms)] // FFI 命名随 Win32 SDK（BOOL/HANDLE 是官方拼写）

    use std::mem::size_of;

    /// Win32 `PROCESS_MEMORY_COUNTERS`（PSAPI.h）的布局镜像。
    ///
    /// 字段全部由 FFI 写入（rustc 看不到"读"，`dead_code` 会误报）；
    /// 一个字段都不允许删——布局错位就是内存破坏。
    #[repr(C)]
    #[allow(dead_code)]
    #[derive(Debug, Clone, Copy)]
    struct PROCESS_MEMORY_COUNTERS {
        cb: u32,
        PageFaultCount: u32,
        PeakWorkingSetSize: usize,
        WorkingSetSize: usize,
        QuotaPeakPagedPoolUsage: usize,
        QuotaPagedPoolUsage: usize,
        QuotaPeakNonPagedPoolUsage: usize,
        QuotaNonPagedPoolUsage: usize,
        PagefileUsage: usize,
        PeakPagefileUsage: usize,
    }

    /// Win32 `BOOL`。
    type BOOL = i32;
    /// Win32 `HANDLE`（伪句柄场景用裸整数，不构造包装类型）。
    type HANDLE = isize;

    unsafe extern "system" {
        /// 当前进程伪句柄（常量，无需关闭）。
        fn GetCurrentProcess() -> HANDLE;
        /// 读进程内存统计。
        fn GetProcessMemoryInfo(
            process: HANDLE,
            counters: *mut PROCESS_MEMORY_COUNTERS,
            cb: u32,
        ) -> BOOL;
    }

    /// 当前进程工作集字节数。
    pub fn working_set() -> Option<u64> {
        let cb = u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS>())
            .expect("结构体大小远小于 u32 上限"); // cast 纪律：try_from 收口
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb,
            PageFaultCount: 0,
            PeakWorkingSetSize: 0,
            WorkingSetSize: 0,
            QuotaPeakPagedPoolUsage: 0,
            QuotaPagedPoolUsage: 0,
            QuotaPeakNonPagedPoolUsage: 0,
            QuotaNonPagedPoolUsage: 0,
            PagefileUsage: 0,
            PeakPagefileUsage: 0,
        };
        let ok = unsafe {
            // SAFETY: counters 指向栈上的合法结构，cb 与结构体实际大小一致；
            // GetCurrentProcess 返回伪句柄恒有效（见模块 SAFETY 论证）
            GetProcessMemoryInfo(GetCurrentProcess(), &raw mut counters, counters.cb)
        };
        (ok != 0).then_some(counters.WorkingSetSize as u64) // usize→u64：同宽无损
    }
}

// ────────────────────────────────────────────────────────────────
// Linux：/proc/self/status 的 VmRSS
// ────────────────────────────────────────────────────────────────
#[cfg(target_os = "linux")]
mod linux {
    //! /proc 文件系统：内核给用户态的标准自省接口。

    use std::fs::File;
    use std::io::{BufRead, BufReader};

    /// 当前进程常驻内存（VmRSS）字节数。
    pub fn vm_rss() -> Option<u64> {
        let file = File::open("/proc/self/status").ok()?;
        for line in BufReader::new(file).lines() {
            let line = line.ok()?;
            // 行形如 "VmRSS:\t  123456 kB"（内核保证每行一个指标）
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb = rest.trim_end_matches("kB").trim().parse::<u64>().ok()?;
                return kb.checked_mul(1024);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机平台（Windows/Linux）应能采到非零工作集。
    /// 其他平台允许 None（平台矩阵的契约测试）。
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn snapshot_reports_nonzero_working_set() {
        let snap = snapshot().expect("本平台应实现内存采样");
        assert!(snap.working_set > 0, "运行中的测试进程不可能零内存");
        // 连续两次采样都应成功（工作集可能缩小——页面可被换出，
        // 所以这里只验"能采到"，不做单调性断言）
        let after = snapshot().expect("第二次采样同样应成功");
        assert!(after.working_set > 0);
    }

    /// MiB 格式化：KiB→MiB 的两位小数换算。
    #[test]
    fn fmt_mib_two_decimals() {
        assert_eq!(fmt_mib(0), "0.00 MiB");
        assert_eq!(fmt_mib(1024 * 1024), "1.00 MiB");
        assert_eq!(fmt_mib(1024 * 1024 + 512 * 1024), "1.50 MiB");
        assert_eq!(fmt_mib(10 * 1024 * 1024), "10.00 MiB");
    }
}
