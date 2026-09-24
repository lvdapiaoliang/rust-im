//! 雪花 ID 生成器（算法图谱 #6）：位段分配 + 时钟回拨处理。
//!
//! # 位段布局（64 bit）
//!
//! ```text
//! ┌─┬───────────────────┬─────────────┬───────────────┐
//! │0│   41 bit 毫秒时间戳 │ 10 bit 机器 │  12 bit 序列   │
//! └─┴───────────────────┴─────────────┴───────────────┘
//!  │  ≈ 69 年（自定义纪元起）│  1024 台机器 │ 每毫秒 4096 个 │
//! ```
//!
//! - **时间戳在前**：ID 天然按时间有序（B+ 树索引友好、按 ID 排序 = 按时间排序）；
//! - **机器段居中**：多实例部署免协调，同一毫秒内不同机器的 ID 互不冲突；
//! - **序列收尾**：单机单毫秒 4096 个，超出即报错（见 [`SnowflakeError::SequenceExhausted`]）。
//!
//! # 时钟回拨（本模块的教学重点）
//!
//! NTP 校时可能让系统时钟**往回跳**。雪花算法的 ID 有序性建立在
//! 「时间戳只前进」上——回拨瞬间若继续发号，会产生比已发 ID 更小的 ID，
//! 破坏有序性甚至造成重复。策略：
//!
//! 1. 回拨量 ≤ [`MAX_BACKWARD_MS`]（容忍 NTP 微调）：错误值携带回拨量，
//!    调用方可稍候重试（等待真实时间追平）；
//! 2. 回拨量更大：同样报错但需运维介入（换 machine_id 或等时钟稳定）。
//!    阈值本身由调用方把握——生成器只负责「拒绝 + 报告回拨量」。
//!
//! # 测试策略：注入时钟
//!
//! `Clock` trait 把「现在几毫秒」变成可注入依赖——单元测试用
//! [`ManualClock`] 手动拨针，毫秒不快进也能测序列耗尽/回拨/恢复，
//! 不需要真的 `sleep`。「控制时间」是测时间敏感代码的唯一体面方式。
//!
//! # 模式落点
//!
//! - 算法：位段组装（`<<` / `|`）、位段解析（`>>` / `&` 掩码）；
//! - 依赖注入：trait Clock + 系统实现，测试替身（test double）。

use std::fmt;

/// 自定义纪元：2024-01-01T00:00:00Z（毫秒）。
///
/// 不用 Unix 纪元：41 位毫秒从 1970 年起只能用到 2039 年；
/// 自定义纪元把寿命推到 ~2092 年。
pub const EPOCH_MS: u64 = 1_700_000_000_000;

/// 时间戳位宽。
pub const TIMESTAMP_BITS: u32 = 41;
/// 机器 ID 位宽。
pub const MACHINE_BITS: u32 = 10;
/// 序列号位宽。
pub const SEQUENCE_BITS: u32 = 12;

/// 机器 ID 上限（含）。
pub const MAX_MACHINE_ID: u64 = (1 << MACHINE_BITS) - 1;
/// 单毫秒序列上限（含）。
pub const MAX_SEQUENCE: u64 = (1 << SEQUENCE_BITS) - 1;
/// 可容忍的时钟回拨上限（毫秒）。
pub const MAX_BACKWARD_MS: u64 = 5;

/// 雪花 ID 生成错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnowflakeError {
    /// 时钟回拨超过容忍阈值（`backwards` 毫秒）。
    ///
    /// 有序性无法维持，需要人工介入或等待时钟稳定。
    ClockMovedBackwards {
        /// 回拨的毫秒数。
        backwards: u64,
    },
    /// 当前毫秒的 4096 个序列号已耗尽。
    ///
    /// 正常业务极难触发；调用方 sleep 到下一毫秒再试。
    SequenceExhausted,
}

impl fmt::Display for SnowflakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClockMovedBackwards { backwards } => {
                write!(f, "clock moved backwards by {backwards}ms, refusing to issue ids")
            }
            Self::SequenceExhausted => {
                write!(f, "sequence exhausted for current millisecond")
            }
        }
    }
}

impl std::error::Error for SnowflakeError {}

/// 时钟抽象：当前毫秒（相对 [`EPOCH_MS`]）。
///
/// 生产用 [`SystemClock`]，测试用 [`ManualClock`] 手动拨针。
pub trait Clock: Send {
    /// 返回当前相对纪元的毫秒数。
    fn now_ms(&self) -> u64;
}

/// 系统时钟（生产用）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        unix_ms.saturating_sub(EPOCH_MS)
    }
}

/// 手动时钟（测试用）：`Cell` 单线程拨针，测试跑在单线程里足够。
#[derive(Debug)]
pub struct ManualClock {
    now: std::cell::Cell<u64>,
}

