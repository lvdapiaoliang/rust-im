# 00 - 项目总路线图与知识图谱

> 本文档是整个 rust-im 项目的「地图」。每开始一个新阶段，先回到这里看一眼
> 你在全局的什么位置；每学完一份文档，回到这里勾掉对应的岗位要求。

## 一、这个项目是什么

rust-im 是一个用 Rust 从零构建的开源 IM（即时通讯）全栈项目：

- **目标定位**：对标 Telegram 的功能形态，工程指标见下表
- **技术形态**：Cargo workspace 多 crate —— 协议层 / 传输层 / 加密层 / 存储层 / 服务端 / 客户端 / FFI SDK / 压测工具
- **学习形态**：每个开发阶段配套全中文学习文档，以 Java 工程师视角对照讲解，面试题直接对标「Rust 资深开发（IM / 挂载盘、跨平台 SDK）」岗位

### 性能目标（三级里程碑）

所有数字在未压测前均为**目标值**，压测后用真实数据替换（数据 + 脚本全部开源）：

| 里程碑 | 指标 | 主要手段 | 状态 |
|--------|------|----------|------|
| M1 | 单机 10 万并发连接 | 常规 Tokio + 参数调优 | 未开始 |
| M2 | 单机 100 万并发连接 | `io_uring`、内核调优、每连接内存压到 KB 级 | 未开始 |
| M3 | 单机 500 万并发连接 | `SO_REUSEPORT` 多进程、内存池、零拷贝（极限挑战） | 未开始 |
| 延迟 | 消息端到端 P99 < 10ms（同机房） | 背压、锁竞争消除、批量化 | 未开始 |
| 可靠 | 100ms RTT + 10% 丢包下到达率 99.999% | ACK + 指数退避重传 + 去重 | 未开始 |

> 说明：「500 万并发」是 C10M 级世界难题，作为项目愿景保留。真实的优化过程
> （哪怕最终停在 200 万）远比口号有价值——每个瓶颈、每次火焰图分析都会写进 docs/08。

## 二、代码结构

```
crates/
├── im-protocol/   二进制协议：帧编解码、命令字、seq/ack、粘包处理（纯逻辑，无 IO）
├── im-transport/  传输层：Tokio TCP 长连接、心跳、重连、TLS、（后期 QUIC）
├── im-crypto/     加密层：TLS 材料、E2EE Signal 双棘轮、密钥存储
├── im-storage/    存储层：SQLite → SQLCipher、WAL 写入、元数据缓存
├── im-server/     服务端：网关、会话路由、消息扇出、离线消息
├── im-client/     客户端：CLI/TUI（ratatui）→ 桌面端（Tauri）
├── im-sdk/        FFI SDK：C ABI 动态库（.so/.dll/.dylib）+ JNI 示例
├── im-bench/      压测：连接风暴、吞吐基准、用户态弱网模拟
└── xtask/         构建任务：交叉编译、SDK 打包（cargo xtask <task>）
```

依赖方向（自底向上）：

```
im-protocol ← im-transport ← im-server / im-client / im-sdk
                  ↑                ↑
        im-crypto ┘                └── im-storage
```

## 三、开发阶段总览

| 阶段 | 内容 | 代码入口 | 配套文档 | 状态 |
|------|------|----------|----------|------|
| 0 | workspace 骨架 + echo 热身 | `im-transport/src/echo.rs` | 01、02、03 | ✅ 已完成 |
| 1 | 二进制协议：帧格式 / 编解码 / 粘包 | `im-protocol` | 04 | 未开始 |
| 2 | 传输层：心跳 / 重连 / seq-ACK / TLS | `im-transport` | 05 | 未开始 |
| 3 | 服务端：网关 / 路由 / 扇出 / 持久化 | `im-server` | 06 | 未开始 |
| 4 | 客户端：TUI / 消息同步 / 本地库 | `im-client` | 07 | 未开始 |
| 5 | 压测与三级性能里程碑 | `im-bench` | 08 | 未开始 |
| 6 | FFI SDK：C ABI / JNI / 内存契约 | `im-sdk` | 09 | 未开始 |
| 7 | 桌面端（Tauri）+ E2EE（Signal） | `im-client` + `im-crypto` | 10 | 未开始 |
| 8 | QUIC（quinn）+ 挂载盘（FUSE/WinFsp） | 扩展 | 11 | 未开始 |
| 9 | 开源工程化：CI 矩阵 / 版本 / 文档站 | `.github` | — | 未开始 |

## 四、知识图谱：岗位要求 ↔ 项目模块 ↔ 文档

> 这是本文档最重要的部分：**招聘要求里的每一句话，在这个项目里都有落点**。
> 面试前的复习路径就是这张表的从左到右。

### 4.1 Rust 语言核心

