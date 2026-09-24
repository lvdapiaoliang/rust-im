# 01 - 复杂度分析与 Vec：从动态数组开始

## 本章目标

复习 Big-O（重点补 Java 工程师常缺的 amortized 分析），手写动态数组，
吃透 Vec 的每一个行为。

## 一、复杂度：工程视角速览

| 复杂度 | 名称 | 直觉 |
|--------|------|------|
| O(1) | 常数 | 哈希表查找、数组随机访问 |
| O(log n) | 对数 | 二分、平衡树——「一半一半」 |
| O(n) | 线性 | 遍历 |
| O(n log n) | 线性对数 | 归并/堆排（比较排序下限） |
| O(n²) | 平方 | 双重循环 |

### Amortized（均摊）：Vec.push 的真实成本

单次 push 可能触发扩容（O(n) 拷贝），但**扩容按 2 倍几何增长**：

```
容量 1→2→4→8→...→n，总拷贝次数 = 1+2+4+...+n < 2n
n 次 push 总成本 O(2n) → 均摊每次 O(1)
```

> 【Java】ArrayList 默认 1.5 倍扩容（省内存），Rust Vec 是「首次 4（小类型）或 1，
> 之后约 2 倍」。工程含义：**能预估容量就 with_capacity(n) 预分配**，
> rust-im 的每连接缓冲区全部预分配——均摊 O(1) 依然不如 0 次扩容。

### 缓存局部性：为什么 O(n) 的 Vec 遍历常比 O(1) 的链表快

```
Vec：连续内存，一次 cache line（64B）装 8 个 i64 → 预取器全力工作
链表：节点散落堆上，每跳一次都可能 cache miss（~100 周期）
```

> 面试加分点：现代 CPU 下「复杂度低」≠「跑得快」，
> 数据布局（cache friendly）常是常数因子的胜负手。
> 这也是 Rust 默认给你连续内存结构（Vec）而非链表的原因。

## 二、手写 MyVec（核心 ~60 行）

```rust
pub struct MyVec<T> {
    ptr: Option<Box<[T]>>,      // 用 Option 处理空 Vec 的空指针
    len: usize,
    // 容量 = ptr 的长度（Box<[T]> 自带长度信息）
}

impl<T: Clone> MyVec<T> {
    pub fn new() -> Self { Self { ptr: None, len: 0 } }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            ptr: Some(vec![T::clone(&self_zero()); cap)].ok().map(|v| v.into_boxed_slice()),
            len: 0,
        }
    }
    // ↑ 上面是简化示意；真实实现用 RawVec + MaybeUninit（见下方说明）

    pub fn len(&self) -> usize { self.len }
    pub fn is_empty(&self) -> bool { self.len == 0 }

    pub fn push(&mut self, value: T) {
        let cap = self.ptr.as_ref().map_or(0, |b| b.len());
        if self.len == cap {
            self.grow(cap * 2.max(1));
        }
        // 真实实现：ptr::write 写入未初始化内存（无 Clone 约束、无 panic 安全洞）
        // 教学版用 clone 占位会引入额外拷贝——理解思想即可
        self.raw_set(self.len, value);
        self.len += 1;
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 { return None; }
        self.len -= 1;
        Some(self.raw_take(self.len))
    }

    fn grow(&mut self, new_cap: usize) {
        let mut old = self.ptr.take().map(|b| b.into_vec()).unwrap_or_default();
        old.resize(new_cap, /* 占位 */);
        // ... 详见下方「真实实现」说明
    }

    fn raw_set(&mut self, idx: usize, v: T) { /* unsafe: 写入 */ }
    fn raw_take(&mut self, idx: usize) -> T { /* unsafe: 读出并置空 */ }
}
```

> **诚实的注解**：教学版到处借力。生产级的 Vec 实现要点是
> **`MaybeUninit<T>` + `ptr::write/read`**——
> Rust 不允许「未初始化的 T」安全存在（无 null T），所以标准库用
> `MaybeUninit<T>` 表示「可能有值」的内存，push 时 `ptr::write`（直接写位，不读旧值），
> pop 时 `ptr::read`（移出所有权）。
> 这 60 行 unsafe 是「为什么 Vec 不能手搓着玩」的原因，
> 也是 unsafe 使用的教科书案例：**unsafe 只出现在边界（裸内存操作），安全接口包住它**。

