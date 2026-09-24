//! seq 去重窗口：位图滑动窗口（算法图谱 #4）。
//!
//! # 解决什么问题
//!
//! ACK 丢失导致发送方重发、网络重排导致乱序到达——接收方需要回答：
//! **「这个 seq 我见过吗？」**。朴素答案是 `HashSet<u64>`（每个 seq
//! 至少 8 字节 + 哈希开销）；本模块用**一个 `u64` 位图**回答同样的问题：
//!
//! ```text
//! seq:        ... 41  42  43  44  45  46  47 ...
//!                  └─ rcv_nxt（下一个期望：43 之前的已收齐）
//! bitmap 位:        bit0 bit1 bit2 ...  bit62
//!                  （表示 rcv_nxt+1 ..= rcv_nxt+63 是否已收到）
//! ```
//!
//! 64 字节的窗口覆盖 64 个 seq，每个 seq 的判定 O(1) 几条位指令——
//! 这就是 TCP 接收窗口的位图版（对照 Linux 内核 `struct tcp_sock`
//! 的 `rcv_wnd` 管理）。
//!
//! # 语义（与 TCP 同族）
//!
//! - `seq == rcv_nxt`：按序新帧，窗口右滑并**自动吸收**位图低位的
//!   连续已收乱序帧（补洞）；
//! - `rcv_nxt < seq <= rcv_nxt + 63`：乱序超前，置位缓存；重复置位 = 重复帧；
//! - `seq < rcv_nxt`：窗口后面的旧帧，重复；
//! - `seq > rcv_nxt + 63`：超窗（发送方窗口失控），交调用方决策。
//!
//! # 模式落点
//!
//! - 算法：位图滑动窗口、`trailing_zeros` 补洞；
//! - 类型即文档：[`Verdict`] 四变体让「怎么处理这一帧」不需要 if-else 链。

use im_protocol::Frame;

/// 单个 seq 的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 按序新帧（正好是期望的 seq，可立即上递业务层）。
    InOrder,
    /// 乱序新帧（已缓存进位图，等待前面的洞补上）。
    OutOfOrder,
    /// 重复帧（可安全丢弃；应重发 ACK 提醒对端）。
    Duplicate,
    /// 超出窗口前方（窗口尺寸 64；发送方与接收方进度严重脱节）。
    ///
    /// 常见于接收方重启丢失状态——调用方应考虑重同步。
    TooFar {
        /// 期望的下一个 seq（对端可据此对齐）。
        expected: u64,
    },
}

/// seq 去重窗口：一个接收方向一个 [`Verdict`]。
///
/// 每条连接（每个接收方向）一个实例；`u64` 窗口内零分配。
///
/// # Examples
///
/// ```
/// use im_transport::{DedupWindow, Verdict};
///
/// let mut w = DedupWindow::new(1);
/// assert_eq!(w.feed(1), Verdict::InOrder);   // 按序
/// assert_eq!(w.feed(3), Verdict::OutOfOrder); // 乱序缓存
/// assert_eq!(w.feed(3), Verdict::Duplicate);  // 重复
/// assert_eq!(w.feed(2), Verdict::InOrder);    // 补上洞：2 和 3 连续吸收
/// assert_eq!(w.feed(3), Verdict::Duplicate);  // 已越过
/// assert_eq!(w.ack(), 4);                     // 「4 之前的我全收齐了」
/// ```
pub struct DedupWindow {
    /// 下一个期望的 seq（`rcv_nxt`）：严格小于它的都已收齐。
    rcv_nxt: u64,
    /// 位图：bit `i` 表示 seq `rcv_nxt + 1 + i` 已收到（乱序缓存）。
    ///
    /// 不含 `rcv_nxt` 本身——它没到，所以 `rcv_nxt` 才是「下一个期望」。
    bitmap: u64,
}

/// 窗口尺寸：位图位数（不含 `rcv_nxt` 本身）。
pub const WINDOW_SIZE: u64 = 64;

impl DedupWindow {
    /// 以给定的初始 seq（对端声明的起始 seq）创建。
    #[must_use]
    pub fn new(initial_seq: u64) -> Self {
        Self {
            rcv_nxt: initial_seq,
            bitmap: 0,
        }
    }

