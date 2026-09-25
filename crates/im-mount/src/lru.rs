//! 手写 LRU 缓存（阶段 13，roadmap 数据结构表承诺的「挂载盘目录缓存：双向链表 + 哈希表」）。
//!
//! # 为什么不用 `lru` crate
//!
//! 阶段 3 的去重窗口、阶段 7 的成员快照用的都是现成容器上叠逻辑；
//! 这里是项目里**真正从零手写**的复杂数据结构——LRU 的经典实现
//! 「哈希表管定位、双向链表管时序」恰好把两个基础数据结构组合出
//! O(1) 的 get/put，是数据结构课的招牌应用题。挂载盘目录缓存是
//! 它在本项目的落地场景（[`crate::dir_cache`]）。
//!
//! # O(1) 的原理
//!
//! ```text
//!   map: HashMap<K, slot 下标>        ← O(1) 定位
//!   slots: Vec<Slot> + head/tail 索引  ← 双向链表管「谁最新谁最旧」
//!
//!   get(k)：map 找到 slot → 链表摘下 → 插回表头        O(1)
//!   put(k,v)：已存在则更新并提到表头；满了淘汰表尾      O(1)
//! ```
//!
//! # 为什么用「下标链表」而不是 `Box` + 指针
//!
//! 经典教科书实现用裸指针链 `Box<Node>`（拿 prev 指针要 unsafe）。
//! 本实现把节点放进 `Vec<Slot>`（slab），prev/next 存**下标**——
//! 全程零 unsafe（workspace 钉 `unsafe_code = "warn"`，SDK 层之外
//! 不写 unsafe 的纪律在这里兑现），代价只是删除节点时 slot 进
//! free-list 复用，缓存容量稳定后 Vec 不再增长。
//!
//! # K: Clone 的来由
//!
//! 哈希表和链表节点各需要一份 key（淘汰表尾时要从 map 里删掉对应
//! 项，此时 key 只在节点手里）——所以插入时 clone 一份，如实写进
//! 泛型边界而不是用 unsafe 挪动。

use std::collections::HashMap;
use std::hash::Hash;

/// 链表槽位：值 + 双向链表的 prev/next 下标。
#[derive(Debug)]
struct Slot<K, V> {
    key: K,
    value: V,
    prev: Option<usize>,
    next: Option<usize>,
}

/// O(1) LRU 缓存（最近使用在表头，淘汰从表尾）。
///
/// # Examples
///
/// ```
/// use im_mount::lru::LruCache;
///
/// let mut cache: LruCache<String, u32> = LruCache::new(2);
/// cache.put("a".into(), 1);
/// cache.put("b".into(), 2);
/// assert_eq!(cache.get(&"a".to_string()), Some(&1)); // a 被提到表头
/// cache.put("c".into(), 3);                          // 淘汰的是 b（最久未用）
/// assert_eq!(cache.get(&"b".to_string()), None);
/// assert_eq!(cache.get(&"c".to_string()), Some(&3));
/// ```
#[derive(Debug)]
pub struct LruCache<K, V> {
    /// 节点池（slab）：occupied 的节点挂在链表上，空位在 free 里复用
    slots: Vec<Option<Slot<K, V>>>,
    /// 空槽下标栈：淘汰/删除的槽位回到这里，put 时优先复用
    free: Vec<usize>,
    /// key → slot 下标（O(1) 定位）
    map: HashMap<K, usize>,
    /// 表头（最近使用）；空缓存时为 None
    head: Option<usize>,
    /// 表尾（最久未用，淘汰对象）
    tail: Option<usize>,
    capacity: usize,
}

