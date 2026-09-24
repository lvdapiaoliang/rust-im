//! 有界去重窗口（阶段 4，接收侧）。
//!
//! 发送端「至少一次」重传的代价是接收端会看到重复：同一条消息可能
//! 经实时投递、离线补投、重发投递三条路径各到一次。去重靠发送方
//! 生成的稳定键 `(from, client_msg_id)`——服务端 `msg_id` 在重发后
//! 会换新值，不能当去重键（这正是 p4-1 给协议加 `client_msg_id`
//! 的理由）。
//!
//! # 算法落点
//!
//! **HashSet（O(1) 查重）+ VecDeque（FIFO 逐出）**：窗口满了淘汰
//! 最旧的。对照：纯 HashSet 无法逐出（内存无限涨）；纯 Vec 线性扫
//! （O(n)）；带时间戳的 LRU 是过度设计——重复的时效性由重传 RTO
//! （秒级）决定，1024 条的 FIFO 足以覆盖任何现实的重传窗口。

use std::collections::{HashSet, VecDeque};

/// 默认窗口容量（条数）。pub(crate) 供连接状态机引用。
pub(crate) const DEFAULT_CAPACITY: usize = 1024;

/// 有界去重窗口：见过的键留下，满了逐出最旧的。
pub(crate) struct DedupWindow {
    seen: HashSet<(u64, u64)>,
    order: VecDeque<(u64, u64)>,
    capacity: usize,
}

impl DedupWindow {
    /// 建窗口。容量至少为 1（0 容量等于「什么都记不住」，没有意义）。
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// 记录并查重：`true` = 第一次见（放行），`false` = 重复（丢弃）。
    pub(crate) fn admit(&mut self, key: (u64, u64)) -> bool {
        if self.seen.contains(&key) {
            return false;
        }
        if self.order.len() == self.capacity {
            // 满了：逐出最旧的，为新键腾位（HashSet 与 VecDeque 同步删）
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        self.order.push_back(key);
        self.seen.insert(key);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 首见放行，重复拦截。
    #[test]
    fn admits_first_and_rejects_duplicate() {
        let mut window = DedupWindow::new(4);
        assert!(window.admit((1, 100)));
        assert!(!window.admit((1, 100)), "重复应被拦截");
        // 同 client_msg_id 不同 from：不同发送方，各自独立
        assert!(window.admit((2, 100)));
    }

    /// 窗口有界：满后逐出最旧的，被逐出的键会再次放行。
    #[test]
    fn evicts_oldest_when_full() {
        let mut window = DedupWindow::new(2);
        assert!(window.admit((1, 1)));
        assert!(window.admit((1, 2)));
        assert!(window.admit((1, 3)), "满了逐出最旧，新键照常放行");
        assert!(window.admit((1, 1)), "被逐出的键视为没见过（有界语义）");
        assert!(!window.admit((1, 3)), "窗口内的键仍被拦截");
    }
}
