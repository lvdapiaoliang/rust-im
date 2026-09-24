//! 协议层错误类型。
//!
//! 设计原则（「错误即类型」，见 learning-rust-from-scratch/05-patterns/01）：
//! 每个变体对应一种**调用方需要区别对待**的失败模式——
//! 调用方可以 `match` 决策：`BadMagic` 说明对端不是本协议（配置错误），
//! `CrcMismatch` 说明链路误码（可断开重连）。
//! `#[non_exhaustive]` 保证未来新增变体不算 breaking change。

/// 协议编解码错误。
///
/// 出错后解码器状态不可复用，调用方应断开对应连接
/// （阶段 2 传输层会讨论「断开 vs 重同步」的取舍）。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// 帧头 magic 不匹配：对端发的可能不是本协议，或字节流已错位。
    #[error("bad magic: expected {expected:#06x}, got {got:#06x}")]
    BadMagic {
        /// 期望的 magic 值（本端协议常量）。
        expected: u16,
        /// 实际读到的值。
        got: u16,
    },

    /// 协议版本不支持：本端太旧或对端太新。
    #[error("unsupported protocol version: got {got}, supported: {supported}")]
    UnsupportedVersion {
        /// 对端声明的版本号。
        got: u8,
        /// 本端支持的版本号。
        supported: u8,
    },

    /// 未知命令字：两端版本不一致，或字节流被污染。
    #[error("unknown command byte: {got:#04x}")]
    UnknownCommand {
        /// 读到的一字节命令字。
        got: u8,
    },

    /// varint 超过 10 字节上限（u64 最多需要 10 个 7-bit 组）。
    ///
    /// 通常意味着字节流错位——正常的 varint 不可能这么长。
    #[error("varint longer than {max} bytes")]
    VarintTooLong {
        /// varint 的最大合法长度。
        max: usize,
    },

    /// 声明的 payload 长度超过上限。
    ///
    /// 关键防护：在**分配内存之前**拒绝——恶意/损坏的长度字段
    /// 最多骗我们读几个 varint 字节，骗不来 1 GiB 的分配。
    #[error("frame too large: {got} bytes (max {max})")]
    FrameTooLarge {
        /// 声明的 payload 长度。
        got: u64,
        /// 配置的单帧上限。
        max: usize,
    },

    /// CRC 校验失败：链路误码或载荷被篡改。
    #[error("crc mismatch: expected {expected:#010x}, got {got:#010x}")]
    CrcMismatch {
        /// 按收到的字节计算出的 CRC。
        expected: u32,
        /// 帧尾携带的 CRC。
        got: u32,
    },

    /// payload 比字段声明需要的还短（截断的载荷）。
    ///
    /// 与帧层的截断不同：帧本身是完整的（CRC 过了），
    /// 但载荷内部的字段序列不满足格式——对端编码器有 bug。
    #[error("payload truncated: need {need} more bytes, got {got}")]
    PayloadTooShort {
        /// 还缺多少字节。
        need: usize,
        /// 实际剩余字节数。
        got: usize,
    },

    /// 载荷中声明了非法的字符串：UTF-8 序列损坏。
    #[error("invalid utf-8 in payload string")]
    InvalidUtf8,

    /// 载荷解析完字段后还有剩余字节（两端的载荷格式不一致）。
    #[error("payload has {extra} trailing bytes")]
    TrailingBytes {
        /// 多余的字节数。
        extra: usize,
    },
}

#[cfg(test)]
mod tests {
    //! 错误类型自身无逻辑，只验证 Display 文案可读（日志友好性）。
    use super::*;

    #[test]
    fn display_messages_are_helpful() {
        let e = ProtocolError::BadMagic { expected: 0x494D, got: 0x1234 };
        assert!(e.to_string().contains("494d"), "实际输出: {e}");

        let e = ProtocolError::FrameTooLarge { got: 999, max: 100 };
        assert!(e.to_string().contains("999"));
    }
}