impl<K, V> LruCache<K, V>
where
    K: Hash + Eq + Clone,
{
    /// 构造指定容量的缓存。容量为 0 表示「什么都不存」（put 直接淘汰）。
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            map: HashMap::new(),
            head: None,
            tail: None,
            capacity,
        }
    }

    /// 当前条目数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 容量上限。
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 查缓存（**命中会把条目提到表头**——「使用」就包括读）。
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let idx = *self.map.get(key)?;
        self.move_to_front(idx);
        Some(self.slot_ref(idx).value_ref())
    }

    /// 查缓存的可变引用（同样会提到表头）。
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let idx = *self.map.get(key)?;
        self.move_to_front(idx);
        Some(self.slot_mut(idx).value_mut())
    }

    /// 只看不刷新时序（调试/统计用，不影响淘汰顺序）。
    #[must_use]
    pub fn peek(&self, key: &K) -> Option<&V> {
        let idx = self.map.get(key).copied()?;
        Some(self.slot_ref(idx).value_ref())
    }

    /// 是否存在（不刷新时序——`get` 才算「使用」）。
    #[must_use]
    pub fn contains_key(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// 插入/更新。返回被淘汰的 `(key, value)`（容量满时的表尾），
    /// 调用方可以借机做资源回收（比如关掉被逐出目录的文件句柄）。
    ///
    /// # Panics
    ///
    /// 不 panic——但注意容量 0 的缓存会当场淘汰刚插入的条目
    /// （返回值就是它自己，测试 `zero_capacity_evicts_immediately`
    /// 锁死这个行为），这个行为与 `lru` crate 一致。
    pub fn put(&mut self, key: K, value: V) -> Option<(K, V)> {
        // 已存在：更新值、提到表头（key 留在原槽位，新 key 直接丢弃）
        if let Some(&idx) = self.map.get(&key) {
            self.slot_mut(idx).value = value;
            self.move_to_front(idx);
            return None;
        }

        // 插入新条目：空槽复用优先，否则尾部追加。
        // （淘汰放在插入之后：容量 0 时刚插入的项自己就是表尾，
        // 「插入即淘汰」自然发生；容量 n 时先到 n+1 再踢回 n，
        // 两种情况同一条代码路径——先淘汰后插入在容量 0 时会落空，
        // 这个边界用例当初就是这么写出来的）
        let idx = match self.free.pop() {
            Some(idx) => {
                *self.slot_at(idx) = Some(Slot { key: key.clone(), value, prev: None, next: None });
                idx
            }
            None => {
                self.slots.push(Some(Slot { key: key.clone(), value, prev: None, next: None }));
                self.slots.len() - 1
            }
        };
        self.push_front(idx);
        self.map.insert(key, idx);
        // 超容 → 淘汰表尾（最久未用）
        let evicted = if self.map.len() > self.capacity {
            self.evict_tail()
        } else {
            None
        };
        evicted
    }

    /// 主动删除某条（与淘汰同一条清理路径）。
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let idx = self.map.remove(key)?;
        self.unlink(idx);
        let slot = self.slot_at(idx).take()?;
        self.free.push(idx);
        Some(slot.value)
    }

    /// 清空（容量不变）。
    pub fn clear(&mut self) {
        self.slots.clear();
        self.free.clear();
        self.map.clear();
        self.head = None;
        self.tail = None;
    }

    /// 按新→旧列出所有 key（测试与调试用；生产代码别依赖遍历顺序）。
    #[must_use]
    pub fn mru_order(&self) -> Vec<&K> {
        let mut order = Vec::with_capacity(self.map.len());
        let mut cur = self.head;
        while let Some(idx) = cur {
            let slot = self.slot_ref(idx);
            order.push(slot.key_ref());
            cur = slot.next;
        }
        order
    }

    // ── 内部：链表三件套（unlink / push_front / move_to_front）──

    /// 从链表摘下 idx（不动 map/槽位，纯指针操作——下标版）。
    fn unlink(&mut self, idx: usize) {
        let (prev, next) = {
            let slot = self.slot_ref(idx);
            (slot.prev, slot.next)
        };
        // 修补前驱的 next / 后继的 prev；边界（表头/表尾）同步修正
        if let Some(p) = prev {
            self.slot_mut(p).next = next;
        } else {
            self.head = next;
        }
        if let Some(n) = next {
            self.slot_mut(n).prev = prev;
        } else {
            self.tail = prev;
        }
        self.slot_mut(idx).prev = None;
        self.slot_mut(idx).next = None;
    }

    /// 插到表头。
    fn push_front(&mut self, idx: usize) {
        self.slot_mut(idx).prev = None;
        self.slot_mut(idx).next = self.head;
        if let Some(h) = self.head {
            self.slot_mut(h).prev = Some(idx);
        }
        self.head = Some(idx);
        // 空链表插入时，表尾也是它
        if self.tail.is_none() {
            self.tail = Some(idx);
        }
    }

    /// 命中后提到表头（已在表头则免修——热 key 的快路径）。
    fn move_to_front(&mut self, idx: usize) {
        if self.head == Some(idx) {
            return;
        }
        self.unlink(idx);
        self.push_front(idx);
    }

    /// 淘汰表尾：摘链、腾槽、删 map，把被逐出的对还给调用方。
    fn evict_tail(&mut self) -> Option<(K, V)> {
        let tail = self.tail?;
        self.unlink(tail);
        let slot = self.slot_at(tail).take()?;
        self.free.push(tail);
        self.map.remove(slot.key_ref());
        Some(slot.into_pair())
    }

    // ── 内部：slab 访问三弟兄（Option<Slot> 包着的下标访问）──

    fn slot_at(&mut self, idx: usize) -> &mut Option<Slot<K, V>> {
        // 槽位由链表与 free 管理保证一致，越界只可能来自内部 bug
        &mut self.slots[idx]
    }

    fn slot_ref(&self, idx: usize) -> &Slot<K, V> {
        self.slots[idx].as_ref().expect("占用的槽位不该是 None（内部一致性被破坏）")
    }

    fn slot_mut(&mut self, idx: usize) -> &mut Slot<K, V> {
        self.slots[idx].as_mut().expect("占用的槽位不该是 None（内部一致性被破坏）")
    }
}

