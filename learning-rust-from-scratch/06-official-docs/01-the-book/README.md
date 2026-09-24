# 01 - The Rust Programming Language（官方入门书）

> **原文**：<https://doc.rust-lang.org/book/title-page.html>
> **中文**：<https://kaisery.github.io/trpl-zh-cn/>（100% 完整，即《Rust 权威指南》）
> **本地**：`rustup doc --book`（英文最新版，离线可读）
> **规模**：21 章 + 附录 A-G，配 Rust 1.90 / edition 2024

## 定位与读法

The Book 是官方钦定的入门路径，但**不是教材式的语法罗列**——它用两个实战项目（猜数字、minigrep）和一个毕业项目（多线程 Web Server）把语言串起来。

**对你（Java 工程师）的特殊读法**：你已经会编程，01-basics/02-core 系列是你的主线，
The Book 作为**官方视角的补充与勘误源**，重点读与学习系列交叉印证的章节。
下表「对照」列标注了与 learning-rust-from-scratch 的对应关系——两边都讲过的，
说明是核心中的核心。

## 完整章节目录（21 章 + 附录）

> 优先级：⭐ 精读 | 👀 快速过（已有 Java/学习系列基础）| 🔍 查阅

| 章 | 英文原名 | 中文 | 导读 | 优先级 | 对照 |
|----|----------|------|------|--------|------|
| 1 | Getting Started | 入门指南 | 安装 rustup、hello world、Cargo 三件套 | 👀 | basics/01 |
| 2 | Programming a Guessing Game | 猜数字游戏 | 第一个完整程序：输入/随机数/依赖/match | ⭐ | basics/01-03 |
| 3 | Common Programming Concepts | 通用编程概念 | 变量可变性、标量/复合类型、函数、控制流 | 👀 | basics/02-03 |
| 4 | Understanding Ownership | 理解所有权 | **全书最重要的一章**：所有权/移动/借用/切片 | ⭐⭐⭐ | core/06 |
| 5 | Using Structs | 使用结构体组织数据 | 结构体、方法、关联函数 | 👀 | basics/04 |
| 6 | Enums and Pattern Matching | 枚举与模式匹配 | enum/Option/match 穷尽性/`if let`/`let-else` | ⭐ | basics/03-04 |
| 7 | Packages, Crates, and Modules | 包、crate 与模块 | 模块树/可见性/use 路径——Rust 的「包管理+访问控制」 | ⭐ | core/10 |
| 8 | Common Collections | 常用集合 | Vec/String/HashMap 的底层行为与坑 | ⭐ | core/08、algorithms/01,03 |
| 9 | Error Handling | 错误处理 | `panic!` vs `Result` vs `?`，错误传播哲学 | ⭐ | basics/05 |
| 10 | Generic Types, Traits, and Lifetimes | 泛型、trait 与生命周期 | 单态化、trait 约束、生命周期三大省略规则 | ⭐⭐⭐ | core/06-07 |
| 11 | Writing Automated Tests | 编写自动化测试 | 单元/集成测试、`#[should_panic]`、测试组织 | ⭐ | core/10 |
| 12 | An I/O Project: Building a Command Line Program | I/O 项目：命令行程序 | minigrep 项目：TDD + 模块化 + 错误处理综合实战 | ⭐ | 全系列综合 |
| 13 | Functional Language Features | 迭代器与闭包 | 闭包捕获、迭代器适配器、零成本抽象证明 | ⭐⭐ | core/08 |
| 14 | More about Cargo and Crates.io | 深入 Cargo | release profile、发布流程、**workspace**、自定义命令 | ⭐ | core/10、Cargo Book |
| 15 | Smart Pointers | 智能指针 | Box/Deref/Drop/**Rc/RefCell**/引用循环泄漏 | ⭐⭐⭐ | core/09 |
| 16 | Fearless Concurrency | 无畏并发 | 线程、**消息传递、Mutex/`Arc`、Send/Sync** | ⭐⭐⭐ | core/09、docs/02 |
| 17 | Fundamentals of Asynchronous Programming | 异步编程基础 | **新增章**：Future/async-await/并发/streams/async trait | ⭐⭐⭐ | tokio 全系列 |
| 18 | Object-Oriented Programming Features | 面向对象特性 | trait 对象 vs 泛型、Rust 不是 OOP 但能做 OOP 设计 | ⭐ | core/07、patterns/02 |
| 19 | Patterns and Matching | 模式与匹配 | 模式可出现的所有位置、refutability、模式语法大全 | ⭐ | basics/03 |
| 20 | Advanced Features | 高级特性 | **unsafe 入门**、高级 trait（关联类型/HRTB）、宏 | ⭐ | Nomicon 入口 |
| 21 | Final Project: A Web Server | 毕业项目：Web 服务器 | 单线程→线程池→优雅关闭，全书知识总动员 | ⭐ | rust-im echo 服务器进阶版 |
| A | Keywords | 关键字 | 保留字与关键字清单 | 🔍 | — |
| B | Operators and Symbols | 运算符与符号 | 每个符号（`?` `&` `*` `..`）的语义表 | 🔍 | — |
| C | Derivable Traits | 可派生 trait | `#[derive]` 都帮你实现了什么 | 🔍 | — |
| D | Useful Development Tools | 实用开发工具 | rustfmt/clippy/cargo-watch 等 | 👀 | — |
| E | Editions | 版本 | edition 机制概述（详见 Edition Guide） | 🔍 | edition-guide |
| F | Translations | 本书翻译 | 社区翻译列表 | — | — |
| G | How Rust is Made and Nightly Rust | Rust 的构建与 nightly | 6 周发布火车、rustc 自举 | 👀 | — |

