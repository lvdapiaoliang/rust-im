# 05 - im-transport 传输层设计（阶段 2）

> 对应代码：`crates/im-transport/` ｜ 前置阅读：`docs/03-async-tokio.md`、`docs/04-protocol-design.md`
> 学习配套：`learning-rust-from-scratch/03-tokio/`（select、channel、超时与取消）、`05-patterns/`（Actor 模式）

## 一、这一层解决什么问题

阶段 1 产出了纯逻辑的帧编解码器，但它还不认识 TCP。阶段 2 回答四个
任何长连接系统都绕不开的问题：

| 问题 | 答案 |
|---|---|
| 怎么把字节流变成帧流？ | `Connection`：`read_frame` / `write_frame`，粘包半包全透明 |
| 怎么发现「对端已经死了但 TCP 还连着」？ | 心跳 Ping/Pong + 读空闲超时 |
| 多个 task 想同时写一条连接怎么办？ | 写 actor 独占写半部，发送方只与通道打交道 |
| 怎么停？ | 两级关停信号 + 写队列排干（优雅 = 已接受的帧一条不丢） |

## 二、模块地图

```text
im-transport
├── connection.rs   Connection / ReadHalf / WriteHalf —— 帧流与半部拆分
├── gateway.rs      run_gateway_connection —— 每连接生命周期管理器
├── shutdown.rs     ShutdownTx / ShutdownRx —— watch 通道封装的关停信号
├── error.rs        TransportError —— Io / Protocol / IdleTimeout / Closed
└── echo.rs         阶段 0 热身代码（保留作学习参照）
```

## 三、每连接的任务布局（核心设计）

一条连接 = 三个协作的 task，**零锁、零共享可变状态**：

```text
  对端 TCP ◀═════════════════════════════════════════════════╣
     │ 读                          写                        │
     ▼                            ▲                          │
 ┌─────────────┐  InboundFrame  ┌─┴────────────┐            │
 │  读循环 task │ ─────────────▶│   业务层      │            │
 │ （读循环）    │                │ （调用方）    │            │
 └──────┬──────┘                └──────┬───────┘            │
        │ Ping(服务端模式)              │ ConnectionHandle   │
        │ → 组装 Pong                   │ .send(frame)       │
        ▼                              ▼                     │
     mpsc 通道 ◀── 心跳 task(客户端模式, 定时发 Ping) ─┐       │
        │                                          │       │
        ▼                                          │       │
 ┌─────────────┐      done 信号（读循环结束时触发）   │       │
 │ 写 actor task│ ◀───────────────────────────────┘       │
 │ （排干队列后退出）                                       │
 └───────────────────────────────────────────────────────────┘
```

### 3.1 为什么写侧用 actor 而不是 `Arc<Mutex<WriteHalf>>`

- **锁的竞争面**：百万连接下广播场景，写锁是热点；通道把「争用」变成
  「排队」，语义从互斥升级为顺序化——帧与帧之间天然不交错；
- **背压显式化**：通道满了 `send` 挂起，慢消费者拖慢生产者（链路级反压），
  而锁只会让后来的 task 排队干等；
- **生命周期清晰**：写 actor 与连接同生共死，`Drop` 写半部即关闭 socket 写方向。

### 3.2 `InboundFrame { peer, handle, frame }`：把「回话权」随帧递送

业务层收到帧的同时拿到 `ConnectionHandle`——不需要自己维护
「连接 id → 发送端」路由表就能回话。这是 Actor 模式「消息里带回复地址」
（mailbox 回信模式）的移植。

### 3.3 两级关停信号

| 信号 | 范围 | 触发者 |
|---|---|---|
| 外部 `ShutdownRx` | 整个服务的关停 | 服务管理员 / 测试 |
| 内部 `done` 通道 | **单条连接**的收尾 | `run_gateway_connection` 自己 |

读循环无论因何退出（EOF / 超时 / 协议错误 / 外部关停），都走同一段
收尾代码：`done.trigger()` → 心跳退出 → 写 actor 排干队列退出 →
等待两个 task 结束 → 返回原因。**函数返回 = 资源全部回收**，
调用方拿到返回值就能安全地清理与这条连接相关的任何记账。

### 3.4 优雅关闭的定义