    /// 喂入一个收到的 seq，返回判定（见 [`Verdict`]）。
    #[must_use]
    pub fn feed(&mut self, seq: u64) -> Verdict {
        if seq < self.rcv_nxt {
            // 窗口后方：进度已被越过，重复
            return Verdict::Duplicate;
        }
        if seq == self.rcv_nxt {
            // 按序：推进一格；位图的语义基准随之前移——
            // 旧 bit j 表示 rcv_nxt_old+1+j = rcv_nxt_new+j，
            // 于是 bit0 恰好是「新的 rcv_nxt」是否已缓存：
            // 为 1 则吸收并继续，直到遇到第一个洞。
            // wrapping：seq 空间是环，u64::MAX 的下一格是 0（回绕安全）。
            self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
            while self.bitmap & 1 == 1 {
                self.bitmap >>= 1;
                self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
            }
            // 弹出终止循环的 0 位，恢复不变式「bit j = rcv_nxt+1+j」
            self.bitmap >>= 1;
            return Verdict::InOrder;
        }
        // 前方：offset ∈ [1, WINDOW_SIZE] 可缓存
        let offset = seq - self.rcv_nxt; // > 0
        if offset > WINDOW_SIZE {
            return Verdict::TooFar {
                expected: self.rcv_nxt,
            };
        }
        let mask = 1u64 << (offset - 1);
        if self.bitmap & mask != 0 {
            return Verdict::Duplicate;
        }
        self.bitmap |= mask;
        Verdict::OutOfOrder
    }

    /// 累计确认值：严格小于它的 seq 全部收齐（语义同 TCP ACK）。
    ///
    /// 回 ACK 帧时填帧头的 `ack` 字段。
    #[must_use]
    pub fn ack(&self) -> u64 {
        self.rcv_nxt
    }

    /// 位图里缓存了多少个乱序帧（诊断指标）。
    #[must_use]
    pub fn backlog(&self) -> u32 {
        self.bitmap.count_ones()
    }
}

