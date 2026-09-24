# 09 - Command Line Applications in Rust（命令行应用实战）

> **原文**：<https://rust-cli.github.io/book/index.html>
> **中文**：<https://github.com/chinanf-boy/cli-wg-zh>（基于 2019 版，主线内容仍适用）
> **规模**：教程（约 8 节）+ 深入主题（若干篇）
> **前置**：The Book 前 10 章

## 定位

**用 Rust 写 CLI 是这个语言性价比最高的实战入口**——编译快、单二进制分发、
错误处理和类型系统的价值在小工具里立刻兑现。rust-im 的 `im-client`
（阶段 2-5）就是一个标准 CLI 应用；`xtask` 也是。这本书讲的正是
「一个体面的 CLI 该长什么样」。

> 【Java】Java 世界 CLI 是 Picocli 等第三方框架的事；Rust 把它做成了
> 生态级标准流程（clap + 退出码约定 + stdout/stderr 分离）。

## 章节结构与导读

### 一、Getting started（入门）

书的结构与定位：从快速教程到深入主题。

### 二、Tutorial（教程主线）⭐⭐⭐

跟随教程实现一个真实的 CLI 工具（grr，一个 grep 替代品），完整走一遍：

| 英文 | 中文 | 导读 |
|---|---|---|
| Implementing a CLI | 实现一个 CLI | 项目骨架、clap 集成 |
| CLI arguments | 命令行参数 | **clap derive 写法**：参数/子命令/默认值全在 struct 上声明 |
| Working with stdout/stderr | 标准输出与错误输出 | **stdout=数据、stderr=日志**的铁律——`2>/dev/null` 语义的由来 |
| Readable error messages | 可读的错误信息 | anyhow/退出码/`--help` 文案设计 |
| Exit codes | 退出码 | 0 成功/非 0 失败的约定与脚本集成 |
| Testing | 测试 | 如何测 CLI（参数解析单测 + 输出断言） |
| Packaging and distributing | 打包分发 | release profile、跨平台构建、版本号 |

### 三、In-depth topics（深入主题）⭐⭐

各篇相对独立，按需阅读：

| 主题 | 中文 | 与 rust-im 的关联 |
|---|---|---|
| Parsing command-line arguments | 参数解析进阶 | im-client 的用户界面层 |
| Human-readable output | 人类可读输出 | 表格/颜色/进度条 |
| Exit codes revisited | 退出码细则 | 脚本化测试的基石 |
| **Signals** | **信号处理** | Ctrl+C 优雅关闭——**im-server 优雅下线同款问题** |
| Processes and system commands | 进程与系统命令 | 子进程管理（xtask） |
| Configuration files | 配置文件 | TOML/目录约定（XDG） |
| Logging and output | 日志与输出 | tracing/log 生态入门 |

## 对 rust-im 的直接价值

1. **im-client 的骨架教程**：阶段 2 写第一个客户端 CLI 前，
   把 Tutorial 部分过一遍（半天），clap derive + anyhow + 退出码三件套直接复用
2. **信号处理**：CLI Book 的 Signals 篇是 RustRover 里 `Ctrl+C`
   时 `im-server` 优雅关闭（drain 连接、flush 日志）的最小教学版
3. **测试策略**：CLI 的集成测试用「跑二进制 + 断言 stdout/退出码」的模式，
   im-bench 的验收测试将采用同款思路

## 版本提醒

中文版基于 2019 年的 cli-wg 仓库，当时的 structopt 已合并进 clap v3+
（现在直接 `#[derive(Parser)]`）。读中文版时注意 API 名替换，
clap 4 的最新写法以 [docs.rs/clap](https://docs.rs/clap) 为准。

返回 [官方文档导航](../README.md) | 前往 [原文](https://rust-cli.github.io/book/index.html) | [中文版](https://github.com/chinanf-boy/cli-wg-zh)
