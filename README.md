# rust-im

用 Rust 从零构建的开源 IM（即时通讯）全栈项目。对标 Telegram 的功能形态，目标支撑单机百万级长连接的实时消息系统。

## 项目状态

**阶段 0~12 已完成**：二进制协议、传输层（心跳/优雅关闭）、会话层（认证/路由/离线补投）、
客户端消息级重传 + 自研本地库（LSM 思想）+ ratatui TUI，全链路 e2e 含崩溃重传场景；
Web 接入与持久化（FrameSink 传输解耦、PostgreSQL + sqlx、axum REST、WS 网关
JSON 信封协议、Vue 3 前端骨架），TCP/TUI 与 Web 双接入并存；好友系统全流程
（事件推送 + 服务端仅好友间可发消息校验）与富媒体消息（文本/表情/图片/文件，
TUI 对非 text 降级显示）已上线；群组系统（每群一个扇出 actor + 成员快照写时失效
+ `try_send` 慢消费者隔离 + `not_member` 发送门槛）已上线，im-bench 实测单 actor
吞吐 ~94 万人次/秒（2 万人单条 P50 ≈ 19ms，人均 ~1µs，见 docs/13）；
1对1 音视频与远程桌面（WebRTC P2P：信令走 WS `signal` 信封不透明转发，
媒体流端到端直连不经服务器，见 docs/14）；群会议与屏幕共享（LiveKit SFU：
服务端手签 JWT 入会令牌 + is_member 门槛，媒体转发外包给 SFU，见 docs/15）已上线。
压测与性能里程碑（M1 达成：单机 99,969 并发连接、每连接 29.59 KiB（双端）、
拆除 6.02s 路由表清零；分位数草图（HdrHistogram 思想）从零实现；
用户态弱网模拟器（确定性丢包/延迟/乱序）实测双向 10% 丢包 + 100ms RTT 下
上行 100% 到达；顺带抓出并修复接收窗静默楔死缺陷，见 docs/16）；
FFI SDK（`im-sdk`：同步外观 + 7 函数 C ABI + 手写 `im_sdk.h`，三条跨语言契约
——内存/线程/错误；JNI 绑定（feature `jni`）经 JDK 27 真机冒烟：中文消息
全链路无损、干净关闭；`cargo xtask sdk` 一键打包 dist/sdk，交叉编译诚实跳过，
见 docs/17）；TLS 传输加密与 E2EE（rustls：Connection 泛型化 +
GatewayStream 依赖倒置，自签 CA 材料层与装配层分离；Signal 双棘轮
学习实现：X3DH 异步密钥协商 + 消息级/轮级双棘轮 + 乱序容忍跳过缓存，
性质测试替代官方测试向量；`im-sdk::native` 类型状态 Rust 原生 API
（TypedSdkClient<Connected>——未连接的形态上 send 方法不存在，
compile_fail doctest 锁死承诺），Tauri 壳走诚实边界，见 docs/18）。

| 阶段 | 内容 | 状态 |
|------|------|------|
| 0 | workspace 骨架 + echo 热身 | ✅ |
| 1 | 二进制协议（帧编解码 / 粘包处理） | ✅ |
| 2 | 传输层（心跳 / 重连 / seq-ACK / 优雅关闭；TLS 移至阶段 12 前置） | ✅ |
| 3 | 服务端（认证 / 会话路由 / 离线补投 / 雪花 ID）+ 最小客户端 | ✅ |
| 4 | 客户端（消息重传 / 本地库 / TUI / 消息同步） | ✅ |
| 5 | Web 接入与持久化（REST + WS 网关 + PostgreSQL + Vue 前端） | ✅ |
| 6 | 好友系统全流程 + 富媒体消息（文件 / 表情） | ✅ |
| 7 | 群组 + 2 万人同时在线（群扇出 + 慢消费者隔离） | ✅ |
| 8 | 1对1 音视频 + 远程桌面（WebRTC P2P） | ✅ |
| 9 | 群会议 + 屏幕共享（LiveKit SFU） | ✅ |
| 10 | 压测与三级性能里程碑（M1 达成 99,969 连接；修复接收窗楔死缺陷） | ✅ |
| 11 | FFI SDK（C ABI 动态库 / JNI） | ✅ |
| 12 | TLS（rustls）+ E2EE（Signal 双棘轮）+ 类型状态原生 API | ✅ |
| 13 | QUIC + 挂载盘（FUSE / WinFsp） | ⬜ |
| 14 | 开源工程化（CI 矩阵 / 文档站） | ⬜ |

> 阶段重排说明：阶段 5~9 新增 Web 接入与社交功能，原压测/FFI/桌面+E2EE/QUIC/
> 工程化顺延为 10~14，详见 [docs/00-roadmap.md](docs/00-roadmap.md)。

## 快速开始

前置（Web 功能需要）：PostgreSQL 可达，建库后由 sqlx 迁移自动建表；
连接串等环境变量见 `crates/im-server/src/main.rs`（`IM_DATABASE_URL` /
`IM_WEB_ADDR` / `IM_SERVER_ADDR` / `IM_FILES_DIR` 可覆盖）。

