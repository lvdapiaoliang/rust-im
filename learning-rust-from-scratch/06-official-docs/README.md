# 06 - Rust 官方文档中文导航（12 部全覆盖）

> 你在这里看到的是 Rust 官方 12 部文档的**中文导航索引**：
> 每部书一个文件夹、一份 README，包含**完整章节目录 + 每章中文导读 + 优先级 + 现成中文翻译链接**。
>
> **为什么不全文翻译？** 这 12 部文档合计数千页面、数百万英文词
> （仅标准库文档就有数万个 API 条目），全文翻译在物理上不可行；
> 且社区已有高质量中文翻译（下表），重复造轮子没有价值。
> 这里的导航帮你解决三个问题：**该读哪部、读到哪章、中文版在哪**。

## 一、12 部文档总览与优先级

> 优先级针对你的目标定制：Java 工程师 → 北京 Rust 岗（IM / 挂载盘 / 跨平台 SDK）。
> ★★★ = 求职必备精读；★★ = 推荐通读；★ = 查阅型，用到再看。

| # | 文档 | 是什么 | 规模 | 优先级 | 中文资源 |
|---|------|--------|------|--------|----------|
| 01 | [The Book](./01-the-book/) | 官方入门书，从零到项目 | 21 章 + 附录 A-G | ★★★ | [trpl-zh-cn（100% 完整）](https://kaisery.github.io/trpl-zh-cn/) |
| 02 | [Nomicon 死灵书](./02-nomicon/) | unsafe Rust 黑魔法 | 13 章（约 40 小节） | ★★（FFI 前必读） | [nomicon.purewhite.io](https://nomicon.purewhite.io/) |
| 03 | [std 标准库](./03-std/) | API 文档（数万条目） | 数万页 | ★★★（天天查） | [rustwiki 标准库中文版](https://rustwiki.org/zh/std/) |
| 04 | [Edition Guide 版本指南](./04-edition-guide/) | 2015/2018/2021/2024 版本差异 | 4 个版本约 40 节 | ★★（2024 版重点） | 无完整中译（短，直接读英文） |
| 05 | [Cargo Book](./05-cargo/) | 构建系统与包管理 | 4 大板块约 40 章 | ★★★（workspace/features） | [rustwiki（翻译中）](https://rustwiki.org/zh/docs/cargo-intro/) |
| 06 | [rustdoc Book](./06-rustdoc/) | 文档生成器 | 12 节 | ★（半天读完） | [rustwiki RustDoc 手册](https://rustwiki.org/zh/docs/rustdoc/) |
| 07 | [rustc Book](./07-rustc/) | 编译器选项与 lint | 30 章 + 平台支持页 | ★★（lint/target 章） | [learnku 中文版](https://learnku.com/docs/rustc-book/2020) |
| 08 | [Error Codes 错误码](./08-error-codes/) | 700+ 编译错误详解 | 700+ 页 | ★★★（天天查） | 本目录含高频错误码中文速查表 |
| 09 | [CLI Book](./09-rust-cli/) | 命令行应用实战 | 教程 + 深入主题 | ★★（im-client 就是 CLI） | [cli-wg-zh（2019 版）](https://github.com/chinanf-boy/cli-wg-zh) |
| 10 | [Embedded Book](./10-embedded/) | 裸金属嵌入式 | 11 章 + 附录 | ★（FFI/静态保证章有用） | 官方列有中文翻译（见原书首页） |
| 11 | [Reference 语言参考](./11-reference/) | 语言规范（语言律师级） | 30+ 章 | ★（查阅型） | [rustwiki 参考手册（100%）](https://rustwiki.org/zh/reference/) |
| 12 | [Unstable Book](./12-unstable/) | nightly 未稳定特性 | 200+ 特性 | ★（io_uring 时再看） | 无中译（查阅型） |

## 二、按学习阶段使用官方文档

```
阶段 A（入门，对应 01-basics/02-core）
  ├─ The Book 1-11 章        ← 配合 learning-rust-from-scratch 双轨对照
  ├─ Error Codes             ← 每次编译报错就查（养成习惯！）
  └─ std 文档                ← 学 Vec/HashMap/String 时查官方说明

阶段 B（异步重点，对应 03-tokio）
  ├─ The Book 第 17 章       ← async/await/futures/streams 官方底座
  ├─ The Book 第 16 章       ← Send/Sync（tokio spawn 的前置知识）
  └─ std 文档                ← std::future/std::task/std::pin

阶段 C（工程化，对应 04-algorithms/05-patterns + rust-im 实战）
  ├─ Cargo Book              ← workspace/manifest/features/build.rs
  ├─ rustc Book 的 Lints 章  ← clippy 背后的机制
  ├─ rustdoc Book            ← doctest 是 rust-im 的测试策略之一
  └─ CLI Book                ← im-client CLI 端的工程实践

阶段 D（进阶/深水区，对应 docs/ 深度文档）
  ├─ Nomicon                 ← unsafe/FFI/Vec 实现（阶段 11 SDK 前必读）
  ├─ Reference               ← 语义争议时的最终仲裁
  ├─ Edition Guide 2024 部分 ← 理解 rust-im 为什么用 edition 2024
  └─ Unstable Book           ← 只在跟踪特定特性（如 io_uring）时查
```

## 三、中文资源站点汇总

| 站点 | 覆盖 | 状态 |
|------|------|------|
| [kaisery.github.io/trpl-zh-cn](https://kaisery.github.io/trpl-zh-cn/) | The Book 全书 | ✅ 100%，即纸质书《Rust 权威指南》 |
| [rustwiki.org](https://rustwiki.org/) / [rustwiki.org.cn](https://www.rustwiki.org.cn/) | Book/std/Reference/Cargo/rustdoc/rustc 等集中地 | ✅ 大部分 100%，Cargo 翻译中 |
| [nomicon.purewhite.io](https://nomicon.purewhite.io/) | Nomicon 全书 | ✅ 持续维护（基于 2021 后版本） |
| [learnku Rust 社区文档](https://learnku.com/docs/rust-lang) | Book/rustc/Nomicon 等 | ✅ 部分版本稍旧 |
| [cli-wg-zh](https://github.com/chinanf-boy/cli-wg-zh) | CLI Book | ⚠️ 基于 2019 版，主线章节仍适用 |

> **版本提醒**：Rust 文档随版本每 6 周更新，中文翻译普遍滞后半年到数年。
> 学习期看中文没问题；**写代码报错/查 API 时以官方英文版为准**
> （`rustup doc` 一键打开本地最新版，无需联网）。

## 四、离线文档：rustup doc

```powershell
rustup doc            # 打开本地全部文档首页（与在线版同步到你的工具链版本）
rustup doc --book     # 直接打开 The Book
rustup doc --std      # 直接打开标准库文档
rustup doc --reference
```

> 【Java】相当于把 Javadoc + 官方 tutorial 装进了本地，
> 飞机/地铁上也能查——这是 Rust 工具链对 Java 工程师最友好的礼物之一。

## 五、版权与使用说明

- 官方文档（Book/Nomicon/std/Cargo/rustc/rustdoc/Edition/Reference/Unstable）
  采用 **MIT OR Apache-2.0** 双许可；Embedded Book 正文采用 **CC-BY-SA 4.0**。
  学习、翻译、引用均合法，注明出处即可。
- 本目录是**导航与导读**，不是原文复制品；深入学习请跳转上表的中英文资源。

---

进入各部文档的导航：

- [01 The Book - Rust 程序设计语言](./01-the-book/)
- [02 Nomicon - 死灵书](./02-nomicon/)
- [03 std - 标准库](./03-std/)
- [04 Edition Guide - 版本指南](./04-edition-guide/)
- [05 Cargo - 构建系统](./05-cargo/)
- [06 rustdoc - 文档工具](./06-rustdoc/)
- [07 rustc - 编译器](./07-rustc/)
- [08 Error Codes - 错误码](./08-error-codes/)
- [09 CLI - 命令行应用](./09-rust-cli/)
- [10 Embedded - 嵌入式](./10-embedded/)
- [11 Reference - 语言参考](./11-reference/)
- [12 Unstable - 未稳定特性](./12-unstable/)

返回 [learning-rust-from-scratch 总目录](../README.md)