### 越界与 panic 安全

```rust
impl<T> MyVec<T> {
    pub fn get(&self, i: usize) -> Option<&T> {         // 安全访问
        (i < self.len).then(|| unsafe { self.raw_ref(i) })
    }
    // v[i] 的等价物必须 panic（与标准库一致）：
    // self.get(i).unwrap_or_else(|| panic!("index {i} out of range"))
}
```

**Drain 陷阱**（进阶，知道即可）：标准库的 `drain` 在 panic 时也要保证元素不泄漏不 double-free——
Rust 容器实现里大量代码在处理「panic 发生在中途」的内存安全，这是 Java（有 GC）完全不用操心的一半工作。

## 三、Vec 日常武器库

```rust
let mut v: Vec<u32> = Vec::new();
let v2 = vec![0u8; 4096];              // 预分配定值
let v3 = Vec::with_capacity(1024);     // 预分配空容量

v.push(1);
v.insert(0, 0);                         // O(n)！中间插入要挪
v.remove(0);                            // O(n)
v.swap_remove(0);                       // O(1)：尾部元素补位（顺序变，慎用）
v.pop();

// 切片视图（零拷贝）：
fn sum(s: &[u32]) -> u32 { s.iter().sum() }
sum(&v);                                // &Vec<u32> 自动 deref 成 &[u32]

// 排序：
v.sort();                               // 稳定（归并）
v.sort_unstable();                      // 不稳定（快排变种），更快，无重复元素时优先
v.sort_by(|a, b| b.cmp(a));             // 自定义比较器（降序）
v.sort_unstable_by_key(|x| x.priority);

// 二分（必须已排序！）：
v.binary_search(&5);                    // Result<usize, usize>：找到/应插入位置
v.partition_point(|x| x < &10);         // 第一个不满足谓词的位置（最通用的二分）

// 切分与拼接：
let (a, b) = v.split_at(2);
let merged = [a, b].concat();
v.extend_from_slice(&[1, 2, 3]);
v.dedup();                              // 去除相邻重复
v.chunks(4) / v.windows(2);             // 分块/滑窗（【实战】协议分帧的视图工具）
```

### retain：安全地边遍历边删

```rust
// ❌ Java 习惯翻车：遍历中 remove（借用冲突，编译器拦住了）
// for x in &v { if bad(x) { v.remove(...) } }
// ✅ retain：一次遍历原位压缩
v.retain(|x| !bad(x));
```

## 四、算法实战：原地旋转（对应核心篇 06 的练习 3）

```rust
/// 旋转数组 [1,2,3,4,5,6,7], k=3 → [5,6,7,1,2,3,4]
/// 三次反转法：O(n) 时间 O(1) 空间
pub fn rotate(nums: &mut [i32], k: usize) {
    let n = nums.len();
    if n == 0 { return; }
    let k = k % n;
    nums[..n - k].reverse();        // split_at_mut 是关键：两段可变借用共存
    nums[n - k..].reverse();
    nums.reverse();
}
```

`split_at_mut` 是「一个可变借用拆成两个不重叠的可变借用」的唯一安全途径——
很多原地算法（快排分区、归并）都靠它。

## 练习与题单

1. 实现教学版 MyVec 的 `insert`/`remove`，注意 `len -= 1` 后元素左移的方向。
2. 用 `windows(2)` 实现检查数组是否已排序。
3. LeetCode：27 移除元素（retain 思想）、26 删除有序数组重复项、80 删除重复 II、59 螺旋矩阵。

## 自测

1. Vec push 的均摊 O(1) 推导？什么时候退化？
2. `sort` 和 `sort_unstable` 的区别与选择？
3. 为什么遍历中删元素要 retain？split_at_mut 解决什么问题？

下一篇：[02-链表栈队列与环形缓冲.md](02-链表栈队列与环形缓冲.md)
