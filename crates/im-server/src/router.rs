//! 会话路由表：手写分片并发哈希表（Sharded HashMap，算法图谱 #7）。
//!
//! # 解决什么问题
//!
//! 网关要维护「`user_id` → 连接句柄」的全局映射，被所有连接 task 并发读写。
//! 朴素方案 `RwLock<HashMap>`：**一把锁罩住全表**——百万连接下，
//! 每条消息的路由查询都要排队，锁成为吞吐上限。
//!
//! 分片思想（`DashMap` 的内核）：把表切成 N 片，**每片独立加锁**——
//! 不同 key 落到不同片就互不阻塞，锁冲突概率随片数线性下降：
//!
//! ```text
//! user_id ──hash──▶ ┌─ shard 0 : Mutex<HashMap>  ←── 命中片 0 的读写
//!                  ├─ shard 1 : Mutex<HashMap>
//!                  ├─   ...    （N = 下一个 2 的幂）
//!                  └─ shard N-1: Mutex<HashMap>  ←── 命中片 N-1 的读写
//! ```
//!
//! **分片数取 2 的幂**：`hash & (N-1)` 一条位与替代取模（除法指令
//! 数十周期，位与 1 周期）——与 [`crate::snowflake`] 的位段提取同一思想。
//!
//! # 为什么用 `std::sync::Mutex` 而不是 `tokio::sync::Mutex`
//!
//! 临界区内只有 `HashMap` 的查找/插入/移除——**微秒级的纯内存操作，
//! 不跨 await**。`std::sync::Mutex` 更快（无 tokio 调度参与），
//! 且「不跨 await 持锁」是 Rust 异步的铁律之一
//! （跨 await 的锁会让整个 worker 线程卡住，见 docs/03）。
//!
//! # 模式落点
//!
//! - 算法：锁分片、位与取模、`SipHash`（`std` 默认 hasher，抗哈希碰撞攻击）；
//! - RAII：`MutexGuard` 的作用域即临界区，离开即解锁——编译器保证。

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, Mutex};

/// 路由表错误。
#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    /// 该用户已在线：单端登录策略下新连接顶不掉旧连接（阶段 3 简化：
    /// 拒绝重复登录；多端登录是阶段 6 的话题）。
    #[error("user {user_id} already online")]
    AlreadyOnline {
        /// 重复登录的用户 ID。
        user_id: u64,
    },
}

/// 分片并发路由表：`user_id → V`。
///
/// `V` 通常是 `ConnectionHandle`（可克隆的发送端），但本表刻意泛型——
/// 不依赖 im-transport，纯数据结构，可以独立测试与复用。
///
/// # Examples
///
/// ```
/// use im_server::router::Router;
///
/// let router = Router::new(16);
/// router.register(42, "conn-a".to_string()).unwrap();
/// assert_eq!(router.get(42), Some("conn-a".to_string()));
///
/// router.unregister(42, &"conn-a".to_string());
/// assert_eq!(router.get(42), None);
/// ```
pub struct Router<V> {
    /// 分片数组：固定长度，每片一把锁。
    shards: Box<[Mutex<HashMap<u64, V>>]>,
    /// 哈希构造器（`RandomState` 抗碰撞攻击，见模块文档）。
    hasher: RandomState,
}

