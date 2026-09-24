# 06 - The rustdoc Book（文档生成器）

> **原文**：<https://doc.rust-lang.org/rustdoc/index.html>
> **中文**：<https://rustwiki.org/zh/docs/rustdoc/>（RustDoc 手册中文版）
> **本地**：`rustup doc --rustdoc`
> **规模**：12 节，半天可读完——**性价比最高的官方文档**

## 定位

rustdoc 是文档生成器，也是 Rust 生态「文档文化」的根基：
你看到的 docs.rs（crates.io 的文档站）就是 rustdoc 跑出来的。
对 rust-im 而言它有双重身份：**开源项目的门面**（文档质量=开源项目第一印象）
和**测试运行器**（doctest 是测试策略的一部分）。

## 完整章节目录（12 节）

| 英文 | 中文 | 导读 | 优先级 |
|---|---|---|---|
| What is rustdoc? | 什么是 rustdoc | 基本用法、`cargo doc --open`、`///` vs `//!` | ⭐⭐ |
| Command-line arguments | 命令行参数 | 输出路径/crate 名/extern 配置 | 🔍 |
| **How to read rustdoc output** | **怎么读 rustdoc 产物** | 符号体系（ⓘ 何时稳定/🔬 nightly/⚠️ 已弃用）、trait 展开面板 | ⭐⭐（配合 std 文档食用） |
| — In-doc settings | 文档内设置 | 主题、扩展 trait 显示 | 👀 |
| — Search | 搜索 | 搜索语法（`Vec::push`、fuzzy、`struct:` 前缀） | 👀 |
| **How to write documentation** | **怎么写文档** | 文档注释的结构与范例（Panics/Errors/Safety 段落约定） | ⭐⭐⭐ |
| — What to include | 写什么不写什么 | 好文档 = 解释「为什么」+ 完整可运行示例 | ⭐⭐⭐ |
| — The `#[doc]` attribute | doc 属性 | `#[doc(hidden)]`/`#[doc(alias)]`/内联文档 | ⭐ |
| — Re-exports | 重导出 | `pub use` 与门面模式（patterns/02 的工具化） | ⭐ |
| — Linking to items | 条目链接 | `[`Foo`]` 自动链接——文档内导航 | ⭐ |
| — **Documentation tests** | **文档测试** | **doctest**：示例代码即测试，`cargo test` 自动运行 | ⭐⭐⭐ |
| Rustdoc-specific lints | rustdoc 专用 lint | broken_intra_doc_links 等质量控制 | ⭐ |
| Scraped examples | 抓取示例 | 从示例代码反向聚合到 API 文档 | 👀 |
| Advanced features / Unstable features | 高级/不稳定特性 | --cfg 文档、跨 crate 链接 | 🔍 |
| Deprecated features | 已弃用特性 | 避坑清单 | 🔍 |

## 三个对 rust-im 直接有用的点

### 1. doctest 是免费测试（Documentation tests 节）

```rust
/// 把字节流转成 UTF-8 字符串。
///
/// # Examples
///
/// ```
/// let s = im_protocol::decode_utf8(&[104, 105]);
/// assert_eq!(s.unwrap(), "hi");
/// ```
pub fn decode_utf8(bytes: &[u8]) -> Result<String, Utf8Error> { ... }
```

`cargo test` 会把每个 ``` 代码块编译运行。rust-im 的 im-protocol/im-sdk
的公开 API 文档将全部用 doctest 写——**文档即测试，过期即报错**，
这是 Java（Javadoc 示例全靠自觉）没有的保障。

### 2. 文档段落约定（How to write documentation 节）

- `# Errors` —— 返回 Result 的函数必须写（clippy 的 `missing_errors_doc` 检查，
  rust-im 已启用 pedantic lint）
- `# Panics` —— 可能 panic 的条件
- `# Safety` —— unsafe 函数的安全契约（im-sdk 的每一条 unsafe 都要写）
- `# Examples` —— doctest

### 3. 门面与 re-export（Re-exports 节）

[patterns/02 门面模式](../../05-patterns/02-创建型与结构型.md)的文档侧实现：
`pub use` 让调用方 `use im_protocol::prelude::*` 一行拿到所有常用类型。

## 学习建议

1. 现在就花两小时通读「How to write documentation」+「Documentation tests」两节
2. 从 rust-im 第一个公开 API 开始就写文档注释——习惯比补写便宜一百倍
3. 给 rust-im 加一个 xtask 命令：`cargo xtask docs` = `cargo doc --open --workspace`

返回 [官方文档导航](../README.md) | 前往 [中文版](https://rustwiki.org/zh/docs/rustdoc/)
