//! varint（变长整数）与 zigzag 编码——rust-im 算法图谱第一项落地。
//!
//! # 为什么需要 varint
//!
//! IM 帧里的 seq/ack/长度字段绝大多数是小数字（心跳的 seq 每次只加一，
//! 典型消息 payload 几百字节）。u64 定长编码永远占 8 字节，
//! varint 小数字只占 1~2 字节。百万连接 × 高 QPS 下，
//! 省下的每个字节都直接转化为带宽与内存占用。
//!
//! 编码规则：每字节低 7 位存数据，最高位 `1` 表示"后续还有字节"（字节序为低组在前）。
//! 举例：`300 = 0b100101100` → 低 7 位 `0101100`（补标志位 `0xAC`），
//! 高 2 位 `10`（`0x02`）→ `[0xAC, 0x02]` 两字节。
//!
//! Protobuf、SQLite、LEB128（WASM）都是这套编码。
//!
//! # 为什么需要 zigzag
//!
//! 直接 varint 有符号数会把 `-1`（全 1 位模式）编码成 10 字节——
//! 最"贵"的编码给了最常见的小负数。zigzag 把有符号数映射到无符号数：
//! `0→0, -1→1, 1→2, -2→3, 2→4…`，让接近零的正负数都变短。
//! 【Java】`DataOutputStream.writeUTF` 不是 varint；Java 里通常用 Guava 或手写。

use bytes::BufMut;

use crate::error::ProtocolError;

/// u64 varint 的最大编码长度：⌈64 / 7⌉ = 10 字节。
pub const MAX_VARINT_LEN: usize = 10;

/// 把 `value` 以 varint 编码写入 `dst`，返回写入的字节数。
///
/// # Examples
///
/// ```
/// use im_protocol::varint;
///
/// let mut buf = Vec::new();
/// assert_eq!(varint::encode_u64(300, &mut buf), 2);
/// assert_eq!(buf, [0xAC, 0x02]);
///
/// let mut buf = Vec::new();
/// assert_eq!(varint::encode_u64(0, &mut buf), 1);
/// assert_eq!(buf, [0x00]);
/// ```
pub fn encode_u64(mut value: u64, dst: &mut impl BufMut) -> usize {
    let mut written = 0;
    loop {
        // 低 7 位必然落在 u8 内；`try_from` + `expect` 零运行时开销（编译器消除）
        let byte = u8::try_from(value & 0x7F).expect("7 位必在 u8 范围内");
        value >>= 7;
        if value == 0 {
            dst.put_u8(byte);
            return written + 1;
        }
        // 还有高位：置最高位为 1，表示"后续还有字节"
        dst.put_u8(byte | 0x80);
        written += 1;
    }
}

/// 预计算 `value` 的 varint 编码长度（用于一次性分配准确的缓冲区）。
#[must_use]
pub const fn encoded_len(value: u64) -> usize {
    // 有效位数 = 64 - 前导零；每 7 位一组；value = 0 也要 1 字节
    let bits = 64 - value.leading_zeros() as usize;
    if bits == 0 {
        1
    } else {
        (bits + 6) / 7
    }
}

/// zigzag 编码：有符号 → 无符号（`0→0, -1→1, 1→2, -2→3…`）。
// 位运算后符号位已无数值语义，重解释是本算法的核心，无符号丢失可言
#[allow(clippy::cast_sign_loss)]
#[must_use]
pub const fn zigzag_encode(v: i64) -> u64 {
    (((v << 1) ^ (v >> 63)) as u64)
}

/// zigzag 解码：无符号 → 有符号（[`zigzag_encode`] 的逆映射）。
#[allow(clippy::cast_sign_loss)]
#[must_use]
pub const fn zigzag_decode(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -i64::from(u & 1)
}

/// 把有符号数 zigzag + varint 一体编码写入 `dst`，返回写入的字节数。
///
/// # Examples
///
/// ```
/// use im_protocol::varint;
///
/// let mut buf = Vec::new();
/// // -1 zigzag 后是 1，varint 后是单字节
/// assert_eq!(varint::encode_i64(-1, &mut buf), 1);
/// assert_eq!(buf, [0x01]);
/// ```
pub fn encode_i64(value: i64, dst: &mut impl BufMut) -> usize {
    encode_u64(zigzag_encode(value), dst)
}

