# learning-rust-from-scratch：从零开始学 Rust

> 面向 Java 工程师的完整 Rust 学习体系。与项目根目录 `docs/` 的分工：
> **这里学语言本身**（语法、Tokio、算法、设计模式），
> **`docs/` 看实战应用**（IM 项目的真实代码走读）。两者互相引用，配合食用。

## 目录结构与学习路径

```
learning-rust-from-scratch/
├── README.md              ← 你在这里（总目录 + 学习路径）
│
├── 01-basics/             第 1 周：从零开始的语法
│   ├── 01-环境与第一个程序.md
│   ├── 02-变量类型函数.md
│   ├── 03-控制流与模式匹配.md
│   ├── 04-struct-enum-trait.md
│   └── 05-错误处理.md
│
├── 02-core/               第 2 周：Rust 的灵魂
│   ├── 06-所有权借用生命周期.md     ← 最重要的一篇
│   ├── 07-泛型trait与trait对象.md
│   ├── 08-集合迭代器闭包.md
│   ├── 09-智能指针.md
│   └── 10-模块系统与工程实践.md
│
├── 03-tokio/              第 3~4 周：异步编程（本项目重点，篇幅最大）
│   ├── 01-异步编程入门.md
│   ├── 02-Future与Waker原理.md     ← 面试核心
│   ├── 03-Tokio运行时与调度器.md   ← 面试核心
│   ├── 04-任务管理与Channel.md
│   ├── 05-异步IO超时select.md
│   ├── 06-同步原语与共享状态.md
│   └── 07-常见陷阱与最佳实践.md
│
├── 04-algorithms/         第 5~6 周：算法与数据结构（Rust 版）
│   ├── README.md（学习路线 + LeetCode 实践法）
│   ├── 01-复杂度分析与Vec.md
│   ├── 02-链表栈队列与环形缓冲.md
│   ├── 03-哈希表原理与手写.md
│   ├── 04-树与堆.md
│   ├── 05-图与搜索.md
│   └── 06-排序与二分.md
│
├── 05-patterns/           第 7 周：Rust 设计模式
│   ├── README.md（GoF 在 Rust 中的形态总览）
│   ├── 01-Rust惯用法-NEWTYPE等.md
│   ├── 02-创建型与结构型模式.md
│   └── 03-行为型模式.md
│
└── 06-official-docs/      随时查阅：官方文档中文导航（12 部全覆盖）
    ├── README.md（12 部文档总览 + 按学习阶段的使用指南）
    ├── 01-the-book/      官方入门书（21 章全目录导读）
    ├── 02-nomicon/       死灵书：unsafe 黑魔法（FFI 前必读）
    ├── 03-std/           标准库模块地图与查询心法
    ├── 04-edition-guide/ 版本指南（2024 版重点）
    ├── 05-cargo/         构建系统（workspace/features）
    ├── 06-rustdoc/       文档工具（doctest）
    ├── 07-rustc/         编译器（lint/交叉编译/调优）
    ├── 08-error-codes/   高频编译错误码中文速查表
    ├── 09-rust-cli/      命令行应用实战
    ├── 10-embedded/      嵌入式（仅 FFI/Typestate 章值得读）
    ├── 11-reference/     语言规范（查询型，中文版完整）
    └── 12-unstable/      未稳定特性（io_uring 时再查）
```

## 学习路径建议

```
第 1 周   01-basics 全部          目标：能写小程序，见怪不怪
第 2 周   02-core 全部            目标：所有权内化，编译器报错能读懂 80%
第 3 周   03-tokio 01~03          目标：讲清 Future/Waker/调度器（面试题）
第 4 周   03-tokio 04~07          目标：独立写异步网络程序
第 5 周   04-algorithms 01~03     目标：手写哈希表、环形缓冲
第 6 周   04-algorithms 04~06     目标：手写堆、BFS/DFS、快排/归并
第 7 周   05-patterns 全部        目标：API 设计有自己的品味
贯穿全程 06-official-docs       随手查：报错→08，API→03，章节→01
之后      回到 ../docs/ 实战      知识在 rust-im 项目里全部落地
```

> **关于 06-official-docs**：Rust 官方 12 部文档合计数百万英文词，
> 全文翻译不可行也无必要——那里提供的是**导航索引**：
> 每部书的完整章节中文导读 + 优先级 + 现成社区中文翻译链接（trpl-zh-cn、
> rustwiki、nomicon 中文版等），详见[官方文档导航](./06-official-docs/README.md)。

## 每篇的统一结构

1. **本章目标**：读完能做什么
2. **概念讲解**：Java 对照（你已有的知识是最好的脚手架）
3. **代码示例**：全部可运行，推荐建一个 `playground/` 目录边读边敲
4. **练习**：小题若干，动手才学得会
5. **自测**：能回答的三个问题

## 约定

- 所有示例基于 Rust stable、edition 2024
- 代码注释里的【Java】标记表示与 Java 的对照点
- 与 rust-im 项目相关的知识会标注 `→ 实战：docs/xx`，方便跳转
