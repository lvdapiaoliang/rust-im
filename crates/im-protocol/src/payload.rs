//! payload 编解码：帧的「内容」格式（阶段 3）。
//!
//! 帧头（`frame.rs`）解决「一条消息的边界与完整性」；
//! 本模块解决「payload 里的字节怎么读出结构化字段」——
//! 与 Protobuf 的 message 同一层的「手工版」。
//!
//! # 设计决定
//!
//! - **全部字段用 varint**：与帧头同一编码（复用 [`crate::varint`]），
//!   user_id/msg_id 这类小数值常态下 1~4 字节；
//! - **字符串/字节串 = 长度前缀 + 内容**（`len:varint + bytes`），
//!   与 Protobuf wire format 一致；
//! - **解码器是 `&mut &[u8]` 游标**：slice 的「消费」就是重新赋值，
//!   零拷贝、无下标算术——这是 Rust 里最优雅的 cursor 写法；
//! - **解码完必须恰好耗尽**：多余的尾部字节说明两端格式不一致，
//!   早失败优于静默忽略（[`ProtocolError::TrailingBytes`]）；
//! - **每个类型关联自己的 `Cmd`**（[`Payload::CMD`]）：
//!   业务层编码时不再手写命令字，错配在类型层就被挡住。
//!
//! # 算法/模式图谱落点
//!
//! - varint 编码的第二次实战（第一次在帧头）；
//! - NEWTYPE 式薄封装（游标 [`Reader`]）；
//! - 错误即类型：截断/坏 UTF-8/尾部多余各有独立变体。

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::ProtocolError;
use crate::frame::Cmd;
use crate::varint;

/// 一个可编码进帧 payload 的消息体。
///
/// 实现方自带命令字（[`Payload::CMD`]），
/// `encode_frame` 由此构造完整的 [`Frame`]——
/// 「帧的动词与名词由同一处定义」，杜绝「Cmd::Msg 配上握手载荷」的错配。
pub trait Payload: Sized {
    /// 本载荷对应的命令字。
    const CMD: Cmd;

    /// 编码进 `dst`（追加写）。
    fn encode_into(&self, dst: &mut impl BufMut);

    /// 编码为独立的 [`Bytes`]（`Frame::new` 直接收）。
    fn encode(&self) -> Bytes {
        let mut buf = BytesMut::new();
        self.encode_into(&mut buf);
        buf.freeze()
    }

    /// 从 `src` 解码；`src` 必须恰好耗尽（见模块文档）。
    ///
    /// # Errors
    ///
    /// 载荷截断、UTF-8 非法或存在尾部多余字节时返回 [`ProtocolError`]。
    fn decode(src: &[u8]) -> Result<Self, ProtocolError>;

    /// 编码为一个完整帧（seq/ack 由发送侧填写）。
    #[must_use]
    fn encode_frame(&self, seq: u64, ack: u64) -> crate::Frame {
        crate::Frame::new(Self::CMD, seq, ack, self.encode())
    }

    /// 从一个帧解码（校验命令字匹配后解析 payload）。
    ///
    /// # Errors
    ///
    /// 帧的命令字与本类型不符，或 payload 解析失败时返回 [`ProtocolError`]。
    fn decode_frame(frame: &crate::Frame) -> Result<Self, ProtocolError> {
        if frame.cmd != Self::CMD {
            return Err(ProtocolError::UnknownCommand {
                got: frame.cmd.to_byte(),
            });
        }
        Self::decode(&frame.payload)
    }
}