`shutdown_drains_queued_outbound_frames` 测试固化了语义：
关停前排队的 5 帧必须全部送达对端，**然后**连接才关闭。
写 actor 收到 `done` 后不立即退出，先 `try_recv` 排干队列——
对照 Java 的 `shutdownGracefully(quietPeriod, timeout)`：语义相同，
实现只是几行 `while let Ok(...)`。

## 四、心跳与超时的语义

- **服务端**（`HeartbeatPolicy::Server`）：收到 `Ping` 就地回 `Pong`
  （`ack = seq + 1`，累计确认语义的预演，阶段 3 滑动窗口展开），
  `Ping` 不进业务层；读空闲超过 `idle_timeout` 判定对端死亡；
- **客户端**（`HeartbeatPolicy::Client { interval }`）：每 `interval`
  发一个 `Ping`；服务端回的 `Pong` 进入业务层（可做探活统计）；
- 经验值 `idle_timeout ≥ 2 × interval`：容忍连续丢一个心跳的抖动。

`timeout` 组合子包住 `read_frame`：每读到任何字节（哪怕半帧）计时器重置。
这层不用 `select!` 三路等心跳/数据/超时，因为**心跳和数据本来就是
同一个读循环的两个来源**——先到谁都重置计时器，逻辑等价且代码更少。

## 五、错误类型与「连接为什么死了」

`TransportError` 四变体对应四种处置策略：

| 变体 | 含义 | 处置 |
|---|---|---|
| `Io` | 网络故障 | 记日志，可重连 |
| `Protocol` | 对端实现有 bug 或恶意 | 断连 + 观察计数 |
| `IdleTimeout` | 半开连接（弱网常态） | 静默回收，等待重连 |
| `Closed` | 本侧写通道/业务层退出 | 属于正常关停的一种 |

## 六、测试策略（全部跑在真实 TCP 回环上）

| 用例 | 覆盖 |
|---|---|
| `frame_roundtrip_over_tcp` | Connection 基本读写 |
| `split_halves_cooperate` | 半部拆分后跨代码路径协作 |
| `msg_roundtrip_through_gateway` | 主线：帧入站 → handle 回话 |
| `server_auto_replies_pong_and_swallows_ping` | 心跳应答 + 业务层隔离 |
| `client_heartbeat_receives_pong` | 客户端心跳全链路 |
| `idle_timeout_closes_silent_connection` | 静默连接被按时断开（还断言**没有提前断**） |
| `garbage_stream_returns_protocol_error` | 恶意流 → `BadMagic` 错误返回值 |
| `external_shutdown_closes_connection` | 外部关停 → 对端看到 EOF |
| `shutdown_drains_queued_outbound_frames` | 优雅关闭：排队帧一条不丢 |
| `half_written_frame_still_decodes` | 半包跨写：增量解码在真实 TCP 上成立 |

超时类测试用真实短时长（150ms）而非 paused time：回环网络的真实
调度行为本身就是被测对象的一部分。

## 七、开发中踩过的坑

- **`peer_addr` 是对端地址**：服务端看到的 peer 是客户端的
  「IP + 随机源端口」，不是自己监听的地址——断言写成
  `assert_eq!(event.peer, Some(addr))` 必然失败；
- **`.expect()` 的语义是「None 时 panic」**：想断言 `Option` 是 `None`
  必须用 `assert!(x.is_none())`，把 `expect` 当断言用会得到完全相反的行为；
- **`select!` 分支模式用 `()` 而非 `_`**：clippy 的 `ignored_unit_patterns`
  提醒——显式 `() = fut` 表达「我不关心返回值」，`_` 是忽略模式。

## 八、下一步（阶段 3 预告）

传输层已能「收发帧 + 保活 + 优雅关闭」，但还没有**会话**概念。
阶段 3（`im-server` 接入）将引入：

- 握手与认证（`Handshake` / `HandshakeAck` 真正投入使用）；
- seq/ack 滑动窗口与去重（`Frame.seq` 的单调性开始被校验）；
- 连接注册表与会话路由（`handle` 不再随帧递送，由会话表管理）；
- 指数退避重连（客户端侧）。

---

*阶段 2 完成于传输层：Connection 帧流、网关 actor、心跳、空闲超时、
两级优雅关闭；21 单元测试 + 3 doctest 全绿，clippy pedantic 零警告。*