impl<K, V> Slot<K, V> {
    fn key_ref(&self) -> &K {
        &self.key
    }

    fn value_ref(&self) -> &V {
        &self.value
    }

    fn value_mut(&mut self) -> &mut V {
        &mut self.value
    }

    fn into_pair(self) -> (K, V) {
        (self.key, self.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> String {
        s.to_string()
    }

    /// 基本回路：插入 → 命中刷新 → 淘汰最久未用（doc 示例的展开版）。
    #[test]
    fn evicts_least_recently_used() {
        let mut cache = LruCache::new(2);
        cache.put(k("a"), 1);
        cache.put(k("b"), 2);
        // 访问 a：a 成为最近使用，b 变成最久未用
        assert_eq!(cache.get(&k("a")), Some(&1));
        cache.put(k("c"), 3);
        assert_eq!(cache.get(&k("b")), None, "被淘汰的必须是最久未用的 b");
        assert_eq!(cache.get(&k("a")), Some(&1));
        assert_eq!(cache.get(&k("c")), Some(&3));
    }

    /// 更新已有 key：值替换、时序刷新、不淘汰、不增长。
    #[test]
    fn update_refreshes_without_eviction() {
        let mut cache = LruCache::new(2);
        cache.put(k("a"), 1);
        cache.put(k("b"), 2);
        assert_eq!(cache.put(k("a"), 10), None, "更新不该触发淘汰");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&k("a")), Some(&10), "值必须已更新");
        // a 刚被刷新，再塞一个淘汰的应是 b
        cache.put(k("c"), 3);
        assert_eq!(cache.peek(&k("b")), None);
        assert_eq!(cache.peek(&k("a")), Some(&10));
    }

    /// get 刷新时序、peek 不刷新——两者语义必须分开。
    #[test]
    fn peek_does_not_refresh_recency() {
        let mut cache = LruCache::new(2);
        cache.put(k("a"), 1);
        cache.put(k("b"), 2);
        // peek a：时序不变，a 仍是最久未用
        assert_eq!(cache.peek(&k("a")), Some(&1));
        cache.put(k("c"), 3);
        assert_eq!(cache.peek(&k("a")), None, "peek 不算「使用」，a 该被淘汰");
        assert_eq!(cache.peek(&k("b")), Some(&2));
    }

