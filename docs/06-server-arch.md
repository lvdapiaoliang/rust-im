# 06 - 阶段 3：会话层与服务端架构

> 对应代码：`crates/im-server/`（会话层）、`crates/im-client/`（最小客户端）、
> `im-protocol`（payload）、`im-transport`（去重窗口 / 退避）
> ｜ 前置阅读：`docs/04-protocol-design.md`、`docs/05-network-tokio.md`
> ｜ 学习配套：`learning-rust-from-scratch/`（Arc、channel、原子类型）

## 一、这一层解决什么问题

传输层能「收发帧 + 保活 + 优雅关闭」，但 TCP 连接 ≠ 用户。阶段 3 回答
「连接背后是谁、消息如何找到人」：

| 问题 | 答案 |
|---|---|
| 连接背后是哪个用户？ | `Handshake` 认证 → 路由表注册（`Authenticator` trait 依赖注入） |
| 消息怎么找到接收者？ | 分片并发路由表 `Router`：`user_id → ConnectionHandle` |
| 接收者不在线怎么办？ | 离线队列（`VecDeque`，有界，丢最老）+ 游标同步补投 |
| 重发的消息怎么去重？ | 位图滑动窗口 `DedupWindow`（首帧懒初始化基准） |
| 消息 ID 谁分配？ | 雪花算法（位段分配 + 时钟回拨检测） |
| 断线了客户端怎么办？ | 连接状态机 + full jitter 指数退避 + 命令排队 |

## 二、模块地图

```text
im-protocol  payload.rs     Handshake/Msg/Sync 家族的 encode/decode
im-transport dedup.rs       DedupWindow——位图滑动窗口（InOrder/OutOfOrder/Duplicate/TooFar）
             backoff.rs     Backoff——指数退避 + full jitter
             gateway.rs     spawn_gateway/run_gateway_connection 拆分（客户端需要提前拿句柄）
im-server    snowflake.rs   雪花 ID（41 位毫秒 + 10 位机器 + 12 位序列）
             router.rs      ShardedMap 分片路由表 + remove_if 谓词注销
             session.rs     Sessions 总装：认证/注册/投递/离线/同步 + serve_connection
im-client    client.rs      连接状态机：connect_once 循环 + 退避 + 事件流
```

## 三、每连接一个会话 task（核心架构）

```text
                    ┌────────────────────────────────────┐
   TCP accept ───▶  │  serve_connection（每连接一个 task） │
                    │  ┌──────────────────────────────┐  │
                    │  │ SessionState（task 独占）      │  │
                    │  │  user: Option<u64>           │  │
                    │  │  send_seq: u64               │  │
                    │  │  dedup: Option<DedupWindow>  │  │
                    │  └──────────────────────────────┘  │
                    └──────┬──────────────────────┬──────┘
                           │ 注册/注销             │ 投递查询
                    ┌──────▼──────────────────────▼──────┐
                    │ Sessions（跨 task 共享）            │
                    │  Router（分片并发）: user → handle  │
                    │  Snowflake（Mutex）: 全局 msg_id   │
                    │  offline（Mutex）: user → VecDeque │
                    └────────────────────────────────────┘
```

设计取舍：**会话状态（认证身份、seq、去重窗口）绑定在 task 里零共享**，
只有真正需要跨 task 访问的路由 / 发号 / 离线表才放进 `Sessions` 共享结构。
对照 Java 的「单例 SessionManager + 全局锁」：这里的分界是
「一个 task 能私有的就绝不去共享」。

### 3.1 通道 drop 就是关闭信号

会话 task 退出 → 本地 `frame_tx` drop → 网关读循环的 `inbound.send` 失败 →
网关回收连接 → TCP 关闭。没有显式「关连接」调用——**生命周期属主退出，
资源链自动传导**。反向同理：网关先死（EOF）→ `frame_rx` 结束 →
会话 task 走收尾注销。

### 3.2 投递降级：消息不丢优先于状态新鲜

```rust
pub async fn deliver(&self, msg: &Msg) {
    if let Some(session) = self.inner.router.get(msg.to) {
        let seq = session.send_seq.fetch_add(1, Ordering::Relaxed) + 1;
        if session.handle.send(msg.encode_frame(seq, 0)).await.is_ok() {
            return; // 在线送达
        }
    }
    self.store_offline(msg.clone()); // 离线或投递失败 → 入队
}
```