impl ManualClock {
    /// 创建停在 `ms` 的时钟。
    #[must_use]
    pub fn new(ms: u64) -> Self {
        Self {
            now: std::cell::Cell::new(ms),
        }
    }

    /// 拨到 `ms`（可以往回拨——正好用来测回拨分支）。
    pub fn set(&self, ms: u64) {
        self.now.set(ms);
    }

    /// 前进 `ms` 毫秒。
    pub fn advance(&self, ms: u64) {
        self.now.set(self.now.get() + ms);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }
}

/// 雪花 ID 生成器：每实例独占线程/任务（`&mut self` 即同步语义）。
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use im_server::snowflake::{ManualClock, Snowflake};
///
/// let clock = Arc::new(ManualClock::new(1000));
/// let mut sf = Snowflake::new(7, clock);
///
/// let a = sf.next_id().unwrap();
/// let b = sf.next_id().unwrap();
/// assert!(b > a, "同一毫秒内序列递增，ID 严格有序");
/// ```
pub struct Snowflake<C: Clock> {
    /// 机器 ID（已校验 ≤ 1023）。
    machine_id: u64,
    /// 上次发号的毫秒（相对纪元）；`None` = 尚未发过号。
    ///
    /// 用 `Option` 而非 `0` 哨兵：时钟可能恰好停在 0（测试、开机瞬间），
    /// 哨兵值与真实时间重合会误判「同一毫秒」。
    last_ms: Option<u64>,
    /// 当前毫秒内已用的序列号。
    sequence: u64,
    /// 时间源（可注入）。
    clock: std::sync::Arc<C>,
}

impl<C: Clock> Snowflake<C> {
    /// 创建生成器。
    ///
    /// # Panics
    ///
    /// `machine_id > 1023` 时 panic：这是部署期配置错误（位段装不下），
    /// 早死早超生，不值得运行期错误通道。
    pub fn new(machine_id: u64, clock: std::sync::Arc<C>) -> Self {
        assert!(
            machine_id <= MAX_MACHINE_ID,
            "machine_id {machine_id} 超出 {MACHINE_BITS} 位上限 {MAX_MACHINE_ID}"
        );
        Self {
            machine_id,
            last_ms: None,
            sequence: 0,
            clock,
        }
    }

    /// 生成下一个 ID。
    ///
    /// # Errors
    ///
    /// - [`SnowflakeError::ClockMovedBackwards`]：时钟回拨超阈值；
    /// - [`SnowflakeError::SequenceExhausted`]：本毫秒 4096 个序列号用尽。
    pub fn next_id(&mut self) -> Result<u64, SnowflakeError> {
        let now = self.clock.now_ms();

        match self.last_ms {
            // ── 时钟回拨检查 ──
            Some(last) if now < last => {
                let backwards = last - now;
                // 无论大小都拒绝发号：错误值携带回拨量，调用方/运维
                // 依此分辨「稍候重试」还是「人工介入」。
                return Err(SnowflakeError::ClockMovedBackwards { backwards });
            }
            // 同一毫秒：序列 +1；耗尽则报错
            Some(last) if now == last => {
                if self.sequence == MAX_SEQUENCE {
                    return Err(SnowflakeError::SequenceExhausted);
                }
                self.sequence += 1;
            }
            // 新毫秒（或首个 ID）：序列归零（从 0 开始，避免全 0 ID
            // 与「未发号」混淆）
            _ => {
                self.last_ms = Some(now);
                self.sequence = 0;
            }
        }

        Ok(assemble(now, self.machine_id, self.sequence))
    }
}

/// 位段组装：`timestamp << 22 | machine << 12 | sequence`。
///
/// `const fn`：可以在编译期算出测试期望值（编译期对拍）。
#[must_use]
pub const fn assemble(timestamp: u64, machine_id: u64, sequence: u64) -> u64 {
    (timestamp << (MACHINE_BITS + SEQUENCE_BITS))
        | (machine_id << SEQUENCE_BITS)
        | sequence
}

