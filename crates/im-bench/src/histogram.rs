//! 延迟直方图（分位数草图）：HdrHistogram 思想的从零实现。
//!
//! # 为什么压测需要它（而不是排序数组）
//!
//! 阶段 7 的 `group-fanout` 用「采样 → 排序 → 最近邻分位数」——30 个样本
//! 这么做毫无问题；阶段 10 的测量对象是**百万级样本**（连接风暴的逐连接
//! 里程碑、弱网链路的逐消息延迟）：
//!
//! - 排序数组：内存 O(n)、每次查询 O(n log n)——样本一多，测量工具自己
//!   变成了被测系统的竞争者（观察者效应）；
//! - 直方图草图：内存 O(1)（本实现约 114KB）、记录 O(1)、任意分位数
//!   查询 O(桶数)——**用精度换资源**，这正是"草图"类数据结构的本质。
//!
//! # 结构：对数分桶 + 桶内线性细分（HdrHistogram 的骨架）
//!
//! ```text
//! 桶 0            : [0, 256)      步长 1     —— 小延迟精确到纳秒
//! 桶 1            : [256, 512)    步长 1
//! 桶 2            : [512, 1024)   步长 2
//! 桶 b (b ≥ 1)    : [256·2^(b-1), 256·2^b)  步长 2^(b-1)
//! ...
//! 桶 56           : 覆盖到 u64 顶端
//! ```
//!
//! 每桶固定 256 个槽（`SIGNIFICANT_BITS = 8`）：**任何值的记录误差
//! 相对值 ≤ 2^-8 ≈ 0.39%**——这就是"有效位"参数（HdrHistogram 的
//! 配置项 significant value bits 即此）。查询返回所在槽的**下界**
//! （保守口径：真实分位数 ≥ 报告值）。
//!
//! # 与三方库的对照
//!
//! `HdrHistogram`（C/Java 原版）：同构，多出的能力（原子并发记录、
//! correcting 版本）本场景不需要——单写入者是测量代码的纪律
//! （与 docs/20 §5.3 的单一写入者约定同源）。

use std::time::Duration;

/// 每桶槽数 = 2^SIGNIFICANT_BITS：决定相对精度（1/256 ≈ 0.39%）。
const SIGNIFICANT_BITS: u32 = 8;
/// 每桶槽数。
const SLOTS: u64 = 1 << SIGNIFICANT_BITS; // 256
/// 最大桶号：u64 顶端位段（floor(log2(u64::MAX)) - 8 + 1 = 56）。
const MAX_BUCKET: usize = 64 - SIGNIFICANT_BITS as usize; // 56

/// 延迟直方图：固定 (MAX_BUCKET+1)×256 个计数槽，无堆增长、无再分配。
///
/// 记录口径：纳秒整数（u64 足装 ~584 年；测量代码不掺浮点）。
#[derive(Debug)]
pub struct LatencyHist {
    /// 计数矩阵展平：`counts[bucket * SLOTS + slot]`。
    counts: Vec<u64>,
    /// 总样本数（与所有槽之和守恒——单测校验）。
    total: u64,
    /// 观测最小值（精确记录，不经过桶）。
    min: u64,
    /// 观测最大值（精确记录，不经过桶）。
    max: u64,
    /// 累计和（算平均用；u128 防大值累加溢出）。
    sum_ns: u128,
}

impl LatencyHist {
    /// 空直方图。
    #[must_use]
    pub fn new() -> Self {
        Self {
            counts: vec![0; (MAX_BUCKET + 1) * SLOTS as usize],
            total: 0,
            min: u64::MAX,
            max: 0,
            sum_ns: 0,
        }
    }

    /// 记录一个样本（纳秒）。
    pub fn record_ns(&mut self, ns: u64) {
        let (bucket, slot) = locate(ns);
        self.counts[bucket * SLOTS as usize + slot] += 1;
        self.total += 1;
        self.min = self.min.min(ns);
        self.max = self.max.max(ns);
        self.sum_ns += u128::from(ns);
    }

    /// 记录一个样本（`Duration` 包装，调用侧免写 `as_nanos`）。
    pub fn record(&mut self, d: Duration) {
        // u128 → u64：纳秒超过 u64::MAX（584 年）对延迟测量不可能；
        // try_from 收口是项目 cast 纪律（docs/20 §7.2 第 1 条）
        let ns = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.record_ns(ns);
    }

