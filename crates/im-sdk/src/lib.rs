//! # im-sdk：FFI SDK 层
//!
//! 职责（阶段 6 实现）：
//! - 将 `im-transport` + `im-crypto` 内核封装为 **C ABI 动态库**（cdylib）
//! - 句柄式 API：跨语言只能拿不透明指针，杜绝直接访问 Rust 结构体字段
//! - 跨语言内存契约：**谁分配谁释放**——Rust 分配的内存只能由 Rust 提供的
//!   释放函数回收，绝不能让 C/Java 端 `free`
//! - 错误模型：返回码 + 错误字符串缓冲区，不 panic 穿越 FFI 边界
//!
//! 学习文档：`docs/09-ffi.md`

/// SDK 版本号：跨语言调用方用于运行时兼容性检查
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn sdk_version_exposed() {
        assert!(!super::SDK_VERSION.is_empty());
    }
}