/// 位段解析：ID → (timestamp, machine_id, sequence)。
///
/// 与 [`assemble`] 互逆；运维排查「这个 ID 哪台机器何时发的」全靠它。
#[must_use]
pub const fn decode(id: u64) -> (u64, u64, u64) {
    let timestamp = id >> (MACHINE_BITS + SEQUENCE_BITS);
    let machine_id = (id >> SEQUENCE_BITS) & MAX_MACHINE_ID;
    let sequence = id & MAX_SEQUENCE;
    (timestamp, machine_id, sequence)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generator(ms: u64, machine_id: u64) -> (std::sync::Arc<ManualClock>, Snowflake<ManualClock>) {
        let clock = std::sync::Arc::new(ManualClock::new(ms));
        let sf = Snowflake::new(machine_id, clock.clone());
        (clock, sf)
    }

    /// 单调性：同毫秒序列递增；跨毫秒时间戳递增
    #[test]
    fn ids_are_strictly_monotonic() {
        let (clock, mut sf) = generator(1000, 1);

        let mut prev = 0;
        for i in 0..100 {
            if i == 50 {
                clock.advance(1); // 跨毫秒
            }
            let id = sf.next_id().unwrap();
            assert!(id > prev, "ID 必须严格递增: {id} <= {prev}");
            prev = id;
        }
    }

    /// 位段布局：decode(assemble(x)) == x（编译期与运行期双重验证）
    #[test]
    fn bit_layout_roundtrip() {
        const TS: u64 = 123_456;
        const MACHINE: u64 = 1023;
        const SEQ: u64 = 4095;
        const ID: u64 = assemble(TS, MACHINE, SEQ);
        const DECODED: (u64, u64, u64) = decode(ID);
        assert_eq!(DECODED, (TS, MACHINE, SEQ));

        assert_eq!(decode(assemble(0, 0, 0)), (0, 0, 0));
        assert_eq!(decode(assemble(1, 1, 1)), (1, 1, 1));
    }

    /// 首个 ID 的序列号是 0（新毫秒归零）
    #[test]
    fn first_id_of_new_ms_starts_at_zero() {
        let (_, mut sf) = generator(42, 3);
        let id = sf.next_id().unwrap();
        let (ts, machine, seq) = decode(id);
        assert_eq!((ts, machine, seq), (42, 3, 0));
    }

    /// 序列耗尽：一毫秒 4096 个后报 SequenceExhausted，下一毫秒恢复
    #[test]
    fn sequence_exhaustion_and_recovery() {
        let (clock, mut sf) = generator(0, 0);

        // 4096 个（seq 0..=4095）
        for _ in 0..=MAX_SEQUENCE {
            sf.next_id().unwrap();
        }
        assert_eq!(sf.next_id(), Err(SnowflakeError::SequenceExhausted));
        // 时间前进 1ms：恢复发号
        clock.advance(1);
        let id = sf.next_id().unwrap();
        assert_eq!(decode(id).0, 1, "新毫秒的时间戳");
        assert_eq!(decode(id).2, 0, "序列归零");
    }

    /// 时钟回拨：容忍阈值内拒绝发号（可重试），时钟追平后恢复
    #[test]
    fn small_clock_backwards_is_rejected_then_recovers() {
        let (clock, mut sf) = generator(1000, 0);
        sf.next_id().unwrap();

        clock.set(997); // 回拨 3ms（≤ 5）
        assert_eq!(
            sf.next_id(),
            Err(SnowflakeError::ClockMovedBackwards { backwards: 3 })
        );

        clock.set(1001); // 时钟追平并超过
        let id = sf.next_id().unwrap();
        assert_eq!(decode(id).0, 1001);
    }

    /// 大幅回拨：同样报错（错误值携带回拨量，运维可分辨严重程度）
    #[test]
    fn large_backwards_reports_amount() {
        let (clock, mut sf) = generator(100_000, 0);
        sf.next_id().unwrap();

        clock.set(50_000); // 回拨 50000ms
        assert_eq!(
            sf.next_id(),
            Err(SnowflakeError::ClockMovedBackwards {
                backwards: 50_000
            })
        );
    }

    /// 并发唯一性：8 线程各持独立生成器（不同 machine_id）× 10000 个 ID 无一重复。
    /// 雪花的部署约定：一个 machine_id 只属于一个进程/线程组。
    #[test]
    fn concurrent_generators_produce_unique_ids() {
        use std::collections::HashSet;
        use std::sync::Arc;

        const THREADS: u64 = 8;
        const PER_THREAD: usize = 10_000;

        let mut all_ids = Vec::with_capacity(THREADS as usize * PER_THREAD);
        let mut handles = Vec::new();
        for t in 0..THREADS {
            handles.push(std::thread::spawn(move || {
                let mut sf = Snowflake::new(t, Arc::new(SystemClock));
                (0..PER_THREAD)
                    .map(|_| loop {
                        match sf.next_id() {
                            Ok(id) => break id,
                            // 单毫秒 4096 个用尽是正常约束：生产方的
                            // 标准姿势是等到下一毫秒再取号。
                            Err(SnowflakeError::SequenceExhausted) => {
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                            Err(e) => panic!("发号失败: {e}"),
                        }
                    })
                    .collect::<Vec<_>>()
            }));
        }
        for h in handles {
            all_ids.extend(h.join().unwrap());
        }

        let unique: HashSet<_> = all_ids.iter().collect();
        assert_eq!(unique.len(), all_ids.len(), "存在重复 ID");
    }

    /// Display 可读（日志友好）
    #[test]
    fn error_display_is_helpful() {
        let e = SnowflakeError::ClockMovedBackwards { backwards: 42 };
        assert!(e.to_string().contains("42"));
        assert!(SnowflakeError::SequenceExhausted.to_string().contains("sequence"));
    }
}