```powershell
cargo test --workspace        # 全量测试
cargo clippy --workspace --all-targets   # 静态检查（零警告）
cargo run -p im-transport --example echo_demo   # 运行阶段 0 示例
cargo run -p im-server                     # 起服务端（TCP 127.0.0.1:8888 + Web 127.0.0.1:8080）
cargo run -p im-client 127.0.0.1:8888 1 demo    # 起 TUI 客户端（TCP 二进制路径）
cargo run -p im-bench --release -- conn-storm --connections 100000 --source-ips 7  # M1 连接风暴
cargo run -p im-bench --release -- weak-link        # 弱网可靠性（10% 丢包 + 100ms RTT）
cargo xtask sdk               # 打包 FFI SDK 到 dist/sdk/（头文件 + JNI 源 + 动态库）
cargo run -p im-server --example tls_demo   # TLS 全链路演示（自签证书 + 加密流跑帧协议）

# Web 前端（另一个终端，Node 18+）
cd web
npm install
npm run dev                  # Vite 开发服务器（5173，代理 /api 与 /ws 到 8080）

# 群会议（可选，需要 Docker）：起 LiveKit SFU，后端默认凭据零配置对接
# docker compose -f deploy/docker-compose.yml up -d
```

TUI 按键：`/to <user_id>` 新会话 · `Tab` 切换会话 · Enter 发送 ·
PageUp/Down 翻历史 · `/quit` 或 Esc 退出。本地消息库与重发表持久化在
`im-client-data/`（重启不丢）。Web 端在浏览器注册/登录后走
WS JSON 信封协议（见 [docs/12-web-protocol.md](docs/12-web-protocol.md)）。

## 代码结构

```
crates/
├── im-protocol/   二进制协议：帧编解码、命令字、seq/ack、粘包处理
├── im-transport/  传输层：Tokio TCP 长连接、心跳、重连、TLS
├── im-crypto/     加密层：TLS 材料、E2EE Signal 双棘轮
├── im-storage/    存储层：自研简化 LSM（追加段/memtable/压实）+ WAL 恢复
├── im-server/     服务端：网关、会话路由、消息扇出
├── im-client/     客户端：消息重传/本地库/ratatui TUI → 桌面端
├── im-sdk/        FFI SDK：C ABI 动态库（.so/.dll/.dylib）+ JNI 绑定 + 类型状态原生 API
├── im-bench/      压测：连接风暴、吞吐基准、弱网模拟
└── xtask/         构建任务：交叉编译、SDK 打包

web/               Web 前端：Vue 3 + TypeScript + Pinia（npm 项目，非 cargo 成员）
```

## 学习文档

本项目同时是一套完整的 Rust 学习体系（面向 Java 工程师，全中文），见 [docs/00-roadmap.md](docs/00-roadmap.md)：

- [01 - 所有权、借用与生命周期](docs/01-rust-core.md)
- [02 - Send、Sync 与 Pin](docs/02-send-sync-pin.md)
- [03 - async/await 与 Tokio](docs/03-async-tokio.md)
- [04 - 二进制协议设计](docs/04-protocol-design.md)（阶段 1）
- [05 - 传输层设计](docs/05-network-tokio.md)（阶段 2）
- [06 - 服务端架构](docs/06-server-arch.md)（阶段 3）
- [07 - 客户端：消息可靠性、本地库与 TUI](docs/07-client.md)（阶段 4）
- [12 - Web 协议：REST、WS JSON 信封、事件推送与富媒体内容](docs/12-web-protocol.md)（阶段 5~6）
- [13 - 群消息扇出与 2 万人在线](docs/13-group-fanout.md)（阶段 7）
- [14 - WebRTC 音视频与远程桌面](docs/14-webrtc.md)（阶段 8）
- [15 - 群会议与屏幕共享（LiveKit SFU）](docs/15-meeting.md)（阶段 9）
- [16 - 性能压测与三级里程碑](docs/16-perf.md)（阶段 10）
- [17 - FFI SDK：C ABI / JNI / 内存契约](docs/17-ffi.md)（阶段 11）
- [18 - TLS 与 E2EE：rustls 传输加密 + Signal 双棘轮 + 类型状态原生 API](docs/18-tls-e2ee.md)（阶段 12）
- [20 - Rust 全栈踩坑与填坑实录（含业务开发常见错误）](docs/20-rust-pitfalls.md)（全程）
- 19 随开发阶段逐步补充（QUIC 与挂载盘）

每份文档结构：本章目标 → 概念讲解（Java 对照）→ 项目真实代码走读 → 动手练习 → 面试题与标准回答。

## 从零学习 Rust

另有独立的完整学习体系（语言从零到 Tokio 深度、Rust 版算法与数据结构、Rust 设计模式），见 [learning-rust-from-scratch/](learning-rust-from-scratch/README.md)：

- [01-basics](learning-rust-from-scratch/01-basics/) —— 语法从零开始（5 篇）
- [02-core](learning-rust-from-scratch/02-core/) —— 所有权/泛型/智能指针（5 篇）
- [03-tokio](learning-rust-from-scratch/03-tokio/) —— 异步重点深度系列（7 篇，含手写 Future/Timer）
- [04-algorithms](learning-rust-from-scratch/04-algorithms/) —— 算法与数据结构 Rust 版（7 篇，含手写环形缓冲/哈希表/堆）
- [05-patterns](learning-rust-from-scratch/05-patterns/) —— Rust 设计模式（4 篇，NEWTYPE/Typestate/Actor 等）

## 性能目标（三级里程碑）

所有数字压测前为**目标值**，压测后附脚本与原始数据（实测记录见 [docs/16-perf.md](docs/16-perf.md)）：

- 单机并发连接：10 万 ✅（99,969，每连接 29.59 KiB 双端）→ 100 万 → 500 万（M3 为极限挑战）
- 消息端到端 P99 延迟 < 10ms（同机房；未测——无同机房场景，不冒充）
- 弱网（100ms RTT + 10% 丢包）：上行 100% 到达（200/200 确认）；下行单次 90%
  （投递不重传为已知边界，下行 ACK 闭环记为欠账）

## License

MIT OR Apache-2.0
