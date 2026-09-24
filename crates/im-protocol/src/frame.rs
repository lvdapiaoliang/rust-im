//! 帧结构与编码——协议的"名词"部分（codec 是"动词"部分）。
//!
//! # 线上格式（一帧 = 头 + 变长区 + 载荷 + 尾）
//!
//! ```text
//! ┌────────┬────────┬──────┬───────┬────────┬────────┬────────┬─────────┬───────┐
//! │ magic  │version │ cmd  │ flags │  seq   │  ack   │  len   │ payload │ crc32 │
//! │ 2 byte│ 1 byte │1 byte│1 byte │ varint │ varint │ varint │ len byte│4 byte│
//! └────────┴────────┴──────┴───────┴────────┴────────┴────────┴─────────┴───────┘
//!  <────────────────── 5 字节固定头 ──────────────>  <── 变长 ──>          ↑
//!                                                                     覆盖之前所有字节
//! ```
//!
//! - 固定头让解码器**第一时间校验 magic/版本**，尽早发现"这不是本协议的流"；
//! - seq/ack/len 用 varint：小数字 1~2 字节（见 [`crate::varint`]）；
//! - CRC 覆盖 crc 字段之前的所有字节，帧尾 4 字节大端。

use bytes::{BufMut, Bytes, BytesMut};

use crate::crc32::Crc32;
use crate::error::ProtocolError;
use crate::varint::{self, MAX_VARINT_LEN};

/// 命令字：帧的"动词"。
///
/// 用 enum 而非裸 `u8`：未知值在**解码边界**就变成
/// [`ProtocolError::UnknownCommand`]（而不是污染业务层）；
/// 新增命令字时，所有 `match` 都会被穷尽性检查强制更新。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cmd {
    /// 客户端 → 服务端：握手请求（携带认证凭证）。
    Handshake,
    /// 服务端 → 客户端：握手应答（分配会话）。
    HandshakeAck,
    /// 心跳探测（客户端发起）。
    Ping,
    /// 心跳应答（服务端回）。
    Pong,
    /// 单聊/群聊消息（上行）。
    Msg,
    /// 消息确认（服务端 → 客户端，确认已收到对应 `seq`）。
    MsgAck,
    /// 离线消息同步请求（重连后按 seq 拉取）。
    SyncReq,
    /// 离线消息同步应答。
    SyncResp,
}

impl Cmd {
    /// 命令字的线上一字节值。
    #[must_use]
    pub const fn to_byte(self) -> u8 {
        match self {
            Cmd::Handshake => 0x01,
            Cmd::HandshakeAck => 0x02,
            Cmd::Ping => 0x03,
            Cmd::Pong => 0x04,
            Cmd::Msg => 0x05,
            Cmd::MsgAck => 0x06,
            Cmd::SyncReq => 0x07,
            Cmd::SyncResp => 0x08,
        }
    }
}

impl TryFrom<u8> for Cmd {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Cmd::Handshake),
            0x02 => Ok(Cmd::HandshakeAck),
            0x03 => Ok(Cmd::Ping),
            0x04 => Ok(Cmd::Pong),
            0x05 => Ok(Cmd::Msg),
            0x06 => Ok(Cmd::MsgAck),
            0x07 => Ok(Cmd::SyncReq),
            0x08 => Ok(Cmd::SyncResp),
            got => Err(ProtocolError::UnknownCommand { got }),
        }
    }
}

/// flags 位定义（按位或组合，预留高 5 位）。
pub mod flags {
    /// 载荷已压缩（阶段 5 引入 lz4/zstd 时启用）。
    pub const COMPRESSED: u8 = 0b0000_0001;

    /// 载荷已加密（阶段 7 引入 E2EE 时启用）。
    pub const ENCRYPTED: u8 = 0b0000_0010;
}

/// 固定头长度：magic(2) + version(1) + cmd(1) + flags(1)。
pub const HEADER_LEN: usize = 5;

/// 帧尾 CRC 长度。
pub const CRC_LEN: usize = 4;

/// 一个完整的协议帧（解码后的形态）。
///
/// `payload` 用 [`Bytes`]（共享缓冲区的廉价句柄，见 05-patterns 的享元模式）：
/// 从解码缓冲区 `freeze()` 而来，克隆只是引用计数 +1，
/// 后续扇出给多个订阅者时零拷贝。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// 命令字。
    pub cmd: Cmd,
    /// 标志位（见 [`flags`] 模块的位定义）。
    pub flags: u8,
    /// 本帧序号：发送方单调递增，用于 ACK 关联与去重窗口。
    pub seq: u64,
    /// 确认序号：累计确认"ack 之前的所有 seq 我已收到"（语义同 TCP ACK）。
    pub ack: u64,
    /// 载荷（零拷贝句柄）。
    pub payload: Bytes,
}

