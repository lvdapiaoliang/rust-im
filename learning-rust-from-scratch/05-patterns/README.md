# 05 - Rust 设计模式：GoF 的重生

> 三个前提共识：
> 1. Rust 里**一半的 GoF 模式被语言特性免费送你了**（单例→OnceLock、
>    策略→闭包/trait 对象、命令→闭包、迭代器→原生 Iterator）
> 2. Rust 有自己的**惯用模式**（NEWTYPE、Typestate、RAII、Builder），
>    它们解决的是 GoF 没见过的问题（所有权、编译期状态机）
> 3. 学模式的目的不是套模板，是**品味**：API 面前的每个决策（谁拥有数据？
>    错误怎么传？状态怎么暴露？）都有模式可循

## 总览：GoF 23 模式在 Rust 中的命运

| 命运 | 模式 |
|------|------|
| **语言免费送** | 单例（OnceLock/static）、迭代器、策略（闭包）、命令（闭包）、原型（Clone）、访问者（match） |
| **换个形态活得好** | 生成器（Builder）、装饰器（泛型组合/Deref）、适配器（From/Into）、门面（模块 re-export）、观察者（channel）、状态（enum + match） |
| **要刻意实现** | 责任链、组合（树形结构）、备忘录、桥接 |
| **基本不需要** | 抽象工厂（trait + 泛型足够）、享元（Arc 共享）、中介者（channel/Actor 替代）、代理（Deref/智能指针替代） |

## 目录

| 篇 | 内容 |
|----|------|
| [01-Rust惯用法.md](01-Rust惯用法.md) | NEWTYPE、Typestate、RAII、Builder、错误模型——Rust 原生模式（**本系列核心**） |
| [02-创建型与结构型.md](02-创建型与结构型.md) | Builder 深入、装饰器（泛型组合的零成本抽象）、适配器、门面、组合 |
| [03-行为型.md](03-行为型.md) | 状态机、观察者（channel 形态）、责任链、Actor、空对象（Option）、模板方法（默认 trait 方法） |

## 一个例子预热：同一个需求的三种品味

需求：IM 客户端支持多种压缩（gzip/zstd/无）。

```rust
// Java 思维：策略模式，接口 + 实现类
trait Compressor {
    fn compress(&self, data: &[u8]) -> Vec<u8>;
}
struct Gzip; struct Zstd;
impl Compressor for Gzip { ... }
// 运行时选择：Box<dyn Compressor>
// 编译期选择：泛型 <C: Compressor>

// Rust 惯用：enum + match（数据有限且封闭时更地道！）
enum Compression {
    None,
    Gzip { level: u8 },
    Zstd { level: u8 },
}
impl Compression {
    fn compress(&self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::None => data.to_vec(),
            Self::Gzip { level } => gzip_compress(data, *level),
            Self::Zstd { level } => zstd_compress(data, *level),
        }
    }
}
```

**选择标准**：

- 实现集合**封闭**（自己 crate 内）且需要**携带不同数据** → enum
- 实现**开放**（第三方/插件可实现）或只有**行为差异** → trait + 泛型/dyn
- 需要运行时**热切换** → `Box<dyn Trait>` / `Arc<dyn Trait>`

> 这个「enum vs trait」的分叉是 Rust 设计品味的第一课——
> Java 没有强大的 enum，你从没面对过这个选择；Rust 每天都要选。

## 模式决策的三个永恒问题

每个 API 设计到最后都是这三个问题（比背 23 个模式重要得多）：

1. **谁拥有数据？**（值/引用/Arc/Box——所有权系统逼你回答）
2. **错误如何流动？**（Result + thiserror 分层——错误处理篇的实践）
3. **状态如何暴露？**（enum 使非法状态不可表示——本系列反复出现）

带着三个问题读下去。

开始：[01-Rust惯用法.md](01-Rust惯用法.md)
