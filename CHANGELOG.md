# 更新日志（CHANGELOG）

本项目所有值得注意的变更记录在此。格式参照 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang-lang/zh-CN/)；版本号与各 crate 统一由
workspace 管理（根 `Cargo.toml` 的 `[workspace.package]`，一处 bump 全 workspace 生效）。

项目按阶段演进，每个阶段的动机、代码走读与面试题沉淀在 `docs/` 对应文档（映射见下）。

## [Unreleased]

待下一阶段补充。

## [0.1.0] —— 阶段 0~14（待打 tag 发布）

首个版本：从 workspace 骨架到 QUIC 传输、挂载盘语义层与开源工程化（CI/文档站）的完整 IM 全栈。
每个阶段的详细设计见对应文档。

### 阶段 0~1：骨架与协议（docs/04）

- Cargo workspace 骨架（统一依赖版本、统一 lints：clippy `all` + `pedantic` 零警告口径）
- 二进制协议：帧编解码、命令字、seq/ACK、粘包处理

### 阶段 2~3：传输与服务端（docs/05、docs/06）

- 传输层：心跳保活、读空闲超时、指数退避重连、优雅关闭
- 服务端：认证、会话路由、离线补投、雪花 ID

### 阶段 4：客户端可靠性（docs/07）

- 消息级 ACK 重传、自研本地库（LSM 思想：追加段/memtable/压实 + WAL 恢复）
- ratatui TUI（多会话切换、历史翻页）、崩溃重启后的消息同步

### 阶段 5：Web 接入与持久化（docs/12）

- `FrameSink` 传输解耦：TCP 二进制与 WS JSON 信封双接入并存
- PostgreSQL 持久化（sqlx 迁移，运行时查询 API——无库环境可测试，CI 配真库补盲区）
- axum REST API + Vue 3 前端骨架（登录/会话列表/聊天窗）

### 阶段 6：好友与富媒体（docs/12）

- 好友系统全流程：申请/接受/删除 + 事件推送通道（服务端 `Sessions::push_event`）
- 服务端仅好友间可发消息的校验门槛；富媒体消息（文本/表情/图片/文件）
- TUI 对非 text 消息降级显示 `[文件]`/`[图片]`/`[表情]`

### 阶段 7：群组与扇出（docs/13）

- 每群一个扇出 actor + 成员快照写时失效 + `try_send` 慢消费者隔离
- im-bench 实测：单 actor 吞吐 ~94 万人次/秒（2 万人单条 P50 ≈ 19ms）

### 阶段 8~9：音视频（docs/14、docs/15）

- 1对1 音视频与远程桌面：WebRTC P2P，信令走 WS `signal` 信封服务端不透明转发
- 群会议与屏幕共享：LiveKit SFU，服务端手签 JWT 入会令牌 + is_member 门槛

### 阶段 10：压测与性能（docs/16）

- 分位数草图（HdrHistogram 思想）从零实现；用户态弱网模拟器（确定性丢包/延迟/乱序）
- M1 达成：单机 99,969 并发连接、每连接 29.59 KiB（双端）、拆除 6.02s 路由表清零
- 抓出并修复接收窗静默楔死缺陷（TooFar 重同步），修复后弱网上行 100% 到达

### 阶段 11：FFI SDK（docs/17）

- `im-sdk`：同步外观 + 7 函数 C ABI + 手写 `im_sdk.h`，三条跨语言契约（内存/线程/错误）
- JNI 绑定（feature `jni`）经 JDK 27 真机冒烟；`cargo xtask sdk` 一键打包 dist/sdk

### 阶段 12：TLS 与 E2EE（docs/18）

- rustls TLS：`Connection` 泛型化 + `GatewayStream` 依赖倒置（材料层与装配层分离）
- Signal 双棘轮学习实现：X3DH 密钥协商 + 消息级/轮级双棘轮 + 乱序容忍跳过缓存
- 类型状态原生 API：`TypedSdkClient<Connected>`（未连接形态上 `send` 方法不存在，compile_fail doctest 锁死）

### 阶段 13：QUIC 与挂载盘（docs/19）

- quinn 装配层：`QuicStream` 适配器实现 `GatewayStream`（阶段 12 泛型化的利息零改动兑现）
- 多路复用 + 无队头阻塞测试钉进 CI；quic_demo 两条会话流共用一条连接
- `im-mount` 挂载盘语义层：手写 LRU（slab 下标版，零 unsafe）+ 内存 FS（FUSE 回调对齐
  + POSIX errno 分类）+ 目录缓存（「先改数据再失效」纪律用类型封装）+ IM → FS 只读视图映射

### 阶段 14：开源工程化

- GitHub Actions CI 矩阵：fmt/clippy 门槛 + 三平台构建测试（ubuntu/windows/macos 验工具链，
  DB 测试靠 pool_or_skip 空转）+ ubuntu-only postgres service 真跑 DB 测试 + tls_demo/quic_demo 全链路自检
- 版本与发布：CHANGELOG 立账；版本号由 workspace 统一管理（`version.workspace = true` 全 crate 继承）
- 文档站：docs/ 经 mdBook 构建发布到 GitHub Pages（`docs/SUMMARY.md` + `pages.yml`）
- 修复（CI 落地抓出的第一个真 bug）：并行集成测试共用默认 `machine_id`，雪花 ID
  在同一毫秒撞库唯一约束（偶发失败、单跑必过）——脚手架统一发唯一 `machine_id`（docs/20 §4.8）
- 修复（Actions 实机验证抓出）：service container 仅支持 Linux runner，postgres service
  放在三平台矩阵 job 级别会让 windows/macos 腿启动即失败——拆为三平台矩阵（无 service）
  + ubuntu-only 真库 job（docs/20 §6.7）；拆后 CI 已线上全绿（fmt/clippy/三平台矩阵/postgres）
- 修复（Actions 实机验证抓出）：pages.yml 引用不存在的 `peaceiris/action-mdbook`（少个 s，
  报 repository not found），已改 `peaceiris/actions-mdbook`（docs/20 §6.7）
