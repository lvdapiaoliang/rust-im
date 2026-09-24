# 11 - The Rust Reference（语言参考手册）

> **原文**：<https://doc.rust-lang.org/reference/index.html>
> **中文**：<https://rustwiki.org/zh/reference/>（100% 完整）
> **本地**：`rustup doc --reference`
> **规模**：30+ 章，语言律师级规范
> **状态**：官方注明「尚未成为正式规范（non-normative）」，但比 Nomicon 维护更好

## 定位

The Reference 是 Rust 语言的**语法与语义字典**：
The Book 教你「怎么用」，Reference 定义「它到底是什么」。
官方自己说的使用方式：**永远不要通读**——带着具体问题来查
（比如「临时值在 let 语句里什么时候 drop」「default 泛型参数怎么解析」）。

> 【Java】角色 ≈ JLS（Java Language Specification）。
> 读 JLS 的人分两种：被逼的（OCJP 认证）和语言律师。
> Reference 同理——但面试时能引用 Reference 条款是资深信号。

## 主要章节地图（按查询主题分组）

### 词法与语法基础

| 章 | 主题 | 你什么时候会查 |
|---|---|---|
| Notation | 记法 | 看懂本书的文法记号（EBNF 变体） |
| Lexical Structure | 词法结构 | 标识符/字面量/原始字符串 `r"..."` 的合法形式 |
| Macros | 宏（声明式） | macro_rules! 的匹配语法（pattern 篇会用到） |

### 程序组织

| 章 | 主题 | 你什么时候会查 |
|---|---|---|
| Crates and Source Files | crate 与源文件 | mod 声明与文件系统的对应规则 |
| Conditional Compilation | 条件编译 | `#[cfg]` 的全部语法（im-sdk 跨平台） |
| Items | 条目 | fn/struct/enum/trait/impl/**所有声明形式**的精确定义——查「还能这么写？」的唯一去处 |
| Visibility and Privacy | 可见性 | pub/pub(crate)/pub(in path) 的精确语义 |

### 类型系统（求职重点区）

| 章 | 主题 | 你什么时候会查 |
|---|---|---|
| Type System / Types | 类型系统/类型 | DST/`!`/inference 的规则 |
| **Generic Parameters** | **泛型参数** | const 泛型、默认参数（algorithms 篇 `RingBuffer<const N: usize>` 的依据） |
| **Associated Items** | **关联条目** | 关联类型 vs 泛型参数怎么选（core/07） |
| Traits | trait | 覆盖规则、标记 trait、安全性 |
| **Type Coercion / Casts** | **强制转换** | `as` 的精确规则、隐式转换何时发生 |
| Destructors | 析构 | drop 顺序的精确规则（面试深水题） |
| Lifetime Elision | 生命周期省略 | **三大规则 + 例外**（core/06 的权威出处） |

### 行为语义

| 章 | 主题 | 你什么时候会查 |
|---|---|---|
| **Expressions** | **表达式** | 临时值生命周期、求值顺序——E0716 的权威解释 |
| Statements / Patterns | 语句/模式 | 模式语法的完整定义（basics/03 的完全体） |
| Attributes | 属性 | 所有内建属性的清单 |
| **Memory Model / Linkage / ABI** | 内存模型/链接/ABI | FFI 与 unsafe 的语义边界 |
| Inline Assembly | 内联汇编 | 罕见 |

## 三个高频查询场景（示例）

1. **「这个报错为什么？」**
   生命周期错误 → 查 Lifetime Elision 章；drop 相关 → Expressions 章的
   temporary lifetime 规则（带编号的规则 ID 可直接链接引用）
2. **「这个语法是什么？」**
   冷门语法（`impl Trait` 位置、`where` 子句形式、属性参数）→ Items 章
3. **「这两个写法有区别吗？」**
   `as` vs `From`、`&String` vs `&str` 参数 → Type Coercion 章

## 与 Nomicon 的分工（官方明确说过）

> The Reference 定义**每个部件**的语法语义；Nomicon 讲**部件组合**时的坑。
> 两者冲突时以 Reference 为准（维护更好）。

| 问题 | 去处 |
|---|---|
| 「引用的语法和省略规则是什么」 | Reference |
| 「引用 + 析构器组合会有什么问题」 | Nomicon |
| 「Send/Sync 的定义」 | Reference |
| 「为什么裸指针是 !Send」 | Nomicon |

## 阅读建议

1. 中文版（rustwiki）完整，查询用中文效率高；规则 ID 编号以英文版为准
2. 每条规则旁有 `[rule.label]` 标识和测试链接——**点开测试看行为**
   比读规范文字快
3. 面试前值得精读的仅两节：Lifetime Elision、Destructors——
   这两节是「三年经验 vs 资深」的分水岭题型

返回 [官方文档导航](../README.md) | 前往 [中文版](https://rustwiki.org/zh/reference/)