/// 解码游标：`&mut &[u8]` 的薄封装。
///
/// 「读走一段」就是把这个引用重新指向剩余部分——
/// 不需要下标，不需要拷贝，`src.len() == 0` 天然就是「耗尽」。
struct Reader<'a> {
    src: &'a mut &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(src: &'a mut &'a [u8]) -> Self {
        Self { src }
    }

    /// 剩余字节数。
    fn remaining(&self) -> usize {
        self.src.len()
    }

    /// 读一个 varint（复用阶段 1 的一次性解码器）。
    fn varint(&mut self) -> Result<u64, ProtocolError> {
        let (value, used) =
            varint::decode_u64(self.src).ok_or(ProtocolError::PayloadTooShort {
                need: 1,
                got: 0,
            })?;
        *self.src = &self.src[used..];
        Ok(value)
    }

    /// 读「长度前缀 + 字节串」，零拷贝地切出一段 `&[u8]`。
    fn bytes(&mut self) -> Result<&'a [u8], ProtocolError> {
        let len = self.varint()?;
        let len = usize::try_from(len).map_err(|_| ProtocolError::PayloadTooShort {
            need: usize::MAX,
            got: self.src.len(),
        })?;
        if self.src.len() < len {
            return Err(ProtocolError::PayloadTooShort {
                need: len - self.src.len(),
                got: self.src.len(),
            });
        }
        let (head, tail) = self.src.split_at(len);
        *self.src = tail;
        Ok(head)
    }

    /// 读一个 UTF-8 字符串（内容克隆为 `String`，UTF-8 逐字节校验）。
    fn string(&mut self) -> Result<String, ProtocolError> {
        let raw = self.bytes()?;
        String::from_utf8(raw.to_vec()).map_err(|_| ProtocolError::InvalidUtf8)
    }

    /// 断言恰好耗尽（[`Payload::decode`] 的收尾必调）。
    fn finish(self) -> Result<(), ProtocolError> {
        if self.src.is_empty() {
            Ok(())
        } else {
            Err(ProtocolError::TrailingBytes {
                extra: self.src.len(),
            })
        }
    }
}

/// 便利写函数：长度前缀 + 字节串。
fn put_bytes(dst: &mut impl BufMut, bytes: &[u8]) {
    varint::encode_u64(bytes.len() as u64, dst);
    dst.put_slice(bytes);
}

/// 写函数：长度前缀 + UTF-8 字符串。
fn put_string(dst: &mut impl BufMut, s: &str) {
    put_bytes(dst, s.as_bytes());
}

// ────────────────────────────────────────────────────────────────
// 各命令字的载荷结构
// ────────────────────────────────────────────────────────────────

/// `Handshake`（客户端 → 服务端）：登录凭证。
///
/// `user_id` + `token` 的极简认证：token 明文对照（阶段 7 TLS + 真正的
/// 挑战-应答认证时升级）。字段刻意最少——握手要快，移动端弱网下
/// 每个字节都是延迟。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// 申请登录的用户 ID。
    pub user_id: u64,
    /// 认证令牌（阶段 3 仅做字符串比对）。
    pub token: String,
}

impl Payload for Handshake {
    const CMD: Cmd = Cmd::Handshake;

    fn encode_into(&self, dst: &mut impl BufMut) {
        varint::encode_u64(self.user_id, dst);
        put_string(dst, &self.token);
    }

    fn decode(src: &[u8]) -> Result<Self, ProtocolError> {
        let mut cursor: &[u8] = src;
        let mut r = Reader::new(&mut cursor);
        let user_id = r.varint()?;
        let token = r.string()?;
        r.finish()?;
        Ok(Self { user_id, token })
    }
}

/// `HandshakeAck`（服务端 → 客户端）：登录结果。
///
/// 成功时 `session_id` 是服务端分配的会话 ID（雪花 ID，见 im-server）；
/// 失败时为 0 且 `reason` 说明原因。拒绝也走协议而非直接断连——
/// 客户端需要区分「密码错了」和「网络断了」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeAck {
    /// 会话 ID；0 表示拒绝。
    pub session_id: u64,
    /// 拒绝原因（成功时为空串）。
    pub reason: String,
}

impl HandshakeAck {
    /// 构造「接受」应答。
    #[must_use]
    pub fn accepted(session_id: u64) -> Self {
        Self {
            session_id,
            reason: String::new(),
        }
    }

