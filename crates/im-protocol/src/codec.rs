//! 帧解码器：TCP 字节流 → 帧的增量状态机。
//!
//! # 为什么必须是状态机（算法图谱第三项：粘包/半包处理）
//!
//! TCP 是**字节流**，不保证"一次 read == 一帧"：
//! 一段到达的数据可能包含半帧、正好一帧、或两帧半。
//! 解码器必须**增量**消费，且在任意切分点续传时保持正确。
//!
//! 朴素做法是"攒够一帧再整体解析"，但帧长要读完 header 的 varint 才知道，
//! 半包时已读的字节怎么办？——把"解析到哪了"编码进**类型**：
//! [`Stage`] 枚举的每个变体只消费自己需要的字节，
//! 穷尽性检查保证新增阶段不会漏处理（05-patterns/03 的 enum 状态机实战）。
//!
//! # 防护性设计
//!
//! - **长度上限**：`len` 字段解码完成时、**分配内存之前**校验上限
//!   （[`FrameDecoder::with_max_frame_len`]），恶意长度字段无法撑爆内存；
//! - **早失败**：magic/版本/命令字在头 5 字节内校验，错流尽早暴露；
//! - **CRC 增量累积**：每消费一段字节就喂给 [`Crc32`]，帧尾到达即得整帧校验值，
//!   不回头重扫。

use std::mem;

use bytes::{Buf, Bytes, BytesMut};

use crate::crc32::Crc32;
use crate::error::ProtocolError;
use crate::frame::{CRC_LEN, Cmd, Frame, HEADER_LEN};
use crate::varint::VarIntDecoder;

/// 默认单帧上限：1 MiB。
///
/// IM 消息（文本 + 元数据）远小于此；图片/文件走阶段 5 的分块传输，
/// 不经过超大帧。上限的意义是**拒绝异常**，不是承载业务。
pub const DEFAULT_MAX_FRAME_LEN: usize = 1024 * 1024;

/// 增量帧解码器：一个连接一个实例。
///
/// # Examples
///
/// ```
/// use bytes::Bytes;
/// use im_protocol::{Cmd, Frame, FrameDecoder};
///
/// let frame = Frame::new(Cmd::Msg, 1, 0, Bytes::from_static(b"hello"));
/// let wire = frame.encode();
///
/// let mut decoder = FrameDecoder::new();
/// // 模拟 TCP 半包：先到一半
/// let half = wire.len() / 2;
/// assert!(decoder.decode(&wire[..half]).unwrap().is_empty());
/// // 后一半到达，整帧解出
/// let frames = decoder.decode(&wire[half..]).unwrap();
/// assert_eq!(frames, vec![frame]);
/// ```
pub struct FrameDecoder {
    /// 单帧 payload 上限（防 DoS，见类型级注释）。
    max_frame_len: usize,
    /// 已到达、尚未解析完的字节（可能同时压着多帧）。
    buf: BytesMut,
    /// 进行中的帧（`None` = 等待下一个帧头）。
    current: Option<Partial>,
}

/// 半帧：固定头已验证，变长部分解析到一半。
struct Partial {
    cmd: Cmd,
    flags: u8,
    seq: u64,
    ack: u64,
    /// 覆盖"已消费的所有帧字节"的增量 CRC。
    crc: Crc32,
    /// 解析进度——状态机的"状态"。
    stage: Stage,
}

/// 状态机的阶段。每个变体持有该阶段需要的数据。
enum Stage {
    /// 等待 `seq` 的 varint。
    Seq(VarIntDecoder),
    /// 等待 `ack` 的 varint。
    Ack(VarIntDecoder),
    /// 等待 payload 长度的 varint（本阶段做上限校验）。
    Len(VarIntDecoder),
    /// 长度已确认，正在收集 payload。
    Payload {
        /// 已收到的载荷。
        payload: BytesMut,
        /// 还差多少字节收齐。
        remaining: usize,
    },
    /// payload 已收齐（`freeze` 成廉价句柄），等待 4 字节 CRC。
    Crc {
        payload: Bytes,
        /// 收到的 CRC 字节与期望的累计进度。
        crc: [u8; CRC_LEN],
        filled: usize,
    },
}