「路由查到了但 send 失败」= 接收者连接正在死亡（写 actor 已退役、
路由项还没被收尾摘掉）。消息进离线队列等重连补投，而不是丢——
发送方的 Ack 语义因此完整：**Ack 表示服务端已接管消息**，与接收方
此刻在不在线无关。

### 3.3 路由表：分片并发 + 谓词注销

`Router` 是手写 ShardedMap：`N` 个 `Mutex<HashMap>` 分片，`user_id % N`
定位分片。读写只锁一个分片——并发度从「全局一把锁」提升到
「分片数把锁」。对照 `DashMap`：思想相同，我们手写是为了吃透
「锁分片」这层（Java 的 `ConcurrentHashMap` 同款思路）。

注销用**谓词版** `remove_if(user_id, predicate)`：值校验删除的泛化。
场景：连接收尾时要保证「摘掉的路由项确实是自己的」——但值类型
`SessionHandle` 带发送通道，无法廉价构造占位实例去比对，
`remove_if(|h| h.conn_id == conn_id)` 用谓词回答「这条路由是不是我的」。

### 3.4 雪花 ID：`Option<u64>` 消除哨兵歧义

41 位毫秒时间戳 + 10 位机器 ID + 12 位序列号。序列耗尽返回
`SequenceExhausted`（调用方重试），时钟回拨返回 `ClockMovedBackwards`。

实现里最大的坑：`last_ms` 用 `u64` 且 0 当「未发过号」哨兵——
但时间戳可能**恰好真的是 0**（测试注入时钟），首帧会误入「同一毫秒」
分支导致提前耗尽。修复：`last_ms: Option<u64>`，`None` = 未发过号，
`match` 三分支（回拨 / 同毫秒 / 新毫秒）穷尽所有状态。
**教训：哨兵值与合法值域重叠时，用 `Option` 让类型系统兜底。**

### 3.5 去重窗口：位图滑动窗口 + 懒初始化

`DedupWindow` 用固定长度位图表达「最近 N 个 seq 见过没有」，
`feed(seq)` 返回四种判定：`InOrder` / `OutOfOrder`（上递业务层）、
`Duplicate` / `TooFar`（丢弃）。服务端会话对窗口**懒初始化**：
以客户端首帧 seq 为基准建窗——不依赖「客户端 seq 从 1 开始」的约定，
乱序起点也能对齐。

### 3.6 认证：`Authenticator` trait 依赖注入

`AllowAll`（开发）/ `StaticToken`（演示）只是两个实现。阶段 12 换
挑战-应答（E2EE 握手）时只换实现，`serve_connection` 一行不改——
策略模式的通道版。

## 四、客户端：连接状态机 + 事件流

```text
业务层 ◀─ mpsc ─ ClientEvent      ClientCommand ─ mpsc ─▶ 业务层
   ▲                                   │
   │            ┌──────────────────────┴────────┐
   │            │       run_client 主循环        │
   │            │  loop { connect_once(...);     │
   │            │        backoff.next_delay() }  │
   │            └───────┬────────────────────────┘
   │                    │ 每轮：TCP → spawn_gateway → 握手
   │                    │ → SyncReq → select{ 命令, 入站帧 }
   ▼                    ▼
  TCP ──────────────▶ 服务端
```

三个关键语义：

- **握手被拒 ≠ 网络故障**：`Rejected` 停止重连（重试没有意义——
  换 token 或账号），`Disconnected` 才走退避。状态机的分流点；
- **命令排队即断线缓冲**：断线期间 `send_msg` 只是入队，重连后统一
  发出。代价（未 ACK 的消息断线可能丢）由阶段 4 的本地消息库重发表解决；
- **同步游标跨重连保留**：`last_msg_id` 在主循环层持有，每轮
  `connect_once` 返回时更新——重连只补增量，不重拉全量。

`spawn_gateway` / `run_gateway_connection` 的拆分是本阶段对传输层的
唯一 API 修改：客户端要在收到任何入站帧**之前**发握手帧，而句柄
原来随 `InboundFrame` 附带——拆出「spawn 后立刻拿句柄」的形态。

## 五、算法 / 数据结构 / 设计模式落点（对照 roadmap 4.5）