    /// 构造「拒绝」应答。
    #[must_use]
    pub fn rejected(reason: impl Into<String>) -> Self {
        Self {
            session_id: 0,
            reason: reason.into(),
        }
    }

    /// 是否被接受。
    #[must_use]
    pub fn is_accepted(&self) -> bool {
        self.session_id != 0
    }
}

impl Payload for HandshakeAck {
    const CMD: Cmd = Cmd::HandshakeAck;

    fn encode_into(&self, dst: &mut impl BufMut) {
        varint::encode_u64(self.session_id, dst);
        put_string(dst, &self.reason);
    }

    fn decode(src: &[u8]) -> Result<Self, ProtocolError> {
        let mut cursor: &[u8] = src;
        let mut r = Reader::new(&mut cursor);
        let session_id = r.varint()?;
        let reason = r.string()?;
        r.finish()?;
        Ok(Self { session_id, reason })
    }
}

/// `Msg` 载荷：单聊消息体（上行与下行共用同一格式）。
///
/// - 上行（客户端 → 服务端）：`from` 填 0，服务端以会话身份覆盖——
///   **消息发送者由服务端裁决**，客户端伪造 from 无效；
/// - 下行（服务端 → 客户端）：`from` 为真实发送者；
/// - `msg_id` 是**服务端分配的全局消息 ID**（雪花）：接收端用它去重与
///   排序，离线同步按它游标拉取；
/// - `content` 用 [`Bytes`]：服务端转发时直接传递零拷贝句柄（享元）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg {
    /// 发送者用户 ID（下行时为真实值）。
    pub from: u64,
    /// 接收者用户 ID。
    pub to: u64,
    /// 服务端分配的全局消息 ID（雪花 ID）。
    pub msg_id: u64,
    /// 消息内容（零拷贝句柄）。
    pub content: Bytes,
}

impl Payload for Msg {
    const CMD: Cmd = Cmd::Msg;

    fn encode_into(&self, dst: &mut impl BufMut) {
        varint::encode_u64(self.from, dst);
        varint::encode_u64(self.to, dst);
        varint::encode_u64(self.msg_id, dst);
        put_bytes(dst, &self.content);
    }

    fn decode(src: &[u8]) -> Result<Self, ProtocolError> {
        let mut cursor: &[u8] = src;
        let mut r = Reader::new(&mut cursor);
        let from = r.varint()?;
        let to = r.varint()?;
        let msg_id = r.varint()?;
        let content = Bytes::copy_from_slice(r.bytes()?);
        r.finish()?;
        Ok(Self {
            from,
            to,
            msg_id,
            content,
        })
    }
}

/// `MsgAck` 载荷：服务端对一条上行消息的确认。
///
/// 帧头的 `ack` 字段是**帧级**累计确认（传输层）；这里的 `msg_id` 是
/// **消息级**确认（业务层：服务端已落盘/已排队投递）。两层确认
/// 各管各的语义，不要合并——这正是 TCP 的 ACK 与 HTTP 的 200 的关系。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsgAck {
    /// 被确认的消息 ID。
    pub msg_id: u64,
}

impl Payload for MsgAck {
    const CMD: Cmd = Cmd::MsgAck;

    fn encode_into(&self, dst: &mut impl BufMut) {
        varint::encode_u64(self.msg_id, dst);
    }

    fn decode(src: &[u8]) -> Result<Self, ProtocolError> {
        let mut cursor: &[u8] = src;
        let mut r = Reader::new(&mut cursor);
        let msg_id = r.varint()?;
        r.finish()?;
        Ok(Self { msg_id })
    }
}

/// `SyncReq` 载荷：离线消息拉取请求。
///
/// `since` 是「我已经收到的最大 msg_id」，服务端返回它之后的消息——
/// 游标式同步：断点续传天然成立（重连再发一次同样的 SyncReq 即可）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReq {
    /// 游标：拉取 msg_id 严格大于此值的消息。
    pub since: u64,
}