/// 把「收到一帧后的去重 + ACK 回填」组合成一步。
///
/// 网关读循环对每条**上行业务帧**（`Msg` / `Handshake` / `SyncReq`）调用：
/// `feed` 判定重复则直接丢弃，同时把 `ack()` 填进回执帧头——
/// 去重与确认一体化，业务层不用关心 seq。
///
/// 心跳 `Ping`/`Pong` 不参与去重（无 seq 语义负担）。
///
/// # Examples
///
/// ```
/// use bytes::Bytes;
/// use im_protocol::{Cmd, Frame};
/// use im_transport::{DedupWindow, Verdict};
///
/// let mut w = DedupWindow::new(1);
/// let frame = Frame::new(Cmd::Msg, 1, 0, Bytes::new());
///
/// match w.feed_frame(&frame) {
///     Verdict::InOrder => { /* 上递业务层，回执帧的 ack 已填好 */ }
///     Verdict::Duplicate => { /* 丢弃 */ }
///     v => panic!("unexpected: {v:?}"),
/// }
/// assert_eq!(w.ack(), 2);
/// ```
impl DedupWindow {
    /// 与 [`feed`] 相同的判定，但直接吃 [`Frame`]（取 `frame.seq`）。
    #[must_use]
    pub fn feed_frame(&mut self, frame: &Frame) -> Verdict {
        self.feed(frame.seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按序序列：1..=100 全部 InOrder，ack 逐步 +1
    #[test]
    fn in_order_sequence() {
        let mut w = DedupWindow::new(1);
        for seq in 1..=100u64 {
            assert_eq!(w.feed(seq), Verdict::InOrder, "seq={seq}");
            assert_eq!(w.ack(), seq + 1);
        }
    }

    /// 重复帧：同 seq 二次到达必须 Duplicate
    #[test]
    fn duplicates_are_detected() {
        let mut w = DedupWindow::new(1);
        assert_eq!(w.feed(1), Verdict::InOrder);
        assert_eq!(w.feed(1), Verdict::Duplicate);
        assert_eq!(w.feed(2), Verdict::InOrder);
        assert_eq!(w.feed(2), Verdict::Duplicate);
        assert_eq!(w.feed(1), Verdict::Duplicate);
    }

    /// 乱序 + 补洞：3 先到（缓存）、2 后到 → 一次 feed 吸收两格
    #[test]
    fn out_of_order_then_gap_fill() {
        let mut w = DedupWindow::new(1);
        assert_eq!(w.feed(1), Verdict::InOrder);
        assert_eq!(w.feed(3), Verdict::OutOfOrder);
        assert_eq!(w.backlog(), 1, "缓存了一个乱序帧");
        assert_eq!(w.feed(2), Verdict::InOrder);
        // 2 到达时 3 已在位图：rcv_nxt 应从 2 直接跳到 4
        assert_eq!(w.ack(), 4);
        assert_eq!(w.backlog(), 0, "洞补上后位图清空");
        assert_eq!(w.feed(3), Verdict::Duplicate);
    }

    /// 大洞散布：位图各处置位，洞补上时全部吸收
    #[test]
    fn scattered_holes_collapse() {
        let mut w = DedupWindow::new(0);
        // 先收 1,2,4,5（缺 3）
        for seq in [1u64, 2, 4, 5] {
            let v = w.feed(seq);
            assert!(matches!(v, Verdict::OutOfOrder), "seq={seq} 应乱序缓存");
        }
        assert_eq!(w.ack(), 0, "洞未补，累计确认不动");
        // 0 到达：吸收 0,1,2 后停在洞（3 未到），4,5 仍缓存
        assert_eq!(w.feed(0), Verdict::InOrder);
        assert_eq!(w.ack(), 3);
        assert_eq!(w.backlog(), 2);
        // 3 到达：剩下的 4,5 连着一起吸收完
        assert_eq!(w.feed(3), Verdict::InOrder);
        assert_eq!(w.ack(), 6);
        assert_eq!(w.backlog(), 0);
    }

    /// 超窗：`seq` 落在窗口之外返回 `TooFar` 并携带期望值
    #[test]
    fn beyond_window_is_too_far() {
        let mut w = DedupWindow::new(100);
        assert!(matches!(
            w.feed(100 + WINDOW_SIZE + 1),
            Verdict::TooFar { expected: 100 }
        ));
        // 恰好窗口边界（rcv_nxt + 64）还能缓存
        assert_eq!(w.feed(100 + WINDOW_SIZE), Verdict::OutOfOrder);
    }

    /// 窗口滑走后，曾缓存的乱序帧变成「后方重复」
    #[test]
    fn cached_frames_become_duplicates_after_slide() {
        let mut w = DedupWindow::new(10);
        // 11..=20 全部乱序缓存
        for seq in 11..=20u64 {
            assert_eq!(w.feed(seq), Verdict::OutOfOrder);
        }
        // 10 到达：一次吸收 10..=20，rcv_nxt = 21
        assert_eq!(w.feed(10), Verdict::InOrder);
        assert_eq!(w.ack(), 21);
        for seq in 11..=20u64 {
            assert_eq!(w.feed(seq), Verdict::Duplicate, "seq={seq} 已被吸收");
        }
    }

    /// seq 回绕安全：从 `u64::MAX` 附近开始，回绕后判定依然正确
    #[test]
    fn wraparound_is_safe() {
        let mut w = DedupWindow::new(u64::MAX);
        assert_eq!(w.feed(u64::MAX), Verdict::InOrder);
        // rcv_nxt 回绕到 0
        assert_eq!(w.ack(), 0);
        assert_eq!(w.feed(2), Verdict::OutOfOrder);
        assert_eq!(w.feed(0), Verdict::InOrder);
        assert_eq!(w.feed(1), Verdict::InOrder);
        assert_eq!(w.ack(), 3, "2 的洞补上后连续吸收");
    }

    /// 对拍性质测试：窗口范围内的随机 seq 流，
    /// `DedupWindow` 的「新/旧」判定必须与 `HashSet` 完全一致。
    ///
    /// 这是算法正确性的金标准——两个独立实现给出相同答案。
    #[test]
    fn agrees_with_hashset_on_random_streams() {
        // 简易 xorshift，固定种子可复现
        let mut rng: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        for trial in 0..20 {
            let base = trial * 1000;
            let mut w = DedupWindow::new(base);
            let mut seen = std::collections::HashSet::new();

            for _ in 0..2000 {
                // 只生成窗口内或稍后的 seq，避免 TooFar 干扰对拍
                let seq = base + (next() % 60);
                let verdict = w.feed(seq);
                let is_new = seen.insert(seq);
                match verdict {
                    Verdict::InOrder | Verdict::OutOfOrder => {
                        assert!(is_new, "seq={seq} 判为新但 HashSet 早已见过");
                    }
                    Verdict::Duplicate => {
                        assert!(!is_new, "seq={seq} 判为重复但 HashSet 首见");
                    }
                    Verdict::TooFar { .. } => unreachable!("seq 不会超窗"),
                }
                // ack 语义对拍：ack 之前的所有 seq 都在 seen 里
                for s in base..w.ack() {
                    assert!(seen.contains(&s), "ack={} 但 seq={s} 未收过", w.ack());
                }
            }
        }
    }
}