    /// 最近邻分位数（返回纳秒）：升序第 `pct`% 处样本所在槽的**下界**。
    ///
    /// 与阶段 7 `percentile`（排序数组版）同一排名口径：
    /// `rank = ceil(pct% × n)`，夹到 `1..=n`；空表返回 0。
    #[must_use]
    pub fn percentile_ns(&self, pct: u64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let n = u128::from(self.total);
        let rank = (n * u128::from(pct)).div_ceil(100).min(n);
        let mut seen: u128 = 0;
        for (bucket, chunk) in self.counts.chunks(SLOTS as usize).enumerate() {
            for (slot, &count) in chunk.iter().enumerate() {
                seen += u128::from(count);
                if seen >= rank {
                    return bucket_value(bucket, slot);
                }
            }
        }
        unreachable!("total > 0 时排名必然落在某个槽内")
    }

    /// [`Self::percentile_ns`] 的 `Duration` 包装（报告输出用）。
    #[must_use]
    pub fn percentile(&self, pct: u64) -> Duration {
        Duration::from_nanos(self.percentile_ns(pct))
    }

    /// 样本总数。
    #[must_use]
    pub fn count(&self) -> u64 {
        self.total
    }

    /// 观测最小值（空表返回 `None`）。
    #[must_use]
    pub fn min(&self) -> Option<Duration> {
        (self.total > 0).then(|| Duration::from_nanos(self.min))
    }

    /// 观测最大值（空表返回 `None`）。
    #[must_use]
    pub fn max(&self) -> Option<Duration> {
        (self.total > 0).then(|| Duration::from_nanos(self.max))
    }

    /// 算术平均（整数纳秒截断；空表返回 `None`）。
    #[must_use]
    pub fn mean(&self) -> Option<Duration> {
        if self.total == 0 {
            return None;
        }
        let mean_ns = self.sum_ns / u128::from(self.total);
        Some(Duration::from_nanos(
            u64::try_from(mean_ns).expect("平均数不会超过样本最大值，u64 必装得下"),
        ))
    }
}

impl Default for LatencyHist {
    fn default() -> Self {
        Self::new()
    }
}

/// 值 → (桶, 槽)。
///
/// - `v < 256`：桶 0，槽 = 值本身（步长 1，精确）；
/// - 否则：`bucket = floor(log2 v) - 7`，`slot = (v >> (bucket-1)) - 256`。
#[inline]
fn locate(v: u64) -> (usize, usize) {
    if v < SLOTS {
        let slot = usize::try_from(v).expect("v < 256，usize 必装得下");
        return (0, slot);
    }
    let mag = 63 - v.leading_zeros(); // floor(log2 v)；v ≥ 256 ⇒ mag ≥ 8
    let bucket = usize::try_from(mag - SIGNIFICANT_BITS + 1).expect("桶号装得下 usize");
    let shifted = v >> (bucket - 1);
    let slot = usize::try_from(shifted - SLOTS).expect("同桶内偏移 < 256，usize 必装得下");
    (bucket, slot)
}

/// (桶, 槽) → 槽下界（`locate` 的逆映射；分位数报告此值）。
#[inline]
fn bucket_value(bucket: usize, slot: usize) -> u64 {
    if bucket == 0 {
        return slot as u64; // usize→u64：同宽/上转型，无损
    }
    (slot as u64 + SLOTS) << (bucket - 1)
}

/// 测试用伪随机源（确定性 LCG）：单测的基准真值需要可复现样本。
/// 弱网链路的正式随机源是 weaklink 模块的 xorshift64*——两者刻意分开：
/// 测试 rng 只求"稳定"，正式 rng 要"分布均匀且便宜"。
pub(crate) struct TestRng(u64);

