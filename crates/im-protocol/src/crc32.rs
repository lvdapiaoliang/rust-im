//! CRC-32（IEEE 802.3，反射多项式 `0xEDB88320`）——算法图谱第二项落地。
//!
//! # 为什么选 CRC32 而不是别的
//!
//! - **vs 简单校验和（求和/异或）**：校验和发现不了字节重排与低位连续错；
//!   CRC 是循环码，能检出所有 ≤ 32 位的突发错误（burst error）——
//!   恰好是链路层误码的主要形态。
//! - **vs MD5/SHA**：我们防的是**误码**不是**篡改**（防篡改在阶段 7 的 E2EE 层做）；
//!   查表法 CRC 每字节约 1ns，比密码学哈希快一个数量级。
//!
//! # 实现要点
//!
//! 查表法（每字节一次查表 + 两次异或），256 项表由 `const fn` 在
//! **编译期**生成——运行时零初始化开销。
//! 注意与 `std::hash` 无关：CRC 是**校验码**（特定检错语义），
//! 不是通用哈希（均匀分布语义），工程上不可混用。

/// 反射多项式：标准 CRC-32（与 zlib/PNG/zip 完全兼容，可直接对拍）。
const POLY: u32 = 0xEDB8_8320;

/// 编译期生成的 256 项查表。
static TABLE: [u32; 256] = make_table();

/// 逐项推导查表：`table[i]` 是字节 `i` 经过 8 轮多项式除法的余数。
const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i: u32 = 0;
    while i < 256 {
        let mut c = i;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i as usize] = c; // u32 → usize 是拓宽转换，无损
        i += 1;
    }
    table
}

/// 增量 CRC-32 计算器：适合流式解码（字节分块到达时逐块累积）。
///
/// 这正是 [`crate::codec`] 解码状态机的用法：每消费一段字节就 `update` 一次，
/// 帧尾到达时 `finish` 得到整帧的 CRC——不需要回头重扫整帧。
#[derive(Debug, Clone)]
pub struct Crc32 {
    /// CRC 初值是全 1（`0xFFFF_FFFF`），不是 0——这是 IEEE 定义的一部分。
    state: u32,
}

impl Default for Crc32 {
    fn default() -> Self {
        Self { state: 0xFFFF_FFFF }
    }
}

impl Crc32 {
    /// 新建计算器（初值 `0xFFFF_FFFF`）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节，累积进 CRC 状态。
    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            // 查表法核心：当前状态与输入字节的低 8 位决定表项
            let idx = ((self.state ^ u32::from(b)) & 0xFF) as usize;
            self.state = (self.state >> 8) ^ TABLE[idx];
        }
    }

    /// 结束计算，输出最终 CRC-32 值。
    ///
    /// 按**值消耗** `self`：计算器用完即弃，防止误把旧状态带进下一帧。
    #[must_use]
    pub fn finish(self) -> u32 {
        self.state ^ 0xFFFF_FFFF
    }
}

/// 一次性计算整段字节的 CRC-32（IEEE）。
///
/// # Examples
///
/// ```
/// use im_protocol::crc32;
///
/// // CRC-32/IEEE 的标准校验向量：任何实现对 "123456789" 必得 0xCBF43926
/// assert_eq!(crc32::checksum(b"123456789"), 0xCBF4_3926);
/// assert_eq!(crc32::checksum(b""), 0);
/// ```
#[must_use]
pub fn checksum(bytes: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(bytes);
    c.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn ieee_check_value() {
        // 行业标准校验向量：与 zlib 的 crc32()、PNG chunk 校验完全一致
        assert_eq!(checksum(b"123456789"), 0xCBF4_3926);
        assert_eq!(checksum(b""), 0x0000_0000);
        assert_eq!(checksum(b"a"), 0xE8B7_BE43);
        assert_eq!(checksum(b"abc"), 0x3524_416C);
    }

    #[test]
    fn incremental_equals_oneshot() {
        // 流式分块计算 == 整段一次计算（解码器正确性的前提）
        let data = b"the quick brown fox jumps over the lazy dog";
        let mut c = Crc32::new();
        for chunk in data.chunks(7) {
            c.update(chunk);
        }
        assert_eq!(c.finish(), checksum(data));
    }

    #[test]
    fn detects_single_bit_flip_and_reorder() {
        let data = b"hello world";
        let good = checksum(data);

        // 单比特翻转
        let mut corrupted = data.to_vec();
        corrupted[3] ^= 0x01;
        assert_ne!(checksum(&corrupted), good);

        // 字节重排（求和校验和检测不到的形态）
        let mut reordered = data.to_vec();
        reordered.swap(0, 10);
        assert_ne!(checksum(&reordered), good);
    }

    proptest! {
        #[test]
        fn crc_changes_with_input(bytes in any::<Vec<u8>>()) {
            // 输入任何一个字节变化，CRC 大概率变化（32 位输出，碰撞率 2^-32）
            if let Some(last) = bytes.last() {
                let mut other = bytes.clone();
                let last = *last;
                other.last_mut().map(|l| *l = last.wrapping_add(1));
                // 长度相同、仅末字节不同 → CRC 不同（此处不构造碰撞）
                prop_assert_ne!(checksum(&bytes), checksum(&other));
            }
        }
    }
}
