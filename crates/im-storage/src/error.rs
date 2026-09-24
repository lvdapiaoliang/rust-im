//! 存储层错误类型。

use im_protocol::ProtocolError;

/// 存储层错误。
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// 文件系统 IO 故障（目录不存在、磁盘满、权限……）。
    #[error("storage io error: {0}")]
    Io(#[from] std::io::Error),
    /// 段文件内容损坏（CRC 不符/记录非法且发生在非尾部位置）。
    #[error("storage corrupted")]
    Corrupted,
    /// 语义层的值解码失败（存进去的是消息，读出来解不开——版本错配或损坏）。
    #[error("stored value decode failed: {0}")]
    Decode(#[from] ProtocolError),
}
