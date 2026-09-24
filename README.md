# rust-im

用 Rust 从零构建的开源 IM（即时通讯）全栈项目。对标 Telegram 的功能形态，目标支撑单机百万级长连接的实时消息系统。

## 项目状态

**阶段 0 已完成**：Cargo workspace 骨架 + Tokio echo server/client（首个学习载体）。

| 阶段 | 内容 | 状态 |
|------|------|------|
| 0 | workspace 骨架 + echo 热身 | ✅ |
| 1 | 二进制协议（帧编解码 / 粘包处理） | ⬜ |
| 2 | 传输层（心跳 / 重连 / seq-ACK / TLS） | ⬜ |
| 3 | 服务端（网关 / 路由 / 扇出 / 持久化） | ⬜ |
| 4 | 客户端（TUI / 消息同步） | ⬜ |
| 5 | 压测（10万 → 100万 → 500万连接三级里程碑） | ⬜ |
| 6 | FFI SDK（C ABI 动态库 / JNI） | ⬜ |
| 7 | 桌面端（Tauri）+ E2EE（Signal 协议） | ⬜ |
| 8 | QUIC + 挂载盘（FUSE / WinFsp） | ⬜ |
| 9 | 开源工程化（CI 矩阵 / 文档站） | ⬜ |

## 快速开始

```powershell
cargo test --workspace        # 全量测试
cargo clippy --workspace --all-targets   # 静态检查（零警告）
cargo run -p im-transport --example echo_demo   # 运行阶段 0 示例
```

## 代码结构

```
crates/
├── im-protocol/   二进制协议：帧编解码、命令字、seq/ack、粘包处理
├── im-transport/  传输层：Tokio TCP 长连接、心跳、重连、TLS
├── im-crypto/     加密层：TLS 材料、E2EE Signal 双棘轮
├── im-storage/    存储层：SQLite → SQLCipher、WAL 写入
├── im-server/     服务端：网关、会话路由、消息扇出
├── im-client/     客户端：CLI/TUI → 桌面端
├── im-sdk/        FFI SDK：C ABI 动态库（.so/.dll/.dylib）
├── im-bench/      压测：连接风暴、吞吐基准、弱网模拟
└── xtask/         构建任务：交叉编译、SDK 打包
```

## 学习文档

本项目同时是一套完整的 Rust 学习体系（面向 Java 工程师，全中文），见 [docs/00-roadmap.md](docs/00-roadmap.md)：

- [01 - 所有权、借用与生命周期](docs/01-rust-core.md)
- [02 - Send、Sync 与 Pin](docs/02-send-sync-pin.md)
- [03 - async/await 与 Tokio](docs/03-async-tokio.md)
- 04~11 随开发阶段逐步补充

每份文档结构：本章目标 → 概念讲解（Java 对照）→ 项目真实代码走读 → 动手练习 → 面试题与标准回答。

## 从零学习 Rust

另有独立的完整学习体系（语言从零到 Tokio 深度、Rust 版算法与数据结构、Rust 设计模式），见 [learning-rust-from-scratch/](learning-rust-from-scratch/README.md)：

- [01-basics](learning-rust-from-scratch/01-basics/) —— 语法从零开始（5 篇）
- [02-core](learning-rust-from-scratch/02-core/) —— 所有权/泛型/智能指针（5 篇）
- [03-tokio](learning-rust-from-scratch/03-tokio/) —— 异步重点深度系列（7 篇，含手写 Future/Timer）
- [04-algorithms](learning-rust-from-scratch/04-algorithms/) —— 算法与数据结构 Rust 版（7 篇，含手写环形缓冲/哈希表/堆）
- [05-patterns](learning-rust-from-scratch/05-patterns/) —— Rust 设计模式（4 篇，NEWTYPE/Typestate/Actor 等）

## 性能目标（三级里程碑）

所有数字压测前为**目标值**，压测后附脚本与原始数据：

- 单机并发连接：10 万 → 100 万 → 500 万（M3 为极限挑战）
- 消息端到端 P99 延迟 < 10ms（同机房）
- 弱网（100ms RTT + 10% 丢包）下消息到达率 99.999%

## License

MIT OR Apache-2.0
