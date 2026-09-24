# 12 - The Unstable Book（未稳定特性）

> **原文**：<https://doc.rust-lang.org/nightly/unstable-book/>
> **中文**：无中译（查阅型文档，天然不需要）
> **规模**：200+ 特性条目（语言特性 + 库特性 + 编译器标志）
> **访问前提**：文档在 nightly 通道；使用需 nightly 工具链 + `#![feature(...)]`

## 定位

Unstable Book 是**实验性功能的登记簿**：每个进入 nightly 但尚未稳定的
特性一个条目，说明用法、动机和跟踪 issue。
它回答的问题是：「我在网上看到那个酷炫写法为什么我的编译器报错？」
——因为那是 nightly 特性。

对 rust-im 的意义集中在一处：**性能里程碑 3（500 万连接）阶段的 io_uring**。
当前 tokio 的 io_uring 支持仍在演进（tokio-uring / monoio 等运行时大量依赖
未稳定特性），届时这本书和相关 RFC 是第一手资料。

## 结构

| 板块 | 内容 | 示例条目 |
|---|---|---|
| Compiler flags | 编译器标志（`-Z`） | `-Z sanitizer`、`-Z build-std` |
| Language features | 语言特性 | `let_chains`（曾长期在此，2024 已稳定）、`async_closure`、`gen_blocks`、`never_type` |
| Library features | 库特性 | `io_error_more`、`async_fn_in_trait`（已稳定）…… |

> 每个条目都是同一个模板：特性名 → `#![feature]` 用法 → 动机 → 跟踪 issue 链接。
> 特性**稳定后会从本书消失**（移入正式文档）——所以这本书的目录本身就是
> 「Rust 正在往哪走」的实时地图。

## 怎么用（四个场景）

### 1. 看到奇怪代码时报错信息会指路

```text
error[E0658]: `let` chains are unstable
  --> src/main.rs:3:12
   |
   = help: add `#![feature(let_chains)]` to the crate attributes...
   = note: see issue #53667 ...
```
报错里的 issue 号 → Unstable Book 对应条目 → 看它处于什么阶段。

### 2. 启用一个 nightly 特性（标准姿势）

```rust
// Cargo.toml 的 rust-toolchain.toml 钉住 nightly 版本（rust-im 不会默认这样做）
#![feature(never_type)]   // lib.rs 顶部声明

fn main() {
    let x: ! = panic!("这个类型终于能写了");
}
```

### 3. 跟踪 rust-im 需要的特性成熟度

| 特性 | 用途 | 关注时机 |
|---|---|---|
| `io_error_more` / io_uring 相关 | 里程碑 3 的内核级 IO | 阶段 10 前 |
| `async_closure`（已稳定/接近稳定） | 回调式异步代码简化 | 已可关注 |
| `specialization` | 泛型特化（性能优化空间） | 长期（稳定遥遥无期） |

### 4. 判断生产可用性（工程判断力）

在 job 的技术讨论里，「这个特性在 Unstable Book 的哪个阶段」
（implementation / feature-complete / waiting on decision）是
判断「能不能引入生产」的硬依据——nightly 特性**绝对不能进生产**，
这条红线也适用于 rust-im（开源项目用 stable，压测分支可开 nightly）。

## 相关资源

- [Rust RFC 仓库](https://github.com/rust-lang/rfcs)：特性的「立法过程」
- [This Week in Rust](https://this-week-in-rust.org/)：每周稳定/新特性动态
- `rustup toolchain install nightly && rustup doc --unstable-book`

## 阅读建议

现在不用读。在遇到 E0658 报错或跟踪 io_uring 进展时回来查即可——
它是字典，不是教材。

返回 [官方文档导航](../README.md) | 前往 [原文](https://doc.rust-lang.org/nightly/unstable-book/)