impl<V: Clone> Router<V> {
    /// 创建 `shard_count` 个分片（内部取整到不小于它的 2 的幂）。
    ///
    /// 经验值：分片数 ≈ CPU 核数 × 4~16，或干脆 64/128——
    /// 每片只是一把 `Mutex`（几十字节），多分几乎白给。
    #[must_use]
    pub fn new(shard_count: usize) -> Self {
        // 下一个 2 的幂（至少 1）：位技巧——最高位以下全部置 1 后 +1
        let pow = (shard_count.max(1)).next_power_of_two();
        let shards = (0..pow)
            .map(|_| Mutex::new(HashMap::new()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            shards,
            hasher: RandomState::new(),
        }
    }

    /// key → 分片下标：高位哈希 + 位与取模。
    ///
    /// （截断 allow：32 位目标上 usize 截断哈希高位也无碍——
    /// 掩码只取低位，均匀性不受影响。）
    #[allow(clippy::cast_possible_truncation)]
    fn shard_index(&self, key: u64) -> usize {
        // hash_one：把 u64 哈希成 u64（SipHash 1-3，抗碰撞）
        let hash = self.hasher.hash_one(key);
        (hash as usize) & (self.shards.len() - 1)
    }

    /// 注册：用户上线。已在线返回 [`RouterError::AlreadyOnline`]。
    ///
    /// # Errors
    ///
    /// 该 `user_id` 已注册时返回 [`RouterError::AlreadyOnline`]。
    ///
    /// # Panics
    ///
    /// 分片锁中毒（持锁线程 panic）时 panic。
    pub fn register(&self, user_id: u64, value: V) -> Result<(), RouterError> {
        let shard = &self.shards[self.shard_index(user_id)];
        let mut guard = shard.lock().expect("路由表锁中毒");
        if guard.contains_key(&user_id) {
            return Err(RouterError::AlreadyOnline { user_id });
        }
        guard.insert(user_id, value);
        Ok(())
    }

    /// 注销：用户下线。带值校验——**只有持有正确句柄的一方**才能注销，
    /// 防止旧连接的收尾逻辑误删新连接的路由（重连竞态的经典坑）。
    ///
    /// 返回是否真的移除（`false` = key 不存在或值不匹配）。
    ///
    /// # Panics
    ///
    /// 分片锁中毒（持锁线程 panic）时 panic。
    pub fn unregister(&self, user_id: u64, expect: &V) -> bool
    where
        V: PartialEq,
    {
        let shard = &self.shards[self.shard_index(user_id)];
        let mut guard = shard.lock().expect("路由表锁中毒");
        match guard.get(&user_id) {
            Some(current) if current == expect => {
                guard.remove(&user_id);
                true
            }
            _ => false, // 不存在或已被新连接顶替：不动
        }
    }

    /// 谓词版注销：当当前值满足 `predicate` 时才移除，返回是否移除。
    ///
    /// [`unregister`](Self::unregister) 的泛化（值相等是一个谓词）——
    /// 适合值无法廉价构造（如内部带 channel 的句柄）、
    /// 但能回答「这条路由是不是我的」的场景。
    ///
    /// # Panics
    ///
    /// 分片锁中毒（持锁线程 panic）时 panic。
    pub fn remove_if(&self, user_id: u64, predicate: impl FnOnce(&V) -> bool) -> bool {
        let shard = &self.shards[self.shard_index(user_id)];
        let mut guard = shard.lock().expect("路由表锁中毒");
        if guard.get(&user_id).is_some_and(|value| predicate(value)) {
            guard.remove(&user_id);
            true
        } else {
            false
        }
    }

    /// 查询：用户是否在线，在线返回句柄克隆。
    ///
    /// 为什么不返回引用？锁的守卫不能交出临界区（否则调用方握着锁
    /// 干别的事，分片白分了）——**克隆句柄、立刻放锁**是标准姿势；
    /// `ConnectionHandle` 的克隆只是 channel sender 的引用计数 +1。
    ///
    /// # Panics
    ///
    /// 分片锁中毒（持锁线程 panic）时 panic。
    #[must_use]
    pub fn get(&self, user_id: u64) -> Option<V> {
        let shard = &self.shards[self.shard_index(user_id)];
        let guard = shard.lock().expect("路由表锁中毒");
        guard.get(&user_id).cloned()
    }

    /// 当前在线总数（诊断指标：遍历各片求和，每片瞬时加锁）。
    ///
    /// # Panics
    ///
    /// 任一分片锁中毒时 panic。
    #[must_use]
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().expect("路由表锁中毒").len())
            .sum()
    }

    /// 是否没有任何在线用户。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// Router 手动实现 Clone 要求 V: Clone——分片锁各克隆一份