impl Payload for SyncReq {
    const CMD: Cmd = Cmd::SyncReq;

    fn encode_into(&self, dst: &mut impl BufMut) {
        varint::encode_u64(self.since, dst);
    }

    fn decode(src: &[u8]) -> Result<Self, ProtocolError> {
        let mut cursor: &[u8] = src;
        let mut r = Reader::new(&mut cursor);
        let since = r.varint()?;
        r.finish()?;
        Ok(Self { since })
    }
}

/// `SyncResp` 载荷：一批离线消息。
///
/// 多条消息打包进一帧（而非每条一帧）：拉取是批处理场景，
/// 帧头的固定成本（12 字节起步）不该按条数翻倍。
/// 空列表 = 「没有更多了」，客户端据此停止分页。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncResp {
    /// 本批消息（`msg_id` 升序）。
    pub messages: Vec<Msg>,
}

impl Payload for SyncResp {
    const CMD: Cmd = Cmd::SyncResp;

    fn encode_into(&self, dst: &mut impl BufMut) {
        varint::encode_u64(self.messages.len() as u64, dst);
        for msg in &self.messages {
            msg.encode_into(dst);
        }
    }

    fn decode(src: &[u8]) -> Result<Self, ProtocolError> {
        let mut cursor: &[u8] = src;
        let mut r = Reader::new(&mut cursor);
        let count = r.varint()?;
        let count = usize::try_from(count).map_err(|_| ProtocolError::PayloadTooShort {
            need: usize::MAX,
            got: r.remaining(),
        })?;
        let mut messages = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            messages.push(Msg::decode(r.bytes()?)?);
        }
        r.finish()?;
        Ok(Self { messages })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::flags;

    /// 全载荷类型的「编码 → 帧往返 → 解码」一致性
    #[test]
    fn roundtrip_all_payloads() {
        let handshake = Handshake {
            user_id: 42,
            token: "secret-token-你好".into(),
        };
        assert_eq!(Handshake::decode(&handshake.encode()).unwrap(), handshake);

        let ack = HandshakeAck::accepted(12345678901234567890);
        assert_eq!(HandshakeAck::decode(&ack.encode()).unwrap(), ack);
        let rejected = HandshakeAck::rejected("bad token");
        assert_eq!(HandshakeAck::decode(&rejected.encode()).unwrap(), rejected);
        assert!(!rejected.is_accepted());

        let msg = Msg {
            from: 1,
            to: 2,
            msg_id: u64::MAX,
            content: Bytes::from_static(b"hello, \xE4\xB8\x96\xE7\x95\x8C"),
        };
        assert_eq!(Msg::decode(&msg.encode()).unwrap(), msg);

        let sync_resp = SyncResp {
            messages: vec![msg.clone(), Msg {
                from: 3,
                to: 4,
                msg_id: 5,
                content: Bytes::new(),
            }],
        };
        assert_eq!(SyncResp::decode(&sync_resp.encode()).unwrap(), sync_resp);
        // 空列表也是合法载荷（「没有更多了」）
        let empty = SyncResp { messages: vec![] };
        assert_eq!(SyncResp::decode(&empty.encode()).unwrap(), empty);
    }

    /// 帧级往返：encode_frame → decode_frame
    #[test]
    fn frame_roundtrip_via_trait() {
        let msg = Msg {
            from: 7,
            to: 8,
            msg_id: 9,
            content: Bytes::from_static(b"via-frame"),
        };
        let frame = msg.encode_frame(3, 0);
        assert_eq!(frame.cmd, Cmd::Msg);
        assert_eq!(frame.seq, 3);
        assert_eq!(Msg::decode_frame(&frame).unwrap(), msg);
    }

