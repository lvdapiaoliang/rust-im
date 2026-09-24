# 02 - The Rustonomicon（死灵书：unsafe Rust 黑魔法）

> **原文**：<https://doc.rust-lang.org/nomicon/index.html>
> **中文**：<https://nomicon.purewhite.io/>（持续维护版）；另有 [learnku 2018 版](https://learnku.com/docs/nomicon/2018)
> **本地**：`rustup doc --nomicon`
> **状态**：官方声明「本书未完成」，但已有的章节质量极高
> **前置**：The Book 全书 + [core/06-09](../../02-core/)；不假设你读过 The Book，但假设你懂 Rust

## 定位

unsafe Rust 的官方指南。官方原话：**「如果你想要长久幸福的 Rust 生涯，现在就合上这本书」**——
但你做的是 IM + 挂载盘 + FFI SDK，unsafe 逃不掉：
`im-crypto`（ring/x25519 绑定）、`im-sdk`（C ABI）、`im-storage`（FUSE/WinFsp 绑定）全是 unsafe 密集区。

它回答的问题只有一个：**如何用 unsafe 原语构造出安全的抽象**——
这正是「资深 Rust 工程师」和「会写 Rust 的人」的分界线。

## 完整章节目录（13 章）

| 章 | 英文原名 | 中文 | 导读 | 优先级 |
|----|----------|------|------|--------|
| 1 | Meet Safe and Unsafe | 初识安全与不安全 | safe/unsafe 的真正含义：unsafe 不是「关闭检查」而是「承担义务」 | ⭐⭐⭐ |
| — | How Safe and Unsafe Interact | safe 与 unsafe 如何交互 | soundness（健全性）概念：安全代码依赖 unsafe 代码的契约 | ⭐⭐⭐ |
| — | What Unsafe Can Do | unsafe 能做什么 | 五大超能力：解引用裸指针/调用 unsafe fn/实现 unsafe trait/访问 static mut/内联汇编 | ⭐⭐⭐ |
| — | Working with Unsafe | 与 unsafe 共事 | 最小化 unsafe 块、隔离与注释规范 | ⭐⭐⭐ |
| 2 | Data Layout | 数据布局 | repr(Rust) vs repr(C)、ZST、repr(packed/align)——FFI 前置知识 | ⭐⭐（FFI 必读） |
| 3 | Ownership | 所有权（深入版） | 引用/Aliasing/**Lifetimes 全家桶**：省略规则的例外、无限生命周期、HRTB、Subtyping/Variance、Drop Check、PhantomData、借用拆分 | ⭐⭐⭐ |
| 4 | Type Conversions | 类型转换 | 强制转换(coercion)/点操作符/casts/**transmute**（最危险的原语） | ⭐⭐ |
| 5 | Uninitialized Memory | 未初始化内存 | MaybeUninit 的原理、Drop 标志、UB 的边界 | ⭐⭐ |
| 6 | Ownership Based Resource Management | 基于所有权的资源管理 | 构造器/析构器/Leaking——RAII 的黑暗细节 | ⭐ |
| 7 | Unwinding | 栈展开 | panic 时的 drop 顺序、**异常安全**、Mutex poisoning 的由来 | ⭐⭐ |
| 8 | Concurrency | 并发（深入版） | 数据竞争 vs 竞态、**Send/Sync 的推导规则**、原子操作与内存序 | ⭐⭐⭐ |
| 9 | Implementing Vec | 手写 Vec | 用 11 小节从零实现 Vec：布局/分配/push-pop/Deref/IntoIter/**RawVec**/Drain/ZST | ⭐⭐⭐ |
| 10 | Implementing Arc and Mutex | 手写 Arc | 原子引用计数、布局、clone/drop——Arc 的全部真相 | ⭐⭐ |
| 11 | FFI | 外部函数接口 | 与 C 互操作：#[repr(C)]、extern 块、字符串传递、回调 | ⭐⭐⭐（SDK 阶段必读） |
| 12 | Beneath std | 标准库之下 | #[panic_handler]、no_std 的世界 | ⭐ |
| — | + 各章散落专题 | — | Splitting Borrows 等技巧穿插在 Ownership 章 | — |

## 四个「面试级」知识点的入口

### Variance（型变）——第 3 章 Subtyping and Variance
为什么 `&'static str` 能传给 `&'a str` 参数？为什么 `&mut T` 是 invariant？
这决定了哪些生命周期转换合法——**生命周期报错看不懂时来这里找答案**。

### Drop Check——第 3 章 dropck
为什么有的 struct 加了 `PhantomData<&'a ()>` 才能编译？dropck 检查
「T 被 drop 时借用是否还活着」——泛型容器作者的必修课。

### 手写 Vec——第 9 章
[algorithms/01](../../04-algorithms/01-复杂度分析与Vec.md) 的 `MyVec` 简化版
的完全体。rust-im 阶段 3 的接收缓冲区管理（`BytesMut` 的原理）
本质上就是这一章 + [algorithms/02](../../04-algorithms/02-链表栈队列与环形缓冲.md)
的环形缓冲。

### FFI——第 11 章
`im-sdk` 阶段 6 的直接前置。重点：repr(C) 结构体布局契约、
extern "C" 双向调用、`CString`/`CStr` 的所有权边界、panic 不能跨 FFI 边界
（unwinding 跨边界是 UB——2024 edition 要求显式 `unsafe extern` + abort 声明的由来）。

## 与学习系列的对照

| Nomicon 章节 | 学习系列对应 |
|---|---|
| Ownership/Lifetimes | [core/06](../../02-core/06-所有权借用生命周期.md)（入门）→ Nomicon（深水区） |
| Concurrency/Send/Sync | [core/09](../../02-core/09-智能指针.md) + [docs/02](../../../docs/02-send-sync-pin.md) |
| Implementing Vec | [algorithms/01](../../04-algorithms/01-复杂度分析与Vec.md) |
| 手写 Arc | [core/09](../../02-core/09-智能指针.md) 的 Rc/Arc 小节 |
| FFI | [docs/02](../../../docs/02-send-sync-pin.md) 的 FII 坑 + 本系列未覆盖（读原文） |

## 阅读建议

1. **不要现在通读**——正确的时机是 rust-im 阶段 6（FFI SDK）之前，
   或第 3 次被生命周期报错折磨之后
2. 中文版 purewhite 译本质量好且持续跟进；英文原版 `rustup doc --nomicon` 离线可读
3. 读的时候准备一个 scratch project，每章的代码亲手跑一遍——
   unsafe 的直觉只能靠 UB 现场建立（建议开 `-Zsanitizer=address` 用 nightly 验证）

返回 [官方文档导航](../README.md) | 前往 [中文版](https://nomicon.purewhite.io/)
