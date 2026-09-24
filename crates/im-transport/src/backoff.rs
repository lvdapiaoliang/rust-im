//! 指数退避重连策略（算法图谱 #5）。
//!
//! # 解决什么问题
//!
//! 服务端重启瞬间，所有客户端同时重连会造成「重连风暴」——
//! 惊群效应直接把刚活过来的服务端再次压垮。两个经典对策：
//!
//! 1. **指数退避**：重试间隔 1s → 2s → 4s → 8s…（每次 ×2，封顶），
//!    让「执念」随失败次数指数衰减；
//! 2. **抖动 jitter**：在退避值上叠加随机扰动，打散数千客户端的
//!    重连时刻——AWS 架构博客《Exponential Backoff and Jitter》的
//!    核心结论：**加了 jitter 的退避才是完整方案**。
//!
//! # 状态机视角
//!
//! ```text
//!   成功 ──reset()──▶ attempt=0（下一次失败从 base 起步）
//!   失败 ──next_delay()──▶ attempt+1（间隔 ×2 封顶，等待后重试）
//! ```
//!
//! 对照 Java 的 resilience4j `IntervalBiFunction`：这里是值语义的
//! 10 行结构体，不需要框架。
//!
//! # 模式落点
//!
//! 算法：指数增长 + full jitter（`uniform(0, min(cap, base·2^n))`）；
//! 状态机：`attempt` 单调递增直到 `reset`——与 [`crate::dedup`] 一样，
//! 「进度」本身编码在数据里。

use std::time::Duration;

/// 指数退避计算器：值语义，无锁，每连接一个。
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use im_transport::Backoff;
///
/// let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
/// // 失败四次：1s → 2s → 4s → 8s（含 jitter，实际值 ≤ 档位值）
/// for _ in 0..4 {
///     let d = b.next_delay();
///     assert!(d <= b.ceiling());
/// }
/// assert_eq!(b.attempts(), 4);
///
/// // 成功后归零：下一次失败又从 1s 档起步
/// b.reset();
/// assert_eq!(b.attempts(), 0);
/// ```
#[derive(Debug, Clone)]
pub struct Backoff {
    /// 第一次退避的基准间隔。
    base: Duration,
    /// 退避上限（封顶值）。
    max: Duration,
    /// 连续失败次数（当前档位）。
    attempt: u32,
    /// xorshift 状态（jitter 随机源；从系统熵播种）。
    rng: u64,
}

impl Backoff {
    /// 创建：`base` 为首次退避间隔，`max` 为封顶。
    ///
    /// # Panics
    ///
    /// `base` 为零时 panic（零间隔退避等于无退避的重连风暴）。
    #[must_use]
    pub fn new(base: Duration, max: Duration) -> Self {
        assert!(!base.is_zero(), "base 必须为正：零间隔退避 = 重连风暴");
        Self {
            base,
            max,
            attempt: 0,
            rng: seed_from_system(),
        }
    }

    /// 记录一次失败并返回本次应等待的时长（含 jitter）。
    ///
    /// full jitter：在 `[0, 档位值]` 上均匀采样——
    /// 档位值单调指数增长，但实际等待值随机，多个客户端自然打散。
    #[must_use]
    pub fn next_delay(&mut self) -> Duration {
        let cap = self.ceiling();
        self.attempt = self.attempt.saturating_add(1);
        if cap.is_zero() {
            return Duration::ZERO;
        }
        // 均匀采样 [0, cap]：随机数 × cap / u64::MAX
        let r = self.next_random();
        let millis = cap.as_millis() * u128::from(r) / u128::from(u64::MAX);
        let millis = u64::try_from(millis).unwrap_or(u64::MAX);
        Duration::from_millis(millis)
    }

    /// 成功后归零：下一次失败从 `base` 档重新起步。
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// 连续失败次数。
    #[must_use]
    pub fn attempts(&self) -> u32 {
        self.attempt
    }

    /// 当前档位值（无 jitter 的名义间隔）：`min(base · 2^attempt, max)`。
    ///
    /// 饱和算术：`2^attempt` 再大也不会溢出 panic。
    #[must_use]
    pub fn ceiling(&self) -> Duration {
        // Duration * 2^n：用 checked/saturating 防溢出
        let shift = self.attempt.min(63);
        let factor = 1u64 << shift;
        let base_nanos = self.base.as_nanos().saturating_mul(u128::from(factor));
        let capped = Duration::from_nanos(base_nanos.min(u64::MAX as u128) as u64);
        capped.min(self.max)
    }

    /// xorshift64*：一条乘法 + 三次异或移位，足够打散抖动。
    fn next_random(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// 从系统时钟播种（纳秒精度足够 IM 场景的 jitter；不是密码学随机）。
fn seed_from_system() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    nanos | 1 // xorshift 状态不能为 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 档位指数增长且封顶（ceiling 是纯函数，精确可断言）
    #[test]
    fn tiers_double_until_capped() {
        let b = Backoff::new(Duration::from_secs(1), Duration::from_secs(8));
        assert_eq!(b.ceiling(), Duration::from_secs(1));
        let mut b = b;
        b.attempt = 1;
        assert_eq!(b.ceiling(), Duration::from_secs(2));
        b.attempt = 2;
        assert_eq!(b.ceiling(), Duration::from_secs(4));
        b.attempt = 3;
        assert_eq!(b.ceiling(), Duration::from_secs(8));
        b.attempt = 4;
        assert_eq!(b.ceiling(), Duration::from_secs(8), "封顶后不再增长");
        b.attempt = 100;
        assert_eq!(b.ceiling(), Duration::from_secs(8), "超大幂次饱和而非溢出");
    }

    /// jitter 永远不超过当次档位值
    #[test]
    fn jitter_stays_within_tier() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_secs(1));
        for _ in 0..1000 {
            let tier = b.ceiling(); // 调用前记录档位
            let d = b.next_delay();
            assert!(d <= tier, "超出档位: {d:?} > {tier:?}");
        }
    }

    /// 多次采样覆盖区间（jitter 不是常数——随机源真的在动）
    #[test]
    fn jitter_actually_varies() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(1));
        let mut distinct = std::collections::HashSet::new();
        for _ in 0..100 {
            distinct.insert(b.next_delay().as_millis());
        }
        // 100 次采样至少 10 个不同值：如果随机源坏了会高度集中
        assert!(distinct.len() >= 10, "jitter 分布过于集中: {distinct:?}");
    }

    /// attempts 计数与 reset 语义
    #[test]
    fn attempts_and_reset() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(60));
        assert_eq!(b.attempts(), 0);
        b.next_delay();
        b.next_delay();
        b.next_delay();
        assert_eq!(b.attempts(), 3);
        b.reset();
        assert_eq!(b.attempts(), 0);
        // 归零后档位回到 base
        assert_eq!(b.ceiling(), Duration::from_secs(1));
    }

    /// 两个独立实例（不同种子）产生不同序列——客户端间自然错峰
    #[test]
    fn independent_instances_diverge() {
        let mut a = Backoff::new(Duration::from_secs(1), Duration::from_secs(1));
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(1));
        let seq_a: Vec<_> = (0..5).map(|_| a.next_delay()).collect();
        let seq_b: Vec<_> = (0..5).map(|_| b.next_delay()).collect();
        assert_ne!(seq_a, seq_b, "相同种子会让 jitter 形同虚设");
    }

    /// base 为零被拒绝（防呆：零退避 = 风暴）
    #[test]
    #[should_panic(expected = "base 必须为正")]
    fn zero_base_is_rejected() {
        Backoff::new(Duration::ZERO, Duration::from_secs(1));
    }
}