impl<V: Clone> Clone for Router<V> {
    fn clone(&self) -> Self {
        let shards = self
            .shards
            .iter()
            .map(|s| {
                let guard = s.lock().expect("路由表锁中毒");
                Mutex::new(guard.clone())
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        // 克隆 hasher 保持分片映射一致（重要！否则同一 key 两实例分片不同）
        Self {
            shards,
            hasher: self.hasher.clone(),
        }
    }
}

/// 共享句柄：`Arc<Router<V>>` 的类型别名（网关各 task 持有）。
pub type SharedRouter<V> = Arc<Router<V>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::TryLockError;
    use std::sync::Arc;

    /// 基本 CRUD + 重复注册拒绝
    #[test]
    fn register_get_unregister() {
        let r = Router::new(8);
        r.register(1, "a".to_string()).unwrap();
        assert_eq!(r.get(1), Some("a".to_string()));

        // 重复注册被拒
        assert!(matches!(
            r.register(1, "b".to_string()),
            Err(RouterError::AlreadyOnline { user_id: 1 })
        ));
        // 注册失败不覆盖旧值
        assert_eq!(r.get(1), Some("a".to_string()));

        assert!(r.unregister(1, &"a".to_string()));
        assert_eq!(r.get(1), None);
        assert!(r.is_empty());
    }

    /// 值校验注销：错误的值不能移除路由（重连竞态防护）
    #[test]
    fn unregister_requires_matching_value() {
        let r = Router::new(4);
        r.register(7, "old".to_string()).unwrap();
        // 旧连接拿 "old"，但表里已被新连接 "new" 顶替（假设某种路径）
        // 简化测试：值不匹配 → 不移除
        assert!(!r.unregister(7, &"wrong".to_string()));
        assert_eq!(r.get(7), Some("old".to_string()));
        assert!(!r.unregister(999, &"old".to_string()));
    }

    /// 分片均匀性：一万个 key 分布在所有分片（分片数取 2 的幂的意义）
    #[test]
    fn keys_spread_across_shards() {
        let r = Router::new(64);
        for id in 0..10_000u64 {
            r.register(id, ()).unwrap();
        }
        // 每片至少有 10 个 key（均匀性下限：10000/64 ≈ 156，泊松分布下
        // 最小片 >10 的概率极高；若分片逻辑坏了（全落一片）会 0）
        for shard in &r.shards {
            let len = shard.lock().unwrap().len();
            assert!(len > 10, "分片分布不均: 某片只有 {len}");
        }
        assert_eq!(r.len(), 10_000);
    }

    /// 并发读写：16 线程各注册/查询/注销自己的 key 区间，最终表清空
    #[test]
    fn concurrent_access_no_deadlock_no_loss() {
        const THREADS: u64 = 16;
        const KEYS_PER_THREAD: u64 = 1000;

        let router = Arc::new(Router::new(64));

        // 1. 并发注册
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let r = Arc::clone(&router);
            handles.push(std::thread::spawn(move || {
                for i in 0..KEYS_PER_THREAD {
                    let key = t * KEYS_PER_THREAD + i;
                    r.register(key, format!("v{key}")).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(router.len(), usize::try_from(THREADS * KEYS_PER_THREAD).unwrap());

        // 2. 并发查询
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let r = Arc::clone(&router);
            handles.push(std::thread::spawn(move || {
                for i in 0..KEYS_PER_THREAD {
                    let key = t * KEYS_PER_THREAD + i;
                    assert_eq!(r.get(key), Some(format!("v{key}")), "key={key}");
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // 3. 并发注销
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let r = Arc::clone(&router);
            handles.push(std::thread::spawn(move || {
                for i in 0..KEYS_PER_THREAD {
                    let key = t * KEYS_PER_THREAD + i;
                    let v = format!("v{key}");
                    assert!(r.unregister(key, &v), "key={key} 注销失败");
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(router.is_empty(), "全部注销后表应清空");
    }

    /// Clone 保持分片映射一致：克隆实例能查到原实例注册的 key
    #[test]
    fn clone_preserves_routing() {
        let r = Router::new(8);
        r.register(42, "hello".to_string()).unwrap();
        let r2 = r.clone();
        assert_eq!(r2.get(42), Some("hello".to_string()));
    }

    /// 分片数自动取 2 的幂：new(3) 实际 4 片、new(100) 实际 128 片
    #[test]
    fn shard_count_rounds_to_power_of_two() {
        assert_eq!(Router::<()>::new(3).shards.len(), 4);
        assert_eq!(Router::<()>::new(100).shards.len(), 128);
        assert_eq!(Router::<()>::new(64).shards.len(), 64);
        assert_eq!(Router::<()>::new(0).shards.len(), 1, "0 归一");
    }

    /// `TryLock` 语义冒烟：锁不可重入，但释放后立即可再取
    /// （防呆测试：确认我们没在 guard 存活期间递归加锁同一分片）
    #[test]
    fn locks_are_reentrant_free() {
        let r = Router::<String>::new(2);
        let shard = &r.shards[0];
        let guard = shard.try_lock().unwrap();
        // 同一片二次 try_lock 必须失败（非重入）
        assert!(matches!(shard.try_lock(), Err(TryLockError::WouldBlock)));
        drop(guard); // RAII 释放
        assert!(shard.try_lock().is_ok());
    }
}