impl FrameDecoder {
    /// 以默认上限（[`DEFAULT_MAX_FRAME_LEN`]）创建。
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_frame_len(DEFAULT_MAX_FRAME_LEN)
    }

    /// 以自定义单帧上限创建。
    #[must_use]
    pub fn with_max_frame_len(max_frame_len: usize) -> Self {
        Self { max_frame_len, buf: BytesMut::new(), current: None }
    }

    /// 喂入一段原始字节（来自一次 TCP read），返回本次解出的所有完整帧。
    ///
    /// 空返回值 = 半包（数据不够，已保存在内部缓冲区，等待下次喂入）。
    ///
    /// # Errors
    ///
    /// 流不合法（magic/版本/命令字/长度超限/varint 超长/CRC 失败）时返回
    /// [`ProtocolError`]。**出错后解码器状态已不可用**，
    /// 调用方应断开连接——半途而废的帧没有恢复价值（阶段 2 会加重同步策略）。
    pub fn decode(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, ProtocolError> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(frame) = self.next_frame()? {
            frames.push(frame);
        }
        Ok(frames)
    }

    /// 尝试推进状态机解出一帧；`Ok(None)` 表示字节耗尽（半包）。
    fn next_frame(&mut self) -> Result<Option<Frame>, ProtocolError> {
        // ── 阶段 0：没有进行中的帧 → 先收齐并校验固定头 ──
        if self.current.is_none() {
            if self.buf.len() < HEADER_LEN {
                return Ok(None); // 连头都不齐
            }
            let header = self.buf.split_to(HEADER_LEN);

            let magic = u16::from_be_bytes([header[0], header[1]]);
            if magic != crate::MAGIC {
                return Err(ProtocolError::BadMagic { expected: crate::MAGIC, got: magic });
            }
            if header[2] != crate::VERSION {
                return Err(ProtocolError::UnsupportedVersion {
                    got: header[2],
                    supported: crate::VERSION,
                });
            }
            let cmd = Cmd::try_from(header[3])?;

            let mut crc = Crc32::new();
            crc.update(&header);
            self.current = Some(Partial {
                cmd,
                flags: header[4],
                seq: 0,
                ack: 0,
                crc,
                stage: Stage::Seq(VarIntDecoder::default()),
            });
        }

        // ── 阶段 1~5：推进进行中的帧（buf 与 current 是不相交字段，可同时可变借用） ──
        let Some(partial) = self.current.as_mut() else {
            unreachable!("上方刚放入 Some");
        };
        match advance(partial, &mut self.buf, self.max_frame_len)? {
            Some(frame) => {
                self.current = None; // 本帧完成，回到"等帧头"
                Ok(Some(frame))
            }
            None => Ok(None),
        }
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// 状态机推进核心：尽可能多地消费 `buf`，返回 `Some(frame)` 表示一帧完成。
///
/// 用 `mem::replace` 把 stage **取出来**处理再放回去——
/// 这样每个转移分支都能按值持有旧阶段的数据（如半满的 payload）。
fn advance(
    partial: &mut Partial,
    buf: &mut BytesMut,
    max_frame_len: usize,
) -> Result<Option<Frame>, ProtocolError> {
    loop {
        match mem::replace(&mut partial.stage, Stage::Seq(VarIntDecoder::default())) {
            Stage::Seq(mut dec) => {
                let Some(v) = feed_varint(&mut dec, buf, &mut partial.crc)? else {
                    partial.stage = Stage::Seq(dec);
                    return Ok(None);
                };
                partial.seq = v;
                partial.stage = Stage::Ack(VarIntDecoder::default());
            }
            Stage::Ack(mut dec) => {
                let Some(v) = feed_varint(&mut dec, buf, &mut partial.crc)? else {
                    partial.stage = Stage::Ack(dec);
                    return Ok(None);
                };
                partial.ack = v;
                partial.stage = Stage::Len(VarIntDecoder::default());
            }
            Stage::Len(mut dec) => {
                let Some(v) = feed_varint(&mut dec, buf, &mut partial.crc)? else {
                    partial.stage = Stage::Len(dec);
                    return Ok(None);
                };
                // 防护点：分配之前校验上限（usize::try_from 同时挡住 32 位平台的溢出）
                if v > max_frame_len as u64 {
                    return Err(ProtocolError::FrameTooLarge { got: v, max: max_frame_len });
                }
                let len = usize::try_from(v)
                    .map_err(|_| ProtocolError::FrameTooLarge { got: v, max: max_frame_len })?;
                partial.stage =
                    Stage::Payload { payload: BytesMut::with_capacity(len), remaining: len };
            }
            Stage::Payload { mut payload, remaining } => {
                if buf.is_empty() {
                    partial.stage = Stage::Payload { payload, remaining };
                    return Ok(None);
                }
                let take = remaining.min(buf.len());
                let chunk = buf.split_to(take);
                partial.crc.update(&chunk);
                payload.extend_from_slice(&chunk);
                if take == remaining {
                    // 载荷收齐：freeze 成共享句柄（享元模式：后续克隆零拷贝）
                    partial.stage =
                        Stage::Crc { payload: payload.freeze(), crc: [0; CRC_LEN], filled: 0 };
                } else {
                    partial.stage = Stage::Payload { payload, remaining: remaining - take };
                }
            }
            Stage::Crc { payload, mut crc, mut filled } => {
                if buf.is_empty() {
                    partial.stage = Stage::Crc { payload, crc, filled };
                    return Ok(None);
                }
                // 注意：CRC 字节本身不参与 CRC 计算（它校验的是前面的字节）
                let take = (CRC_LEN - filled).min(buf.len());
                let chunk = buf.split_to(take);
                crc[filled..filled + take].copy_from_slice(&chunk);
                filled += take;
                if filled < CRC_LEN {
                    partial.stage = Stage::Crc { payload, crc, filled };
                    return Ok(None);
                }

                let got = u32::from_be_bytes(crc);
                let expected = partial.crc.finish();
                if got != expected {
                    return Err(ProtocolError::CrcMismatch { expected, got });
                }
                return Ok(Some(Frame {
                    cmd: partial.cmd,
                    flags: partial.flags,
                    seq: partial.seq,
                    ack: partial.ack,
                    payload,
                }));
            }
        }
    }
}

/// 持续从 `buf` 头部喂入 varint 解码器，直到 varint 结束**或缓冲区耗尽**。
///
/// `Ok(Some(v))` = varint 结束；`Ok(None)` = 缓冲区耗尽且未结束
/// （解码器进度保留在 `dec` 里，下次继续）。
/// 每个消费的字节同步喂 CRC。
///
/// 注意与“只吐一个字节”的写法区别：那会把「varint 未结束」误当成
/// 「没字节了」提前退出，多字节 varint 后面还有字节时就会错误地等待。
fn feed_varint(
    dec: &mut VarIntDecoder,
    buf: &mut BytesMut,
    crc: &mut Crc32,
) -> Result<Option<u64>, ProtocolError> {
    while !buf.is_empty() {
        let byte = buf.get_u8();
        crc.update(&[byte]);
        if let Some(v) = dec.push(byte)? {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use proptest::prelude::*;

    use crate::frame::flags;

    fn msg_frame(seq: u64, payload: &[u8]) -> Frame {
        Frame::new(Cmd::Msg, seq, 0, Bytes::copy_from_slice(payload))
    }

    #[test]
    fn single_frame_roundtrip() {
        let frame = msg_frame(42, b"hello rust-im");
        let mut decoder = FrameDecoder::new();
        let frames = decoder.decode(&frame.encode()).unwrap();
        assert_eq!(frames, vec![frame]);
    }

    #[test]
    fn byte_by_byte_is_immune_to_half_packets() {
        // 最狠的半包测试：一次只喂一个字节，任意切分都不出错
        let frame = msg_frame(u64::MAX, b"half-packet-proof"); // 大 seq：10 字节 varint
        let wire = frame.encode();

        let mut decoder = FrameDecoder::new();
        let mut got: Vec<Frame> = Vec::new();
        for (i, byte) in wire.iter().enumerate() {
            let frames = decoder.decode(std::slice::from_ref(byte)).unwrap();
            if i + 1 < wire.len() {
                assert!(frames.is_empty(), "最后一字节前不应有完整帧");
            } else {
                got = frames; // 最后一字节到达时帧完整解出
            }
        }
        assert_eq!(got, vec![frame]);
        assert!(decoder.decode(&[]).unwrap().is_empty(), "缓冲区应已清空");
    }

    #[test]
    fn two_frames_in_one_chunk() {
        // 粘包：两帧挤在同一次 read 里
        let a = Frame::new(Cmd::Ping, 1, 0, Bytes::new());
        let b = msg_frame(2, b"second");
        let mut wire = a.encode().to_vec();
        wire.extend_from_slice(&b.encode());

        let mut decoder = FrameDecoder::new();
        let frames = decoder.decode(&wire).unwrap();
        assert_eq!(frames, vec![a, b]);
    }

    #[test]
    fn flags_survive_roundtrip() {
        // flags 非零（压缩位 | 加密位）
        let mut frame = msg_frame(7, b"x");
        frame.flags = flags::COMPRESSED | flags::ENCRYPTED;
        let mut decoder = FrameDecoder::new();
        let frames = decoder.decode(&frame.encode()).unwrap();
        assert_eq!(frames[0].flags, 0b0000_0011);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut wire = msg_frame(1, b"hi").encode().to_vec();
        wire[0] = 0x00; // 破坏 magic 高字节
        let mut decoder = FrameDecoder::new();
        assert!(matches!(decoder.decode(&wire), Err(ProtocolError::BadMagic { .. })));
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let mut wire = msg_frame(1, b"hi").encode().to_vec();
        wire[2] = crate::VERSION + 1;
        let mut decoder = FrameDecoder::new();
        assert!(matches!(decoder.decode(&wire), Err(ProtocolError::UnsupportedVersion { .. })));
    }

    #[test]
    fn unknown_command_is_rejected() {
        let mut wire = msg_frame(1, b"hi").encode().to_vec();
        wire[3] = 0xEE;
        let mut decoder = FrameDecoder::new();
        assert!(matches!(decoder.decode(&wire), Err(ProtocolError::UnknownCommand { got: 0xEE })));
    }

    #[test]
    fn frame_too_large_rejected_before_allocation() {
        // 声明一个 100 字节的 payload，但上限设为 8 —— 在分配前拒绝
        let frame = msg_frame(1, &[0u8; 100]);
        let mut decoder = FrameDecoder::with_max_frame_len(8);
        assert!(matches!(
            decoder.decode(&frame.encode()),
            Err(ProtocolError::FrameTooLarge { got: 100, max: 8 })
        ));
    }

    #[test]
    fn crc_corruption_is_detected() {
        let mut wire = msg_frame(9, b"integrity").encode().to_vec();
        *wire.last_mut().expect("非空") ^= 0x01; // 翻转 CRC 最低位
        let mut decoder = FrameDecoder::new();
        assert!(matches!(decoder.decode(&wire), Err(ProtocolError::CrcMismatch { .. })));
    }

    #[test]
    fn payload_corruption_is_detected() {
        let mut wire = msg_frame(9, b"integrity").encode().to_vec();
        wire[8] ^= 0x01; // 翻转 payload 首字节（CRC 字节本身不动）
        let mut decoder = FrameDecoder::new();
        assert!(matches!(decoder.decode(&wire), Err(ProtocolError::CrcMismatch { .. })));
    }

    #[test]
    fn truncated_frame_yields_nothing() {
        let wire = msg_frame(3, b"abc").encode();
        let mut decoder = FrameDecoder::new();
        // 喂入除最后一字节外的全部
        assert!(decoder.decode(&wire[..wire.len() - 1]).unwrap().is_empty());
        // 补上最后一字节 → 出帧
        assert_eq!(decoder.decode(&wire[wire.len() - 1..]).unwrap().len(), 1);
    }

    #[test]
    fn empty_decode_call_is_harmless() {
        let mut decoder = FrameDecoder::new();
        assert!(decoder.decode(&[]).unwrap().is_empty());
    }

    proptest! {
        /// 性质测试：任意 seq/ack/payload 与任意切分点，解码结果必须与原帧一致。
        /// 这正是"增量状态机对切分点不敏感"的数学化表达。
        #[test]
        fn roundtrip_at_arbitrary_split(
            seq in any::<u64>(),
            ack in any::<u64>(),
            payload in any::<Vec<u8>>(),
            split in 0..64usize,
        ) {
            let frame = Frame::new(Cmd::Msg, seq, ack, Bytes::from(payload));
            let wire = frame.encode();
            // 切分点限制在「真正的半包」范围内：0 <= split < wire.len()
            let split = split.min(wire.len() - 1);

            let mut decoder = FrameDecoder::new();
            let first = decoder.decode(&wire[..split]).unwrap();
            prop_assert!(first.is_empty(), "半帧不应产出: split={split}");

            let second = decoder.decode(&wire[split..]).unwrap();
            prop_assert_eq!(second, vec![frame]);
        }

        /// 三段切分 + 连发两帧：更接近真实 TCP 的到达模式
        #[test]
        fn two_frames_three_chunks(
            payload_a in any::<Vec<u8>>(),
            payload_b in any::<Vec<u8>>(),
            s1 in 0..32usize,
            s2 in 0..32usize,
        ) {
            let a = Frame::new(Cmd::Msg, 1, 0, Bytes::from(payload_a));
            let b = Frame::new(Cmd::MsgAck, 2, 1, Bytes::from(payload_b));
            let mut wire = a.encode().to_vec();
            wire.extend_from_slice(&b.encode());

            let s1 = s1.min(wire.len());
            let s2 = s1 + s2.min(wire.len() - s1);

            let mut decoder = FrameDecoder::new();
            let mut got: Vec<Frame> = Vec::new();
            got.extend(decoder.decode(&wire[..s1]).unwrap());
            got.extend(decoder.decode(&wire[s1..s2]).unwrap());
            got.extend(decoder.decode(&wire[s2..]).unwrap());
            prop_assert_eq!(got, vec![a, b]);
        }
    }
}