impl TestRng {
    /// 以种子构造（先扰动一步，避免"种子即首个输出"的巧合样本）。
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }

    /// 下一个 64 位值（Numerical Recipes LCG + 高位异或混合）。
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 ^ (self.0 >> 33)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 小值（桶 0）精确无损：记录值即分位数报告值。
    #[test]
    fn small_values_are_exact() {
        let mut h = LatencyHist::new();
        for v in [0u64, 1, 42, 100, 255] {
            h.record_ns(v);
        }
        assert_eq!(h.count(), 5);
        assert_eq!(h.percentile_ns(20), 1); // rank = ceil(0.2×5) = 1 → 最小
        assert_eq!(h.percentile_ns(50), 42);
        assert_eq!(h.percentile_ns(100), 255);
        assert_eq!(h.min().expect("有样本").as_nanos(), 0);
        assert_eq!(h.max().expect("有样本").as_nanos(), 255);
    }

    /// 定位与取值互为逆映射：locate 后 bucket_value 满足"槽下界 ≤ 原值 < 槽上界"。
    #[test]
    fn locate_and_value_roundtrip() {
        for v in [256u64, 257, 300, 511, 512, 513, 1023, 1024, 1 << 20, 1 << 40, u64::MAX / 2, u64::MAX] {
            let (b, s) = locate(v);
            let lb = bucket_value(b, s);
            assert!(lb <= v, "槽下界 {lb} 应 ≤ 值 {v}");
            let step = if b == 0 { 1 } else { 1u64 << (b - 1) };
            if lb <= u64::MAX - step {
                assert!(v < lb + step, "值 {v} 应 < 槽上界 {}", lb + step);
            }
        }
    }

    /// 相对精度承诺：任意值所在槽下界与值之差 ≤ 值/256 + 1。
    #[test]
    fn precision_bound_holds() {
        for v in [256u64, 1_000, 100_000, 10_000_000, 1_000_000_000, 1 << 40] {
            let (b, s) = locate(v);
            let lb = bucket_value(b, s);
            assert!(
                v - lb <= v / SLOTS + 1,
                "槽 {b}/{s} 对 {v} 的精度超标（下界 {lb}）"
            );
        }
    }

    /// 与排序数组基准真值对拍：伪随机 10 万样本，每个分位数的偏差
    /// 不超过一个最大步长（草图精度的实证）。
    #[test]
    fn matches_sorted_ground_truth_within_one_step() {
        let mut rng = TestRng::new(42);
        let mut h = LatencyHist::new();
        let mut raw: Vec<u64> = Vec::with_capacity(100_000);
        for _ in 0..100_000 {
            let v = rng.next_u64() % (1 << 32); // 4 秒内的延迟域
            h.record_ns(v);
            raw.push(v);
        }
        raw.sort_unstable();

        for pct in [1u64, 50, 90, 99, 100] {
            let rank = (raw.len() as u128 * u128::from(pct)).div_ceil(100);
            let rank = usize::try_from(rank).expect("样本数装得下 usize") - 1;
            let truth = raw[rank];
            let sketch = h.percentile_ns(pct);
            // 草图报下界：sketch ≤ truth < sketch + 最大步长（4 秒域内最大步长 = 2^24）
            assert!(
                sketch <= truth && truth - sketch < (1 << 24),
                "P{pct}: 草图 {sketch} 与真值 {truth} 偏差超过一个槽"
            );
        }
    }

    /// 守恒律：槽计数之和 == total（直方图自身的对账，
    /// 与 docs/20 §4.3 扇出计数对账同款纪律）。
    #[test]
    fn slot_counts_sum_to_total() {
        let mut rng = TestRng::new(7);
        let mut h = LatencyHist::new();
        for _ in 0..1_000 {
            h.record_ns(rng.next_u64() >> 8); // 全值域随机
        }
        let sum: u64 = h.counts.iter().sum();
        assert_eq!(sum, h.total, "槽计数之和必须等于 total");
    }

    /// 分位数单调：P50 ≤ P90 ≤ P99 ≤ max（读侧自洽性）。
    #[test]
    fn percentiles_are_monotonic() {
        let mut rng = TestRng::new(99);
        let mut h = LatencyHist::new();
        for _ in 0..1_000 {
            h.record_ns(rng.next_u64() % 1_000_000);
        }
        let p50 = h.percentile_ns(50);
        let p90 = h.percentile_ns(90);
        let p99 = h.percentile_ns(99);
        let mx = h.max().expect("有样本").as_nanos();
        assert!(p50 <= p90 && p90 <= p99 && p99 <= mx);
    }

    /// 平均值：已知样本的算术平均精确可验；空表返回 None。
    #[test]
    fn mean_matches_manual_computation() {
        let mut h = LatencyHist::new();
        assert!(h.mean().is_none());
        for v in [100u64, 200, 300] {
            h.record_ns(v);
        }
        assert_eq!(h.mean().expect("有样本").as_nanos(), 200);
    }

    /// Duration 包装与纳秒口径一致。
    #[test]
    fn duration_wrapper_matches_ns() {
        let mut h = LatencyHist::new();
        h.record(Duration::from_millis(3));
        assert_eq!(h.percentile_ns(100), 3_000_000);
    }
}