| 落点 | 实战内容 |
|---|---|
| ShardedMap（分片哈希表） | `router.rs`：锁分片并发，对比 `DashMap` 思想 |
| 雪花 ID（位段分配） | `snowflake.rs`：时钟回拨检测 + `Option` 哨兵修复 |
| 位图滑动窗口 | `dedup.rs`：O(1) 去重判定，最长连续 seq 提交 |
| 指数退避 + full jitter | `backoff.rs`：重连风暴打散（AWS 架构博客同款） |
| VecDeque 有界队列 | 离线暂存：超上限丢最老，O(1) 头部弹出 |
| Actor 模式 | 每连接一个会话 task + channel 通信，零共享 |
| 策略模式 | `Authenticator` / `HeartbeatPolicy` 依赖注入 |
| 状态机 | 客户端 `Outcome` 三态 + 服务端 `Option<user>` 登录态 |
| 观察者（通道版） | `ClientEvent` 事件流：UI 只订阅事件，不碰协议 |

## 六、测试策略

| 层 | 用例群 | 数量 |
|---|---|---|
| im-protocol | payload 往返 / 拒绝语义 / 帧级 encode-decode | 单元 + doctest |
| im-transport | 去重窗口四判定 / 退避 jitter 边界 / spawn_gateway | 35 单元 + 6 doctest |
| im-server | 握手（成功/拒绝/重复）、路由、离线有界、seq 去重、断连注销重连 | 25 单元 + 2 doctest |
| im-client | 双端互发 / 离线补投 / 闪断重连 / 被拒停机 / 断线排队 | 5 单元 + 1 doctest |
| e2e 集成 | 三幕故事线：在线互发 → 掉线暂存 → 补投恢复 | `tests/e2e.rs` |

集成测试的价值：单元测试各自验证单一场景，e2e 把整条业务故事线
串起来，验证状态在多阶段流转间依然自洽（雪花单调、补投有序、
重连后双向通）。

## 七、开发中踩过的坑

- **`ShutdownTx` 的 drop 语义**：「sender 全部 drop = 视为已关停」
  （`watch` 通道特性）。两个连环坑：(1) 测试辅助函数里 `spawn_server`
  返回的 shutdown_tx 绑在函数局部，函数返回即 drop——服务端当场退场，
  客户端永远连不上；(2) 手动 accept 循环里 `let (_tx, rx) = shutdown_channel()`
  之后把 `rx` move 进连接 task、`_tx` 留在外层块作用域——spawn 完即 drop，
  每条「正常」连接也被秒关。**教训：用到 ShutdownRx 的地方，必须想清楚
  对应的 Tx 活多久**；
- **闪断名额的竞争**：「accept 后立即 drop 模拟闪断」的测试里，
  闪断条件写成 `accepted == 1`，而第一个连接往往被先启动的旁观客户端
  撞上——她的连接被闪断，主角反而直连成功。**教训：基于连接次序的
  测试逻辑，必须先固定「谁先连」**；
- **空 `SyncBatch` 是噪音**：客户端每次连接成功都自动发 `SyncReq`，
  服务端回空批也是事件——测试断言「下一个事件是 Message/Ack」时
  会被这个例行公事打断。测试脚手架统一在 `next_event` 里过滤空批；
- **时钟停在 0**：雪花测试注入 `now = 0` 的时钟，`last_ms = 0` 哨兵
  语义被击穿（见 3.4）；
- **`tokio::spawn` 要求 `'static`**：`spawn(serve_connection(&sessions, ...))`
  借用局部变量编译不过——`async move` 块包住 owned clone 是标准解法；
- **`StdinLock` 非 `Send`**：CLI 的 stdin 读行循环里跨 `await` 持有
  `StdinLock`，整个 future 不满足 `Send`。解法：每行解析后交给独立的
  发送 task，输入循环本身零 `await` 点。

## 八、下一步（阶段 4 预告）

会话层已能「认证 + 路由 + 离线补投 + 重连」，但客户端还是裸的：

- TUI 界面（ratatui）：订阅 `ClientEvent` 事件流即可——事件驱动的回报；
- 本地消息库（自研简化 LSM：追加段 + memtable + 压实）：重发表 +
  同步游标持久化，`client_msg_id` 作为跨重发稳定的去重键；
- 消息级重传：超时未 Ack 的消息按退避重发，至少一次 + 去重 =
  恰好一次。

---

*阶段 3 完成于：payload 编解码、去重窗口、指数退避、雪花 ID、分片路由表、
会话层总装、最小客户端、e2e 集成测试；全 workspace 测试全绿，
clippy pedantic 零警告。*
