# 07 - 泛型、trait 进阶与 trait 对象

## 本章目标

掌握泛型函数/结构体/impl、trait 约束语法（`where`、`impl Trait`）、
关联类型、trait 对象与动态分发的完整细节。

## 一、泛型基础

```rust
// 泛型函数
fn largest<T: PartialOrd>(list: &[T]) -> &T {
    let mut max = &list[0];
    for item in list {
        if item > max { max = item; }
    }
    max
}

// 泛型结构体
struct Pair<T> { first: T, second: T }

impl<T> Pair<T> {
    fn new(first: T, second: T) -> Self { Self { first, second } }
}

// 条件实现：只有 T 可比较时才有 cmp 方法（Java 做不到的能力！）
impl<T: PartialOrd> Pair<T> {
    fn bigger(&self) -> &T {
        if self.first > self.second { &self.first } else { &self.second }
    }
}
```

> 【Java】语法像，本质不同：**Java 泛型是擦除（erasure），
> Rust 泛型是单态化（monomorphization）**——编译期为每个具体类型生成独立代码。
> 好处：性能（可内联、无装箱）、能基于类型能力条件实现方法；
> 代价：编译慢、二进制变大。Rust 的 `Vec<i32>` 是真实连续内存，
> Java 的 `ArrayList<Integer>` 是引用数组（装箱）。

## 二、约束语法全览

```rust
// 多约束用 +
fn copy_all<T: Clone + Send>(src: &[T]) -> Vec<T> { src.to_vec() }

// 约束多时用 where（可读性）
fn process<T, U>(t: T, u: U) -> String
where
    T: Clone + Into<String>,
    U: Iterator<Item = u8>,
{
    format!("{} bytes", u.count()) + &t.clone().into()
}

// impl Trait 参数：匿名泛型的语法糖（最常用！）
fn print_all(items: impl IntoIterator<Item = impl std::fmt::Debug>) { ... }
// 等价于 fn print_all<I: IntoIterator>(items: I) where I::Item: Debug

// impl Trait 返回值：返回「某个实现了该 trait 的具体类型」
fn make_iter() -> impl Iterator<Item = u32> { (0..10).map(|x| x * 2) }
// 注意：不是 trait 对象！编译期已知具体类型，只是隐藏了名字
```

> 【Java】`impl Trait` 返回值 ≈ 「返回 var 类型的接口视图」，
> 零开销；`Box<dyn Trait>` 才对应 Java 的接口引用。

## 三、关联类型

```rust
trait Container {
    type Item;                       // 关联类型：实现时才确定
    fn get(&self, idx: usize) -> Option<&Self::Item>;
    fn len(&self) -> usize;
}

struct Stack<T> { items: Vec<T> }

impl<T> Container for Stack<T> {
    type Item = T;                   // 每个实现只确定一次
    fn get(&self, idx: usize) -> Option<&T> { self.items.get(idx) }
    fn len(&self) -> usize { self.items.len() }
}

// 使用时用 :: 语法指明关联类型
fn total_len<C: Container<Item = u8>>(c: &C) -> usize { c.len() }
```

关联类型 vs 泛型参数的判断：**类型一旦实现 trait 就固定（用关联类型，
如 Iterator::Item）；同一类型可以多种方式实现（用泛型参数，如 From<T>）**。

> 【实战】`Iterator` trait 的 `type Item` 是最经典案例——
> 你在第 8 篇实现迭代器时写的就是它。

## 四、标准库的万能 trait

```rust
// Iterator：为自定义类型实现迭代器（第 8 篇详解）
// IntoIterator：让 for 循环能消费它
// From/Into：类型转换
// TryFrom：可能失败的转换
// AsRef<T>：廉价引用转换（API 接收 &str 的同时兼容 &String 的秘密）
fn open(path: impl AsRef<std::path::Path>) { ... }   // 接受 &str、String、PathBuf...
// Deref：自动解引用转换（&String → &str 的魔法，智能指针篇详解）
// Drop：自定义析构（RAII 的钩子）
impl Drop for Conn {
    fn drop(&mut self) { /* 关闭资源 */ }
}
// Display/Debug、Default、Hash/PartialEq（derive 一族）
```

## 五、trait 对象与动态分发深入

```rust
// dyn Trait 是「类型擦除的胖指针」= (数据指针, 虚表指针)
let shapes: Vec<Box<dyn Area>> = vec![
    Box::new(Circle { r: 1.0 }),
    Box::new(Rect { w: 2.0, h: 3.0 }),
];
for s in &shapes { println!("{}", s.area()); }   // 虚表分发
```

**对象安全（dyn compatibility）**：不是所有 trait 都能 `dyn`。
规则（新版 Rust 的说法）：

- 方法不能返回 `Self`（编译器不知道 Self 具体是谁，无法造出来）
- 方法不能有泛型参数（虚表是每个 trait 一张，泛型方法无限个版本没法放）
- 不要求 `Self: Sized`

```rust
trait Clone2 { fn clone(&self) -> Self; }      // 返回 Self → 不能 dyn
trait Draw { fn draw(&self); }                  // ✅ 可 dyn
```

> 【Java】Java 接口天然全部「对象安全」，因为 JVM 一切皆虚调用。
> Rust 的 dyn 是可选性能权衡，所以有这道门。

### 静态 vs 动态选择清单

| 用静态分发（泛型） | 用动态分发（dyn） |
|---|---|
| 性能敏感热路径 | 异构集合（`Vec<Box<dyn T>>`） |
| 编译期类型已知 | 插件/运行时替换（`Box<dyn Strategy>`） |
| 想要条件方法 | FFI 回调、跨 ABI 边界 |
| 二进制大小可接受 | 泛型组合爆炸时收敛编译时间 |

> 【实战】rust-im：解码器用泛型（热路径零开销）；弱网策略 `Box<dyn LossModel>`
> （运行时按参数选择）；SDK 对外事件回调 `Box<dyn EventCallback>`（C ABI 要求）。

## 六、泛型常量参数（了解）

```rust
struct RingBuffer<T, const N: usize> {      // 编译期定长的环形缓冲！
    buf: [Option<T>; N],
    head: usize,
}
// 【实战】算法篇 02 手写环形缓冲会用到这个特性
```

## 练习

1. 为 `struct MinHeap<T: Ord>` 写 `push`/`pop`（不许用 Vec 内置 sort）。
2. 定义 trait `Summable { type Output; fn sum(self, other: Self) -> Self::Output; }`，
   为 `i32` 和 `String` 实现，体会关联类型的确定时机。
3. 写一个函数同时接受 `&[i32]` 和 `Vec<i32>` 和 `&Vec<i32>`（提示：`AsRef<[i32]>` 或 `Deref`）。

## 自测

1. 单态化和类型擦除对性能与二进制的影响？
2. 关联类型与泛型参数如何选择？
3. trait 对象为什么有对象安全限制？哪两条规则？

下一篇：[08-集合迭代器闭包.md](08-集合迭代器闭包.md)
