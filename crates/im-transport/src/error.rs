//! 传输层错误类型。
//!
//! 与 `im-protocol` 的 [`ProtocolError`]（字节流层面的违规）分工：
//! [`TransportError`] 覆盖**连接生命周期**的全部失败方式——
//! IO 故障、协议违规上抛、空闲超时、通道关闭。
//! 每个变体自带结构化数据（而非字符串），调用方可以按变体决定策略：
//! 重连、断连还是仅仅记录。

use std::io;
use std::time::Duration;

use im_protocol::ProtocolError;

/// 传输层错误：IO 故障、协议违规、连接状态异常的统一表达。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransportError {
    /// 底层 TCP IO 故障（连接重置、网络不可达等）。
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// 字节流违反协议：阶段 1 的 `ProtocolError` 原样上抛。
    ///
    /// 收到它说明**对端实现有 bug 或在发送恶意流**，
    /// 正确动作是立即断连（解码器状态已不可用，见 codec 文档）。
    #[error("protocol violation: {0}")]
    Protocol(#[from] ProtocolError),

    /// 读空闲超时：`idle_timeout` 内一个字节都没到，判定对端已死。
    ///
    /// 对照 Java Netty 的 `ReadTimeoutHandler`——
    /// 区别是这里的超时由 `tokio::time::timeout` 组合子表达，
    /// 没有专门的 Handler 概念。
    #[error("read idle for {0:?}, connection considered dead")]
    IdleTimeout(Duration),

    /// 出站通道已关闭：写 actor 或业务层已退出，连接无法继续发送。
    #[error("outbound channel closed")]
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// From 转换链：io::Error / ProtocolError 都能直接 `?` 进 TransportError
    #[test]
    fn errors_convert_from_sources() {
        let io_err: TransportError = io::Error::new(io::ErrorKind::ConnectionReset, "reset").into();
        assert!(matches!(io_err, TransportError::Io(_)));

        let proto_err: TransportError =
            ProtocolError::UnknownCommand { got: 0xFF }.into();
        assert!(matches!(proto_err, TransportError::Protocol(_)));
    }

    /// Display 输出包含结构化信息（日志友好性检查）
    #[test]
    fn display_is_informative() {
        let e = TransportError::IdleTimeout(Duration::from_secs(30));
        assert!(e.to_string().contains("30s"));
        assert!(e.to_string().contains("idle"));
    }
}
