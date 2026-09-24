//! # im-protocol：IM 二进制协议层（阶段 1）
//!
//! 职责：定义帧格式、命令字、编解码，处理 TCP 粘包/半包。
//! **纯逻辑 crate**——不依赖任何 IO（tokio 等），因此：
//! - 编解码可以用最快的单元测试 + proptest 性质测试覆盖；
//! - 未来更换底层传输（TCP → QUIC）时协议层不动。
//!
//! # 模块地图
//!
//! | 模块 | 内容 | 算法/模式图谱落点 |
//! |------|------|------------------|
//! | [`varint`] | varint / zigzag 编码 | 算法图谱 #1（varint） |
//! | [`crc32`] | CRC-32 查表实现（编译期建表） | 算法图谱 #2（CRC32） |
//! | [`frame`] | 帧结构 / 命令字 / 编码 | 享元（`Bytes`）、错误即类型 |
//! | [`codec`] | 增量解码状态机（粘包/半包） | 算法图谱 #3 + enum 状态机模式 |
//! | [`payload`] | 各命令字的结构化载荷（阶段 3） | varint 实战、游标式解析 |
//!
//! # 快速上手
//!
//! ```
//! use bytes::Bytes;
//! use im_protocol::{Cmd, Frame, FrameDecoder};
//!
//! // 编码一帧
//! let frame = Frame::new(Cmd::Msg, 1, 0, Bytes::from_static(b"hello"));
//! let wire = frame.encode();
//!
//! // 解码（增量：可分任意多次喂入）
//! let mut decoder = FrameDecoder::new();
//! let frames = decoder.decode(&wire).unwrap();
//! assert_eq!(frames, vec![frame]);
//! ```
//!
//! 学习文档：`docs/04-protocol-design.md`（阶段 1 编写）。

pub mod codec;
pub mod crc32;
pub mod error;
pub mod frame;
pub mod payload;
pub mod varint;

pub use codec::{DEFAULT_MAX_FRAME_LEN, FrameDecoder};
pub use error::ProtocolError;
pub use frame::{Cmd, Frame};
pub use payload::{Handshake, HandshakeAck, Msg, MsgAck, Payload, SyncReq, SyncResp};
pub use varint::{decode_u64, encode_i64, encode_u64, zigzag_decode, zigzag_encode};

/// 协议 magic：每帧的固定开头，用于快速识别本协议的流量（类似 PNG 头）。
pub const MAGIC: u16 = 0x494D; // "IM"

/// 协议版本号：预留演进空间，握手时可协商。
pub const VERSION: u8 = 1;

#[cfg(test)]
mod tests {
    /// 门面测试：验证 crate 对外导出的路径全部可用（API 稳定性哨兵）。
    #[test]
    fn public_api_is_reachable() {
        assert_eq!(super::MAGIC, 0x494D);
        assert_eq!(super::VERSION, 1);

        // re-export 的类型可从 crate 根直接引用
        let frame = super::Frame::new(super::Cmd::Ping, 1, 0, bytes::Bytes::new());
        assert_eq!(frame.cmd, super::Cmd::Ping);

        let mut decoder = super::FrameDecoder::new();
        assert_eq!(decoder.decode(&frame.encode()).unwrap(), vec![frame]);

        assert_eq!(super::crc32::checksum(b"123456789"), 0xCBF4_3926);
        assert_eq!(super::varint::zigzag_encode(-1), 1);
    }
}