/// 一次性解码：从 `src` 头部读一个 varint。
///
/// 返回 `Some((值, 消耗字节数))`；`src` 太短（半包）或 varint 超长时返回 `None`。
#[must_use]
pub fn decode_u64(src: &[u8]) -> Option<(u64, usize)> {
    let mut dec = VarIntDecoder::default();
    for (i, &b) in src.iter().enumerate() {
        match dec.push(b) {
            Ok(Some(v)) => return Some((v, i + 1)),
            Ok(None) => {}
            Err(_) => return None,
        }
    }
    None
}

/// 增量 varint 解码器：一次喂一个字节，专供流式解码状态机使用。
///
/// 与 `decode_u64`（一次性、需要完整切片）相对，
/// 它把"解析到第几个字节"存在自身——这正是 [`crate::codec`] 增量解码的基石。
#[derive(Debug, Clone)]
pub struct VarIntDecoder {
    /// 已累积的值（低组在前，逐组左移进位）。
    value: u64,
    /// 下一组 7 位应左移的位数。
    shift: u32,
    /// 已消费的字节数（用于超长检测）。
    count: usize,
}

impl Default for VarIntDecoder {
    fn default() -> Self {
        Self { value: 0, shift: 0, count: 0 }
    }
}

impl VarIntDecoder {
    /// 喂入一个字节。
    ///
    /// 返回 `Ok(Some(v))` 表示 varint 结束、值为 `v`；
    /// `Ok(None)` 表示还需要更多字节。
    ///
    /// # Errors
    ///
    /// 喂满 [`MAX_VARINT_LEN`] 字节仍未结束时返回
    /// [`ProtocolError::VarintTooLong`]——正常流里不可能出现，
    /// 出现即说明字节流已错位。
    pub fn push(&mut self, byte: u8) -> Result<Option<u64>, ProtocolError> {
        self.count += 1;
        if self.count > MAX_VARINT_LEN {
            return Err(ProtocolError::VarintTooLong { max: MAX_VARINT_LEN });
        }
        self.value |= u64::from(byte & 0x7F) << self.shift;
        if byte & 0x80 == 0 {
            return Ok(Some(self.value));
        }
        self.shift += 7;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn known_values() {
        // 教科书用例（与 Protobuf 文档一致）
        let cases: &[(u64, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7F]),
            (128, &[0x80, 0x01]),
            (300, &[0xAC, 0x02]),
            (16_383, &[0xFF, 0x7F]),
            (u64::MAX, &[0xFF; 10]),
        ];
        for &(value, expect) in cases {
            let mut buf = Vec::new();
            assert_eq!(encode_u64(value, &mut buf), expect.len(), "value={value}");
            assert_eq!(&buf, expect, "value={value}");
            assert_eq!(encoded_len(value), expect.len());

            let (got, used) = decode_u64(&buf).expect("完整编码必然可解");
            assert_eq!(got, value);
            assert_eq!(used, expect.len());
        }
    }

    #[test]
    fn zigzag_known_values() {
        assert_eq!(zigzag_encode(0), 0);
        assert_eq!(zigzag_encode(-1), 1);
        assert_eq!(zigzag_encode(1), 2);
        assert_eq!(zigzag_encode(-2), 3);
        assert_eq!(zigzag_encode(i64::MAX), u64::MAX - 1);
        assert_eq!(zigzag_encode(i64::MIN), u64::MAX);
        assert_eq!(zigzag_decode(u64::MAX), i64::MIN);
    }

    #[test]
    fn truncated_varint_returns_none() {
        // 半包：只有一个标志位为 1 的字节，值未结束
        assert_eq!(decode_u64(&[0x80]), None);
        assert_eq!(decode_u64(&[]), None);
    }

    #[test]
    fn varint_too_long_is_rejected() {
        let mut dec = VarIntDecoder::default();
        for _ in 0..MAX_VARINT_LEN {
            assert!(dec.push(0xFF).is_ok(), "10 个字节内不应报错");
        }
        // 第 11 个字节：超长
        assert!(matches!(
            dec.push(0xFF),
            Err(ProtocolError::VarintTooLong { .. })
        ));
    }

    proptest! {
        #[test]
        fn varint_roundtrip(v in any::<u64>()) {
            let mut buf = Vec::new();
            let n = encode_u64(v, &mut buf);
            prop_assert_eq!(n, buf.len());
            prop_assert_eq!(n, encoded_len(v));
            let (got, used) = decode_u64(&buf).expect("编码后必可解码");
            prop_assert_eq!(got, v);
            prop_assert_eq!(used, buf.len());
        }

        #[test]
        fn zigzag_roundtrip(v in any::<i64>()) {
            prop_assert_eq!(zigzag_decode(zigzag_encode(v)), v);
        }
    }
}