| 岗位要求 | 在哪里学 | 在哪里练 |
|----------|----------|----------|
| 所有权、借用规则 | 文档 01 | 全部代码，尤其 `echo.rs` 的 `&buf[..n]` |
| 生命周期标注、省略规则 | 文档 01 | 阶段 1 编解码器（`encode<'a>(&'a [u8]) -> Frame<'a>`） |
| 悬垂指针如何被编译器禁止 | 文档 01 | 阶段 2 连接管理 |
| Send / Sync、何时不能自动实现 | 文档 02 | 阶段 3 路由表（跨 task 共享） |
| FFI 裸指针为什么破坏 Send/Sync | 文档 02 + 09 | 阶段 6 `im-sdk` |
| Pin、自引用结构体、`Pin<&mut T>` | 文档 02 | 阶段 2 自定义 Future |
| async/await 原理、Waker | 文档 03 | 阶段 2 手写一个简化版 `join` |
| Tokio 多线程调度、spawn、阻塞任务 | 文档 03 | `echo.rs`（已实现）、阶段 3 网关 |
| io_uring | 文档 08 | 阶段 5 M2 里程碑 |

### 4.2 网络编程与 IM

| 岗位要求 | 在哪里学 | 在哪里练 |
|----------|----------|----------|
| TCP 字节流本质、粘包/半包 | 文档 04 | 阶段 1 帧编解码器 + proptest 模糊测试 |
| 心跳、断线重连（指数退避） | 文档 05 | 阶段 2 `im-transport` |
| 消息 ACK、有序性、去重窗口 | 文档 05 | 阶段 2 seq/ack 机制 |
| 弱网优化、100ms RTT + 10% 丢包 | 文档 08 | 阶段 5 用户态弱网模拟器 |
| QUIC | 文档 11 | 阶段 8 quinn 集成与 TCP 对比 |
| TLS/mTLS 握手 | 文档 10 | 阶段 2 rustls |
| E2EE / Signal 协议 | 文档 10 | 阶段 7 X3DH + 双棘轮实现 |

### 4.3 跨平台与 FFI

| 岗位要求 | 在哪里学 | 在哪里练 |
|----------|----------|----------|
| C ABI、句柄式 API 设计 | 文档 09 | 阶段 6 `im-sdk` |
| 跨语言内存释放（谁分配谁释放） | 文档 09 | 阶段 6 `im_sdk_free` 系列 |
| .so / .dll / .dylib 产物 | 文档 09 | 阶段 6 + xtask 打包 |
| 交叉编译 Android/iOS | 文档 09 | 阶段 6 xtask cross 任务 |
| 字符串编码坑（UTF-8 / UTF-16 / char*） | 文档 09 | 阶段 6 |
| Flutter/Electron/RN 集成 | 文档 10 | 阶段 7 Tauri 桌面端 |

### 4.4 工程化与业务

| 岗位要求 | 在哪里学 | 在哪里练 |
|----------|----------|----------|
| SDK 从 0 到 1 设计（API/错误模型/版本兼容） | 文档 09 | 阶段 6 |
| 背压、扇出风暴、内存账本 | 文档 06 | 阶段 3 服务端 |
| 压测方法、火焰图、量化优化 | 文档 08 | 阶段 5 |
| 挂载盘（FUSE / WinFsp / 元数据缓存） | 文档 11 | 阶段 8 |

## 五、文档阅读顺序（Java 工程师路径）

```
01-rust-core.md        所有权/借用/生命周期 —— 一切的地基，必须先读
02-send-sync-pin.md    Send/Sync/Pin —— 多线程与 FFI 的类型关卡
03-async-tokio.md      async/await/Tokio —— 配合 echo.rs 源码对照阅读
04-protocol-design.md  （阶段 1）二进制协议设计
05-network-tokio.md    （阶段 2）网络编程深入
06-server-arch.md      （阶段 3）服务端架构
07-client.md           （阶段 4）客户端
08-perf.md             （阶段 5）性能压测与调优
09-ffi.md              （阶段 6）FFI SDK
10-e2ee.md             （阶段 7）端到端加密
11-quic-fuse.md        （阶段 8）QUIC 与挂载盘
```

每份文档统一结构：**本章目标 → 概念讲解（Java 对照）→ 本项目真实代码走读 → 动手练习 → 面试题与标准回答**。

## 六、常用命令

```powershell
cargo test --workspace        # 全量测试
cargo clippy --workspace --all-targets   # 静态检查（项目要求零警告）
cargo fmt --all               # 格式化
cargo xtask test              # 通过 xtask 跑测试（后续扩展更多任务）
cargo run -p im-transport --example echo_demo   # 阶段 0 示例（见 docs/03 末尾）
```

## 七、协作约定

- 每个阶段完成 = 测试全绿 + clippy 零警告 + 文档补齐，才允许提交 git
- 性能数字未压测一律标注「目标值」
- 提交信息格式：`phase-N: <简述>`（例：`phase-0: workspace 骨架与 echo 热身`）