## 三个必精读章节的理由

### 第 4 章 + 第 10 章：所有权与生命周期
Java 工程师转 Rust 的**最大断层**就是这里。GC 世界里「对象引用随便传」的直觉
在 Rust 里全部失效。这两章 + [core/06](../../02-core/06-所有权借用生命周期.md)
形成三重视角：官方叙事（The Book）→ 报错驱动（学习系列的 E0382 速查表）→
深水区（Nomicon 的 variance）。

### 第 17 章：异步编程基础（2024 新增）
官方终于把 async/await 写进 The Book——这一章是 tokio 学习系列的**官方底座**：
Future trait 的 `poll`/`Waker` 模型、`async fn` 状态机、join/select、Stream。
建议顺序：The Book 17 章（概念）→ [tokio/02](../../03-tokio/02-Future与Waker原理.md)
（手写 Future）→ tokio/03（调度器内部）。

### 第 16 章：Send/Sync
`tokio::spawn` 要求 `'static + Send` 的原因全部埋在第 16 章。
配合 [docs/02-send-sync-pin.md](../../../docs/02-send-sync-pin.md) 食用，
这是北京 Rust 面试的一级高频题。

## 与 rust-im 的关联

- 第 21 章的 Web Server 就是 `im-transport` echo server 的完整版形态
  （TCP accept 循环 → worker 池 → 优雅关闭），rust-im 阶段 2 会重写它的 Tokio 版
- 第 14 章 workspace 是 rust-im 九 crate 结构的理论基础
- 第 12 章 minigrep 的 TDD 流程是 rust-im 全程的开发方法论

## 学习建议

1. **不要从第 1 页顺读到尾**——你的主线是 learning-rust-from-scratch，
   The Book 按「对照」列跳读交叉验证
2. 每章的练习（页面里的 mini-quiz）值得做；Brown 大学还有交互增强版：
   <https://rust-book.cs.brown.edu>（带测验和可视化）
3. 中文版看 kaisery 译本，术语与《Rust 权威指南》纸质书一致

返回 [官方文档导航](../README.md) | 前往 [中文版在线阅读](https://kaisery.github.io/trpl-zh-cn/)