impl Frame {
    /// 便捷构造（`flags` 默认 0）。
    #[must_use]
    pub fn new(cmd: Cmd, seq: u64, ack: u64, payload: impl Into<Bytes>) -> Self {
        Self {
            cmd,
            flags: 0,
            seq,
            ack,
            payload: payload.into(),
        }
    }

    /// 本帧编码后的总字节数（调用方据此一次性分配准确的缓冲区）。
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        HEADER_LEN
            + varint::encoded_len(self.seq)
            + varint::encoded_len(self.ack)
            + varint::encoded_len(self.payload.len() as u64)
            + self.payload.len()
            + CRC_LEN
    }

    /// 编码本帧并追加到 `dst`。
    ///
    /// 编码是全函数（帧结构不含非法状态，非法值在构造/解码时已拦截），
    /// 因此没有失败路径——与解码的 `Result` 返回形成对照。
    pub fn encode_into(&self, dst: &mut impl BufMut) {
        let mut crc = Crc32::new();

        // 1. 固定头（栈上拼好，一次写入）
        let mut head = [0u8; HEADER_LEN];
        head[0..2].copy_from_slice(&crate::MAGIC.to_be_bytes());
        head[2] = crate::VERSION;
        head[3] = self.cmd.to_byte();
        head[4] = self.flags;
        dst.put_slice(&head);
        crc.update(&head);

        // 2. 三个 varint 字段（栈上暂存，避免为 1~10 字节的数据碰堆）
        let mut scratch = [0u8; MAX_VARINT_LEN];
        for value in [self.seq, self.ack, self.payload.len() as u64] {
            let written = {
                let mut slot: &mut [u8] = &mut scratch;
                varint::encode_u64(value, &mut slot)
            };
            dst.put_slice(&scratch[..written]);
            crc.update(&scratch[..written]);
        }

        // 3. 载荷
        dst.put_slice(&self.payload);
        crc.update(&self.payload);

        // 4. 帧尾 CRC（覆盖以上所有字节，大端）
        dst.put_u32(crc.finish());
    }

    /// 编码为独立的 [`Bytes`]（测试与小工具用；热路径请用 `encode_into` 复用缓冲区）。
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.encoded_len());
        self.encode_into(&mut buf);
        buf.freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ping_frame() -> Frame {
        Frame::new(Cmd::Ping, 1, 0, Bytes::new())
    }

    #[test]
    fn wire_layout_matches_spec() {
        // 与文档逐字节对拍：magic(2) + version(1) + cmd(1) + flags(1)
        // + seq varint(1) + ack varint(1) + len varint(1) + crc(4)
        let frame = ping_frame();
        let bytes = frame.encode();

        assert_eq!(bytes.len(), frame.encoded_len());
        assert_eq!(bytes.len(), 10); // 5 + 1 + 1 + 1 + 0 + 4（payload 为空）

        assert_eq!(&bytes[0..2], &[0x49, 0x4D]); // "IM"
        assert_eq!(bytes[2], crate::VERSION);
        assert_eq!(bytes[3], Cmd::Ping.to_byte());
        assert_eq!(bytes[4], 0); // flags
        assert_eq!(bytes[5], 1); // seq = 1
        assert_eq!(bytes[6], 0); // ack = 0
        assert_eq!(bytes[7], 0); // len = 0
        // 尾部 4 字节 = 头 7 字节的 CRC32
        let expect = crate::crc32::checksum(&bytes[..7]);
        assert_eq!(&bytes[8..12], &expect.to_be_bytes());
    }

    #[test]
    fn large_seq_takes_varint_space() {
        // 大 seq（u64::MAX）编码为 10 字节 varint——encoded_len 必须算对
        let frame = Frame::new(Cmd::Msg, u64::MAX, 300, Bytes::new());
        let bytes = frame.encode();
        // 5 头 + 10(seq) + 2(ack=300) + 1(len) + 0 + 4 crc
        assert_eq!(bytes.len(), 22);
        assert_eq!(frame.encoded_len(), 22);
    }

    #[test]
    fn cmd_roundtrip() {
        for cmd in [
            Cmd::Handshake,
            Cmd::HandshakeAck,
            Cmd::Ping,
            Cmd::Pong,
            Cmd::Msg,
            Cmd::MsgAck,
            Cmd::SyncReq,
            Cmd::SyncResp,
        ] {
            assert_eq!(Cmd::try_from(cmd.to_byte()), Ok(cmd));
        }
        assert!(matches!(
            Cmd::try_from(0xFF),
            Err(ProtocolError::UnknownCommand { got: 0xFF })
        ));
    }
}
