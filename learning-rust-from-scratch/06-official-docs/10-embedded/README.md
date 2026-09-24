# 10 - The Embedded Rust Book（嵌入式 Rust）

> **原文**：<https://doc.rust-lang.org/stable/embedded-book/>
> **中文**：官方首页 Translations 节列有中文翻译（社区维护）
> **规模**：11 章 + 术语表；正文 CC-BY-SA 4.0 许可
> **前置**：The Book 全书；假设你有 C/嵌入式背景或愿意补

## 定位（先泼冷水再给理由）

这本书教你**裸金属（no_std、无 OS）嵌入式开发**——
以 ARM Cortex-M 单片机（STM32F3DISCOVERY 开发板）为主线。
你的目标是 IM 服务器 + 跨平台 SDK，**不是** MCU 开发，
所以：**全书不列入必读路径**。

但它有 3 个章节对你的方向有独特价值，值得按需精读。

## 完整章节目录

| 章 | 英文 | 中文 | 导读 | 对你的价值 |
|----|------|------|------|-----------|
| 1 | Introduction | 简介 | no_std 是什么、适用人群、资源地图 | 👀 建立世界观 |
| — | Hardware | 硬件 | MCU/Cortex-M 概念 | 跳过 |
| — | `no_std` | no_std | **std 被拿走后剩下什么**：core/alloc 分层 | ⭐（连接 [std 导航](../03-std/) 的四 crate 知识） |
| — | Tooling | 工具链 | rust-objdump/gdb/OpenOCD | 跳过 |
| — | Installation | 安装 | 各平台环境搭建 | 跳过 |
| 2 | Getting started | 上手 | QEMU 模拟/寄存器/半主机/panic/异常/中断 | 跳过 |
| 3 | Peripherals | 外设 | **用借用检查器管理内存映射寄存器**、Singleton 模式 | ⭐⭐（类型驱动设计的绝佳案例） |
| 4 | **Static Guarantees** | **静态保证** | **Typestate 编程**、外设即状态机、设计契约、零成本抽象 | ⭐⭐⭐（[patterns/01 Typestate](../../05-patterns/01-Rust惯用法.md) 的实战出处！） |
| 5 | Portability | 可移植性 | HAL 抽象层设计 | ⭐ |
| 6 | Concurrency | 并发 | 无 OS 的并发模型 | 👀 |
| 7 | Collections | 集合 | no_std 下的集合（heapless 等） | 👀 |
| 8 | **Design Patterns** | **设计模式** | HAL 的设计模式（Checklist/Naming/互操作/可预测性/GPIO） | ⭐⭐（跨平台抽象的样板） |
| 9 | Tips for embedded C developers | 给 C 工程师的建议 | C↔Rust 心智迁移 | 👀（你是 Java 背景，可跳） |
| 10 | **Interoperability** | **互操作** | A little C with your Rust / A little Rust with your C | ⭐⭐⭐（**FFI 双向实战**，im-sdk 前置） |
| 11 | Unsorted topics | 杂项 | 速度-体积权衡、数学运算 | 🔍 |
| — | Appendix: Glossary | 术语表 | 嵌入式黑话字典 | 🔍 |

## 三个值得精读的章节（与你的岗位强相关）

### 第 4 章 Static Guarantees——Typestate 的圣经

「外设只能在正确状态下使用」由**类型系统**保证：GPIO 配置为输出才允许 `set_high`，
而状态转换发生在类型层面。这是 [patterns/01 的 Typestate 一节](../../05-patterns/01-Rust惯用法.md)
的最佳完整案例——读完它你对「把状态机编进类型」的边界感会清晰很多。

### 第 10 章 Interoperability——FFI 双向桥

- 「A little C with your Rust」：在 Rust 项目里调 C 库
  （im-storage 绑定 WinFsp/FUSE 就是这个方向）
- 「A little Rust with your C」：把 Rust 库编译给 C 调用
  （im-sdk 输出 C ABI 就是这个方向）

比 Nomicon 的 FFI 章更偏实操、更少语言律师内容，适合先读这本再进 Nomicon。

### 第 8 章 Design Patterns——跨平台抽象的样板

HAL（硬件抽象层）的 Checklist/Naming/Interoperability/Predictability 四原则，
本质与 im-sdk 跨五端抽象、im-transport 平台差异化是同一类问题：
**如何设计「多个后端实现一个契约」的 API**。

## 阅读建议

1. 现在只读第 1 章的 no_std 一节（30 分钟），建立 std/core/alloc 的边界感
2. 阶段 11（im-sdk FFI）之前读第 10 章
3. 对 Typestate 感兴趣时（patterns/01 之后）读第 4 章
4. 其余内容除非未来真做 MCU，否则不必碰

返回 [官方文档导航](../README.md) | 前往 [原文](https://doc.rust-lang.org/stable/embedded-book/)