    /// 命令字错配：拿 Msg 帧去解 Handshake 必须报错
    #[test]
    fn cmd_mismatch_is_rejected() {
        let frame = Msg {
            from: 1,
            to: 2,
            msg_id: 3,
            content: Bytes::new(),
        }
        .encode_frame(1, 0);
        assert!(matches!(
            Handshake::decode_frame(&frame),
            Err(ProtocolError::UnknownCommand { got: 0x05 })
        ));
    }

    /// 截断载荷：每个字段序列都被砍一刀
    #[test]
    fn truncated_payload_is_rejected() {
        let handshake = Handshake {
            user_id: 42,
            token: "token".into(),
        };
        let wire = handshake.encode();
        for cut in 0..wire.len() {
            assert!(
                matches!(
                    Handshake::decode(&wire[..cut]),
                    Err(ProtocolError::PayloadTooShort { .. })
                ),
                "截断到 {cut} 字节应报 PayloadTooShort"
            );
        }
    }

    /// 尾部多余字节：完整载荷后追加垃圾必须报 TrailingBytes
    #[test]
    fn trailing_bytes_are_rejected() {
        let mut wire = MsgAck { msg_id: 1 }.encode().to_vec();
        wire.extend_from_slice(&[0xDE, 0xAD]);
        assert!(matches!(
            MsgAck::decode(&wire),
            Err(ProtocolError::TrailingBytes { extra: 2 })
        ));
    }

    /// 坏 UTF-8：token 字节序列非法时报 InvalidUtf8
    #[test]
    fn invalid_utf8_is_rejected() {
        // 手工拼载荷：user_id=1 + len=2 + 0xFF 0xFE（非法 UTF-8）
        let mut wire = Vec::new();
        varint::encode_u64(1, &mut wire);
        varint::encode_u64(2, &mut wire);
        wire.extend_from_slice(&[0xFF, 0xFE]);
        assert!(matches!(
            Handshake::decode(&wire),
            Err(ProtocolError::InvalidUtf8)
        ));
    }

    /// varint 的小数字红利：小 user_id 的握手载荷只有几个字节
    #[test]
    fn small_ids_stay_compact() {
        let hs = Handshake {
            user_id: 1,
            token: String::new(),
        };
        // user_id(1) + len(1) + 空 token = 2 字节
        assert_eq!(hs.encode().len(), 2);
    }

    /// proptest：任意 Msg 字段组合的编码-解码往返一致性
    #[test]
    fn msg_roundtrip_property() {
        use proptest::prelude::*;

        proptest!(|(from in any::<u64>(), to in any::<u64>(), msg_id in any::<u64>(), content in any::<Vec<u8>>())| {
            let msg = Msg {
                from,
                to,
                msg_id,
                content: Bytes::from(content),
            };
            prop_assert_eq!(Msg::decode(&msg.encode()).unwrap(), msg);
        });
    }

    /// SyncResp 大批量消息（模拟离线堆积）的往返
    #[test]
    fn sync_resp_batch_roundtrip() {
        let messages: Vec<Msg> = (0..500u64)
            .map(|i| Msg {
                from: 1,
                to: 2,
                msg_id: 1000 + i,
                content: Bytes::from(format!("offline-{i}")),
            })
            .collect();
        let resp = SyncResp { messages };
        let decoded = SyncResp::decode(&resp.encode()).unwrap();
        assert_eq!(decoded.messages.len(), 500);
        assert_eq!(decoded.messages[499].msg_id, 1499);
    }

    /// 与帧编码组合：带 flags 的完整帧仍能解出 payload
    #[test]
    fn payload_survives_flagged_frame() {
        let mut frame = Msg {
            from: 1,
            to: 2,
            msg_id: 3,
            content: Bytes::from_static(b"flagged"),
        }
        .encode_frame(1, 0);
        frame.flags = flags::COMPRESSED;
        let decoded = Msg::decode_frame(&frame).unwrap();
        assert_eq!(decoded.content, Bytes::from_static(b"flagged"));
    }
}
