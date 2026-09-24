# 04 - The Rust Edition Guide（版本指南）

> **原文**：<https://doc.rust-lang.org/edition-guide/index.html>
> **中文**：无完整中译（本书较短且以查询为主，直接读英文无碍）
> **本地**：`rustup doc --edition-guide`
> **规模**：4 个版本板块 + 迁移指南，约 40 小节
> **前置**：The Book 附录 E（edition 概述）

## 一、Edition 是什么（30 秒版）

> 【Java】Edition ≈ Java 的「语言大版本」（Java 8 → 11 → 17），
> 但有一个 Java 没有的超级能力：**新旧 edition 的代码可以链接在一起**。
> 升级 edition 永远不需要「大爆炸迁移」。

- Edition（2015/2018/2021/2024）是**源码层面的兼容性档位**，不是编译器版本
- 每个 crate 在 `Cargo.toml` 里声明自己的 edition，互不影响
- **rust-im 使用 edition 2024**（workspace 根已配置）
- 升级由工具自动完成：`cargo fix --edition && cargo fix --allow-dirty` → 改 `edition` 字段

## 二、章节结构与导读

### What are editions?（机制篇）⭐

| 英文 | 中文 | 导读 |
|---|---|---|
| What are editions? | 什么是版本 | edition 与编译器版本的正交关系 |
| Creating a new project | 新建项目 | `cargo new` 默认最新 edition |
| Transitioning an existing project | 迁移现有项目 | `cargo fix --edition` 自动迁移流水线 |
| Advanced migrations | 高级迁移 | 复杂项目（多 crate/CI）的分批迁移策略 |

### Rust 2018 板块 👀（历史，快过）

路径与模块系统重构（`use` 语义变化——现在写法的由来）、`async`/`await`/`dyn` 关键字、Cargo 变更。

### Rust 2021 板块 👀（历史，快过）

prelude 增补（IntoIterator for arrays、`TryFrom`）、闭包不相交捕获、panic 宏一致性、保留语法。

### Rust 2024 板块 ⭐⭐⭐（rust-im 所在档位，重点）

| 英文 | 中文 | 导读 | 对你的影响 |
|---|---|---|---|
| RPIT lifetime capture rules | RPIT 生命周期捕获 | 返回 `impl Trait` 的默认捕获规则收紧 | 写 API 时偶发 |
| `if let` temporary scope | if let 临时值作用域 | 临时值提前销毁——微妙的 drop 时机修正 | 理解 drop 报错 |
| **`let` chains** | **let 链** | `if let A && B` 一步写完（等了很多年！） | **日常幸福度++** |
| Tail expression temporary scope | 尾表达式临时作用域 | 块尾表达式 drop 顺序修正 | 罕见 |
| Match ergonomics reservations | 匹配人机工程预留 | 未来变更的预留位 | 暂无 |
| **Unsafe `extern` blocks** | **unsafe extern 块** | extern 必须显式 `unsafe` | **im-sdk FFI 必须遵守** |
| **Unsafe attributes** | **unsafe 属性** | `#[no_mangle]` 等要写 `#[unsafe(no_mangle)]` | **im-sdk FFI 必须遵守** |
| `unsafe_op_in_unsafe_fn` | unsafe fn 中的操作警告 | unsafe fn 里也要显式 unsafe 块 | 好习惯 |
| **Disallow references to `static mut`** | **禁止引用 static mut** | 取 `&mut STATIC` 从错误升级为硬错误 | **共享状态写法必须用新 API** |
| Never type fallback | never 类型回退 | `!` 推导规则变化 | 罕见 |
| `gen` keyword | gen 关键字 | 为 gen blocks 预留 | 知道即可 |
| Standard library changes | 标准库变化 | prelude 增补（Future 族进 prelude！） | async 代码不用再手动 use |
| Cargo changes | Cargo 变化 | **rust-version 感知 resolver**（rust-im workspace 用到）、表格键名一致性 | 配置写法 |
| Rustdoc/Rustfmt changes | 文档与格式化变化 | doctest 合并、style edition | 工具行为 |

## 三、为什么 rust-im 选 edition 2024

1. `let` chains 让协议解析的分支代码干净很多
2. FFI 的 `unsafe extern`/`unsafe attributes` 强制显式化——SDK 的 unsafe 边界更清晰
3. Cargo 的 rust-version-aware resolver 避免「依赖要求比你工具链新」的意外
4. 简历上体现「跟随最新 edition 的工程习惯」——北京团队普遍 2021/2024 混合

## 四、版本速查（面试向）

| 问题 | 答案要点 |
|---|---|
| edition 和 Rust 版本（1.90 等）的关系？ | 正交：编译器 6 周一版可编译所有 edition |
| 一个 workspace 能混用 edition 吗？ | 能，每个 crate 独立声明 |
| 升级 edition 要手动改代码吗？ | `cargo fix --edition` 自动完成 90%+ |
| 2024 最影响日常的变化？ | let chains、unsafe extern/attributes、static mut 引用禁令 |

返回 [官方文档导航](../README.md) | 前往 [原文](https://doc.rust-lang.org/edition-guide/index.html)