    /// 淘汰的 (k, v) 原样归还——调用方可以回收资源。
    #[test]
    fn evicted_pair_is_returned() {
        let mut cache = LruCache::new(2);
        cache.put(k("a"), 1);
        cache.put(k("b"), 2);
        let evicted = cache.put(k("c"), 3);
        assert_eq!(evicted, Some((k("a"), 1)), "淘汰表尾 a 并归还");
    }

    /// 槽位复用回路：反复淘汰/插入，slab 不增长，下标链表不串线。
    #[test]
    fn slot_reuse_keeps_links_consistent() {
        let mut cache = LruCache::new(2);
        for i in 0..100 {
            cache.put(format!("key{i}"), i);
        }
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.mru_order(), vec![&k("key99"), &k("key98")]);
        // 100 轮淘汰/插入后，只有两个槽位被占用——free-list 复用在干活
        assert_eq!(cache.slots.len(), 2, "slab 不该随插入次数增长");
        // 全量下标访问一遍，串线（指向已空槽位）会当场 panic
        assert_eq!(cache.peek(&k("key99")), Some(&99));
        assert_eq!(cache.peek(&k("key98")), Some(&98));
    }

    /// remove 走完整的清理路径：链表、map、free 三处都干净。
    #[test]
    fn remove_frees_slot_for_reuse() {
        let mut cache = LruCache::new(3);
        cache.put(k("a"), 1);
        cache.put(k("b"), 2);
        cache.put(k("c"), 3);
        assert_eq!(cache.remove(&k("b")), Some(2));
        assert_eq!(cache.remove(&k("b")), None, "重复删除应该落空");
        assert_eq!(cache.mru_order(), vec![&k("c"), &k("a")]);
        // 删中间项后链表必须能穿过 a 直达 c
        assert_eq!(cache.get(&k("a")), Some(&1));
        assert_eq!(cache.get(&k("c")), Some(&3));
        assert_eq!(cache.len(), 2);
    }

    /// 删表头/表尾/最后一个元素——三种边界位置都不该把 head/tail 指空槽。
    #[test]
    fn remove_at_boundaries() {
        let mut cache = LruCache::new(3);
        cache.put(k("a"), 1);
        cache.put(k("b"), 2);
        cache.put(k("c"), 3);
        cache.remove(&k("c")); // 表头
        assert_eq!(cache.mru_order(), vec![&k("b"), &k("a")]);
        cache.remove(&k("a")); // 表尾
        assert_eq!(cache.mru_order(), vec![&k("b")]);
        cache.remove(&k("b")); // 最后一个
        assert!(cache.is_empty());
        assert_eq!(cache.mru_order(), Vec::<&String>::new());
        // 清空后再用：链表从 None 重新开始
        cache.put(k("d"), 4);
        assert_eq!(cache.get(&k("d")), Some(&4));
    }

    /// get_mut 也算「使用」，同样刷新时序。
    #[test]
    fn get_mut_refreshes_recency() {
        let mut cache = LruCache::new(2);
        cache.put(k("a"), vec![1]);
        cache.put(k("b"), vec![2]);
        cache.get_mut(&k("a")).unwrap().push(11); // a 现在是 [1, 11]
        cache.put(k("c"), vec![3]);
        assert_eq!(cache.peek(&k("b")), None, "b 最久未用，被淘汰");
        assert_eq!(cache.peek(&k("a")), Some(&vec![1, 11]));
    }

    /// 容量 0：什么都不存，插入即淘汰（返回自己）。
    #[test]
    fn zero_capacity_evicts_immediately() {
        let mut cache = LruCache::new(0);
        assert_eq!(cache.put(k("a"), 1), Some((k("a"), 1)));
        assert!(cache.is_empty());
    }

    /// clear 后容量不变、可继续使用。
    #[test]
    fn clear_resets_but_keeps_capacity() {
        let mut cache = LruCache::new(2);
        cache.put(k("a"), 1);
        cache.put(k("b"), 2);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.capacity(), 2);
        cache.put(k("c"), 3);
        assert_eq!(cache.get(&k("c")), Some(&3));
    }
}
