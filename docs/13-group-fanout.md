# 13 · 群消息扇出与 2 万人在线（阶段 7）

> 阅读前置：docs/06（会话核心与 Actor 模型）、docs/12 §四（WS 信封）。
> 本章代码入口：`crates/im-server/src/web/fanout.rs`（扇出中枢）、
> `crates/im-server/src/session.rs`（`GroupRouter`/`fanout_one`）、
> `crates/im-bench/src/main.rs`（压测）。

## 一、本章目标

单聊的「一对一」闭环（阶段 6）之后，社交形态的另一半是「一对多」：
一条群消息要复制给群里**每个成员**。本章回答四个问题：

1. 成员表在数据库里，扇出热路径怎么做到**不查库**？
2. 2 万人同时在线，一条消息扇出 2 万份，**顺序和延迟**怎么保证？
3. 一个不读消息的慢消费者，怎么做到**拖不垮全群**？
4. 「能扛 2 万人」需要**压测数字**说话——口径怎么设计，数据是多少？

阶段 7 的交付：群扇出 actor（每群一个 task）+ 成员快照缓存（写时失效）
+ `try_send` 慢消费者隔离 + `im-bench group-fanout` 压测（2 千/2 万/10 万
三点实测）+ Vue 群聊界面（建群/拉人/群会话/发送者名字）。

## 二、概念：扇出风暴与三个经典解法（Java 对照）

「一条消息复制给 N 万人」是所有 IM 的核心难题，业界有三个层次的解法：

| 解法 | 形态 | Java 世界的影子 | 本项目取舍 |
|---|---|---|---|
| **朴素循环** | 发送线程里 for 循环逐个投递 | Servlet 里 for 循环 `session.sendMessage` | ❌ 发送者被最慢的接收者卡死（队头阻塞） |
| **每连接写队列** | 投递只入队，每连接自己的写线程慢慢发 | Netty 的 `ChannelOutboundBuffer` + 写水位 `setWriteBufferWaterMark` | ✅ 我们从阶段 3 起就是每连接一个写 task（有界 mpsc） |
| **扇出专用 actor** | 每群一个单线程 executor，串行消费「扇出任务」 | Kafka 的 consumer group / Disruptor 的单写者模型 | ✅ 本章主角：**顺序性与隔离性的公共解** |

三个问题（成员表在哪、群内顺序、慢消费者）的解恰好是同一个结构——
**每群一个扇出 actor**：

- **成员表在哪**：actor 独占一份成员快照（一次全量装载，之后全吃缓存）；
- **顺序性**：单 task 串行处理收件箱，所有成员看到的序 = actor 处理序；
- **慢消费者**：actor 投递用 `try_send`，满即跳过，绝不挂起——
  「跳过一个人」远好于「卡死一群人」。

Java 工程师可以把这个 actor 理解成一个**绑定了缓存的单线程
`ExecutorService`**：没有锁、没有可见性问题（状态只被一个线程触碰），
并发安全靠「消息通信 + 不可变」而不是 synchronized。这正是 Actor 模型
（docs/05 有完整推导）第三次在项目里落地——前两次是 TCP 连接的
读写 task，这次是**领域级 actor**：它守护的不是一条连接，而是一个群。

## 三、架构：门槛 → 分流 → 扇出

群消息从 WS 网关进来到成员收到，经过三层，每层职责单一：

```text
浏览器
  │ {"type":"msg","payload":{"to":"<群ID>",…}}
  ▼
┌─ ws.rs · dispatch_inbound ────────────────────────┐
│ 门槛：to 是群 → 发送者必须是群成员（not_member）    │
│       否    → 必须是好友（not_friend，阶段 6）      │
└──────────────────────────────────────────────────┘
  ▼ Frame（与 TCP 路径同一二进制帧）
┌─ session.rs · handle_msg ─────────────────────────┐
│ 分流：group_router.route(to, msg)                  │
│   ├ 群 → true（扇出路径接管）                      │
│   └ 非群 → false → 单聊直投                        │
└──────────────────────────────────────────────────┘
  ▼ GroupRouter trait（依赖注入，阶段 3 预埋的接口）
┌─ web/fanout.rs · GroupHub ────────────────────────┐
│ Mutex<HashMap<group_id, GroupActor>>               │
│   ├ 命中 → actor.tx.send(Fanout{msg}).await        │
│   └ 未命中 → MemberSource.list_members 装载快照    │
│              → 孵化 actor task → 投递              │
└──────────────────────────────────────────────────┘
  ▼ mpsc 收件箱（容量 256：反压传导给发送者）
┌─ 单群 actor task（每群一个）───────────────────────┐
│ members: Vec<u64>   ← 快照（Invalidate 置脏重载） │
│ for member in members:                             │
│   if member == msg.from: continue  ← 不回显发送者  │
│   sessions.fanout_one(member, &msg)                │
│     → Delivered / Skipped(慢) / Offline(离线)      │
└──────────────────────────────────────────────────┘
```

三个注入点撑起这张图（都在装配层 `AppState::new` 闭合）：

- `Sessions::set_group_router(hub)`——会话核心认识「群」的唯一方式；
- `GroupHub::new(groups, sessions)`——扇出中枢认识「投递面」的唯一方式；
- `MemberSource for GroupStore`——扇出中枢认识「成员表在 DB」的唯一方式。

循环依赖（hub 需要 sessions、sessions 需要 hub）在 setter 上闭合——
装配层是唯一同时知道两边的地方（docs/06 的既有论证，本章受益）。

## 四、代码走读

### 4.1 依赖注入的预埋与兑现：`GroupRouter`

阶段 3 设计会话核心时刻意没让它认识「群」（核心不知道 DB 存在），
但预埋了一个问题口：`to` 是不是群？交给注入的 router 回答：

```rust
// crates/im-server/src/session.rs
pub trait GroupRouter: Send + Sync {
    fn route(&self, to: u64, msg: &Msg) -> RouteFuture<'_>;  // true = 已接管
}
```

阶段 7 之前它是「永远 false」的可选项；本章 `GroupHub` 实现了它。
**四个月前的接口，今天零改动兑现**——这是「依赖倒置」在时间维度上的
回报：好的抽象不是预测未来，而是把「未来的决定」推迟到最了解它的时候。

### 4.2 中枢：查表快、孵化慢、反压有语义

`GroupHub::route` 的快慢路径（`web/fanout.rs`）：

```rust
if let Some(actor) = self.actor(to) {          // 快路径：查表命中
    return self.enqueue(actor, &msg).await;    // 一定是群（表项只由「查到成员」创建）
}
let members = match self.source.list_members(to).await {  // 慢路径：首条消息
    Ok(members) if !members.is_empty() => members,        // 空 = 群不存在
    _ => return false,                                    // 失败回落单聊（降级不吞消息）
};
let actor = self.spawn_actor(to, members);
self.enqueue(actor, &msg).await
```

两个决策值得咀嚼：

- **空表 = 群不存在**：建群事务保证群主必在成员表（`INSERT` 群主与建群
  同一事务），所以「查到空」不需要区分「群不存在」和「群刚建好还没
  人」——不变式替掉了分支；
- **装载失败回落单聊投递**：DB 抖动时消息会走 `deliver` 落进离线队列，
  而不是被吞掉。可用性降级永远优于静默丢失。

`enqueue` 用**阻塞 `send`** 而非 `try_send`——注意这与扇出内部的
`try_send` 是两个决策：actor 收件箱打满说明它卡在快照重载（DB）上，
此刻让**发送者等**（反压传导给这一条连接）也比丢掉一条将要被 Ack 的
消息好。「不丢已接管的消息」优先级高于「发送者永远不被阻塞」；
而慢消费者问题发生在 actor 的**下游**（成员的写队列），那里才轮到
`try_send` 跳过——**反压和丢弃各有各的正确位置**。

### 4.3 actor：单 task、快照、脏标记

```rust
async fn actor_loop(source, sessions, group_id, mut members, mut rx, stats) {
    let mut dirty = false;                     // 写时失效的标记
    while let Some(job) = rx.recv().await {
        match job {
            Job::Invalidate => dirty = true,   // 只标记，不立即重载
            Job::Fanout { msg } => {
                if dirty {
                    if let Ok(fresh) = source.list_members(group_id).await {
                        members = fresh;       // 重载失败保旧快照（取舍见 §七）
                    }
                    dirty = false;
                }
                for &member in &members {
                    if member == msg.from { continue; }        // 不回显发送者
                    match sessions.fanout_one(member, &msg) {  // 同步快路径，无 await
                        FanoutOutcome::Delivered => …,
                        FanoutOutcome::Skipped   => …,          // 慢消费者，跳过
                        FanoutOutcome::Offline   => …,          // 离线，落离线队列
                    }
                }
            }
        }
    }
}
```

- **写时失效（invalidate-on-write）**：REST 加人后 `hub.invalidate(group_id)`
  只发一个标记，**下条消息到达才重载**。大多数加人操作后并没有消息
  紧跟，白查一次库不划算——懒惰是缓存一致性的合法策略；
- **扇出循环无一处 `await`**：`fanout_one` 是同步函数（见 4.4），
  一口气扇完全体成员。这顺带保证了群内投递顺序（actor 单线程 +
  循环内无挂起点 = 原子性），也是 2 万人单条 19ms 的基础；
- **发送者跳过**：单聊路径本就不回显给自己（前端乐观插入已在），
  群路径保持同一语义——`msg.from` 在成员表里也不投给自己。

### 4.4 `fanout_one`：同步快路径与三路结果

```rust
// crates/im-server/src/session.rs
pub fn fanout_one(&self, recipient: u64, msg: &Msg) -> FanoutOutcome {
    // 查路由表 → 有连接 → sink.try_send 帧
    //   Ok(())        → Delivered
    //   Err(Full)     → Skipped   ← 慢消费者：跳过，绝不挂起
    //   Err(Closed)   → 退回 send().await（慢速路径）→ Offline（离线降级）
    // 无连接          → Offline
}
```

为什么 `try_send` 满即跳过是对的：成员的写队列（容量 64）打满说明
这个客户端已经落后了一整队列的消息——再排一条也追不上，而挂起等待
会让**全群**为一个人排队（2 万人里一个卡住 = 每条消息慢一个人的时间）。
Telegram 群消息「在线实时、离线不补」的语义与此同构：**群消息的
实时性是尽力而为的，可靠性让位于活性**（取舍账单见 §七）。

`FrameSink::try_send`（阶段 5 抽的 trait 方法，当时只有测试用）在
本章找到了它的生产消费者——又一个「接口先行、兑现靠后」的案例。

### 4.5 `MemberSource`：为压测而生的第三块拼图

压测要驱动**真实扇出引擎**（GroupHub + actor + fanout_one 全是生产
代码），但 2 万个成员塞进 PG 再每次查全表，测的就成了 DB 不是扇出。
解法与 `GroupRouter` 同一手法——把「成员表在哪」抽象成 trait：

```rust
pub trait MemberSource: Send + Sync {
    fn list_members(&self, group_id: u64) -> MemberList<'_>;
}
pub type MemberList<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<u64>, GroupError>> + Send + 'a>>;

impl MemberSource for GroupStore { /* DB 实现：仓储固有方法装箱进 trait */ }
// im-bench 的 BenchSource { members: Vec<u64> }：内存实现
```

关键在**测量口径**：热路径本来就不碰成员源（快照只装载一次），
替换来源不影响被测对象——压测的合法性不是「环境逼真」而是
「被测路径与生产同构、干扰因素被显式排除」。

### 4.6 门槛：为什么不用 actor 快照做成员校验

`ws.rs::dispatch_inbound` 的发送门槛（阶段 6 单聊 + 阶段 7 群聊）：

```text
msg 上行 → to 是群？
   ├ 是 → is_member(to, 发送者)？ ─ 否 → error(not_member) 连接不断
   └ 否 → is_friend(发送者, to)？ ─ 否 → error(not_friend) 连接不断
```

明知 actor 里有成员快照，为什么每条消息仍然查一次库？因为门槛和
扇出的**不变式不同**：

- 快照只保证「actor 已孵化的群」的**投递范围**，未孵化的群没有快照；
- 门槛要防「知道群 ID 就能发言」——它必须对**任何**群 ID 成立，
  且 REST 建群/加人与 actor 孵化之间没有时序保证；
- 快照是扇出的私产，把它借给鉴权等于**让缓存的失效策略成为安全边界**。

所以门槛走 `GroupStore::is_member` 的主键点查（微秒级），扇出走快照——
**同一份数据，两个消费者，两种一致性要求**，分开是对的。这也回答了
docs/12 §4.8 留下的问题：阶段 7 没有把门槛点查「优化掉」，而是把它
和扇出路径彻底分开——优化债的偿还方式是分层，不是缓存一处了事。

### 4.7 前端：群的「零新增收发」与三块补丁

chat store 的群会话收发**没有新增一行发送代码**——`conversations`
把群与好友统一成 `Conversation{id, kind, name}`，`ingestIncoming`
按「`to` 是不是我加入的群」归档，`sendBody` 的 `to` 天然兼容群 ID。
阶段 5 的这点先见让群聊前端的全部工作量落在**管理界面**：

- `GroupsView.vue`：建群 / 我的群（角色标签）/ 群详情（成员列表 +
  群主拉人——复用加好友的按用户名精确查找）；
- 群消息发送者名字：群内 `from` 是不同成员，`ChatWindow` 在气泡上方
  标显示名（`chat.memberName`：成员缓存反查 → 好友表 → ID 尾号降级）；
- `http.ts` 顺手修了一个潜伏 bug：后端动作型接口（接受好友/拉人）返回
  **200 空体**，`api()` 无条件 `resp.json()` 会抛 SyntaxError——
  先读文本再选择性解析。

## 五、设计模式实战（对照 roadmap 4.5）

| 模式 | 在本阶段的形态 |
|---|
| **Actor 模型（第三次）** | 每群一个扇出 task：单线程独占快照、channel 通信、无锁。与前两次（连接读写 task）的区别：它守护的是**领域状态**（成员表）而非连接状态 |
| **写时失效（invalidate-on-write）** | 缓存一致性策略：DB 是真相源，写路径（REST 加人）打脏标记，读路径（actor 扇出前）重载。对照 Java：Spring Cache 的 `@CacheEvict`、CPU 缓存一致性协议（MESI）的脏行思想 |
| **依赖倒置（再一次）** | `MemberSource`：扇出中枢对「成员表在哪」零假设。项目里这是第三次同型抽取（FrameSink → GroupRouter → MemberSource），模式的手感来自重复 |
| **接口隔离** | hub 实现 `GroupRouter` 只暴露 `route` 给会话核心；`invalidate`/`stats` 是 web 层的私交——能力按消费者最小化 |
| **计数对账（测试模式）** | `fanned × 成员数 = delivered + skipped + offline`（发送者非成员的干净口径）——不变式进测试，扇出 bug 无处藏身 |
| **脏标记 + 惰性求值** | `Invalidate` 只置位、扇出前才重载：把「写代价」从每次加人摊薄到「加人且有下条消息」的组合事件上 |

## 六、压测：im-bench group-fanout

### 6.1 口径设计：测什么、不测什么

| 排除项 | 排除方式 | 为什么合法 |
|---|---|---|
| 网络 | `BenchSink` = 内存 mpsc 直出 | 与生产「有界 mpsc + try_send」同构，排除的只是 TCP 拥塞与序列化 |
| DB | `BenchSource` 内存成员源 | 热路径本来不查库（快照只装载一次），排除的不在被测路径上 |
| 握手/鉴权/去重 | 不走 `handle_msg`，直接 `hub.route` | 被测对象是扇出引擎本身；网关路径阶段 10 有独立场景 |

测量两组指标：

- **单条扇出延迟**：逐条发、逐条等完成（`GroupStats` 原子计数到达
  预期值），30 次采样给分位数。等待用 `yield_now` 忙等而非 `sleep(1ms)`
  ——毫秒级测量里 1ms 的睡眠会污染样本；
- **持续扇出吞吐**：200 条背靠背灌入收件箱（`route` 入队即返回），
  记总耗时换算「人次/秒」。

账目纪律：发送者 ID 刻意**不是成员**，于是
`fanned × 成员数 = delivered + skipped + offline` 必须精确成立——
`report` 里对不上直接报错，压测结果作废。**带对账的压测才可信**。

慢消费者的语义纪律：压测中慢消费者的接收端必须**保活到测量结束**——
提前 drop 会让 sink 报 `Closed` 走「离线降级」，那测的是另一回事。
慢消费者的定义是「活着但永不读」。

### 6.2 用法

```powershell
cargo run -p im-bench --release -- group-fanout                      # 2 万人默认
cargo run -p im-bench --release -- group-fanout --slow 200           # + 200 慢消费者
cargo run -p im-bench --release -- group-fanout --members 2000       # 2 千人对照
cargo run -p im-bench --release -- group-fanout --members 100000     # 10 万人对照
# 完整参数：--members 20000 --messages 200 --samples 30 --slow 0 --capacity 64 --payload 64
```

### 6.3 原始数据（2026-09，Windows 11 · 16 逻辑核 · release 构建 · 内存 channel）

**场景 A：2 万人基线**（`--members 20000`，默认参数）

```text
装配            : 20,000 会话 + 排空 task，耗时 30.40ms
单条扇出延迟（30 次逐条采样）：min 17.98ms   P50 18.91ms   P90 20.67ms   P99 24.81ms   max 25.10ms
持续扇出（200 条背靠背）    ：消息吞吐 47 条/秒 · 成员投递 940,000 人次/秒
计数对账        : fanned=231 delivered=4,620,000 skipped=0 offline=0
```

**场景 B：2 万人 + 200 慢消费者**（`--members 20000 --slow 200`）

```text
单条扇出延迟：P50 18.51ms   P90 20.24ms   P99 24.54ms   （与基线几乎重合）
计数对账    : fanned=231 delivered=4,574,000 skipped=46,000 offline=0
慢消费者隔离: skipped=46,000（各收首条后全部跳过；4,574,000 + 46,000 = 4,620,000，对账通过）
```

**场景 C：2 千人对照**（`--members 2000`）

```text
单条扇出延迟：P50 1.45ms
持续扇出    ：1,226,000 人次/秒
```

**场景 D：10 万人对照**（`--members 100000`）

```text
单条扇出延迟：P50 99.12ms
持续扇出    ：900,000 人次/秒
```

### 6.4 结论

- **人均成本约 1µs**：2 万人 P50 18.91ms ÷ 2 万 ≈ 0.95µs/人，2 千人
  1.45ms ÷ 2 千 ≈ 0.72µs/人——`try_send` + 内存 channel 的直给数字，
  覆盖查表、克隆、入队、原子计数全程；
- **延迟随人数线性扩展**：2 千 → 2 万（×10 人，P50 ×13）、
  2 万 → 10 万（×5 人，P50 ×5.2），无线性放大因子——单 actor 串行
  扇出没有「越多人越亏」的病态；
- **持续扇出的天花板 ≈ 90~120 万人次/秒**：三点的吞吐都落在同一
  带宽（94 万 / 122 万 / 90 万），说明瓶颈是**单个 actor task 的串行
  处理**（一个逻辑核的 try_send 循环），不是人数——多群天然并行
  （每群独立 actor），单群再要快就得上扇出分片（阶段 10 候选项）；
- **慢消费者隔离有效且零代价**：200 个慢消费者在场，P50 18.51ms
  与基线 18.91ms 在噪声范围内——`try_send` 跳过比 `send` 挂起**便宜**，
  这正是隔离策略的设计承诺；
- **参照系**：这个数字是「纯扇出引擎」的。生产路径还要加 JSON 编解码
  与 TCP/WS 写出（阶段 10 的端到端场景会给出含网络的全链路数字）。
  排除干扰不是作弊，把「引擎几斤几两」和「路有多堵」分开量才是方法。

## 七、已知取舍（诚实的账单）

- **actor 不退役**：群冷了 actor 也常驻（一个 mpsc + 一个 Vec，空闲
  成本 ≈ 0）。无限多群的 LRU 回收留给阶段 10 工程化——先不解决
  没被证明存在的问题；
- **快照重载失败保旧**：可用性优先。空快照 = 全群吞消息，比「暂时
  多/少一个成员」贵得多；
- **`Skipped` 的消息不可恢复**：群消息目前不持久化（离线队列只暂存
  离线成员的单聊投递），慢消费者本轮丢失。业界同类取舍：Telegram
  超大群在线消息不补投。补齐需要「群消息持久化 + 同步游标」，
  是阶段 10 前的明确欠账；
- **门槛只在 WS 路径**：`not_member` 校验在 `web::ws::dispatch_inbound`，
  TCP 路径（TUI）的群消息没有成员门槛——TUI 无群管理 UI，知道群 ID
  的攻击面小，但账要记：门槛上移到 `handle_msg`（会话核心层）是
  阶段 12 的工程化项；
- **群管理只有建群/拉人**：退群、踢人、转让群主、@提及、群内递增
  seq 未实现——REST/WS 协议（docs/12）扩 `kind` 即可承载，属产品
  完整性欠账而非架构欠账。

## 八、测试策略

| 层 | 用例 | 要点 |
|---|---|---|
| fanout 单元（真 PG） | 主线扇出：成员收到、发送者零回显、载荷 `to` 保持群 ID | 回显语义与单聊对齐 |
| fanout 单元（真 PG） | 快照失效：加人 + `invalidate` 后新成员下条消息即达 | 写时失效的读路径验证 |
| fanout 单元（真 PG） | 非群 ID 回落单聊、不孵化 actor | `route` 契约的 false 分支 |
| fanout 单元（真 PG） | 慢消费者隔离：slow 收 1 条后全跳，fast 三条完整且有序 | **隔离的完整语义** |
| fanout 单元（无 DB） | `MemSource` + `with_source` 装配：不连库孵化并扇出 | trait 装配的守护测试 |
| 会话核心 | `fanout_one` 三态：Delivered / Skipped / Offline | outcome 枚举的穷举 |
| ws 集成 | 成员发群消息全员收到；非成员 `not_member` 带关联 `client_msg_id` 且不断连 | 门槛 + 错误关联 |
| im-bench 单元 | 千分位/分位数格式化、`BenchSource` 端到端 | 压测工具自身的正确性 |
| 前端 | `vue-tsc` + `vite build` | 类型即测试 |

## 九、开发中踩过的坑

- **慢消费者的保活**：测试里把慢消费者的接收端 `rx` drop 了，sink
  立刻报 `Closed` 走「离线降级」——计数全对，但测的是另一回事。
  慢消费者的定义是「活着但永不读」，`rx` 必须进保活袋、测量结束才
  drop。**测试数据的正确性依赖被测对象的语义精确**；
- **`trait` 方法不在作用域**：`im-bench` 调 `hub.route(...)` 编译报
  E0599——`route` 是 `GroupRouter` 的 trait 方法，不 `use` 进来就
  不可见。依赖注入的配套纪律：**调用方必须显式认识契约**，这是
  trait 与固有方法的可见性差异，不是编译器刁难；
- **装箱 future 的生命周期**：`MemberSource::list_members` 返回
  `MemberList<'_>`（`Pin<Box<dyn Future>>`），`impl MemberSource for
  GroupStore` 里用 `Box::pin(GroupStore::list_members(self, group_id))`
  把固有 async 方法摆进 trait——与 `GroupRouter` 的 `RouteFuture`
  同一换法。**async trait 方法在 Rust 里的标准姿势**（dyn 兼容版）。

## 十、下一步（阶段 8 预告）

社交文本链路至此完整（单聊 + 好友 + 群聊 + 富媒体）。阶段 8 进入
实时媒体：1对1 音视频通话与远程桌面（WebRTC P2P）——

- WS 信令信封扩展（offer/answer/ICE candidate 交换），复用 docs/12
  的事件信封形态；
- WebRTC 数据通道与远程桌面：P2P 直连后服务端只是信令中转——
  与本章的对照会非常有趣：**消息扇出走服务端（需要可靠性），
  媒体流走 P2P（需要低延迟）**，两种拓扑各服务各的语义；
- Vue 通话 UI（呼叫/接听/挂断 + 桌面捕获）。

## 十一、面试题与标准回答

**Q1：群消息为什么要每群一个 actor，而不是一个全局扇出线程或每成员一个 task？**

答：三个方案各有一个致命伤。全局线程：所有群共享一个串行点，
一个热闹群把所有群的延迟拉高（队头阻塞的群际版本）；每成员一个
task：2 万人 = 每条消息 2 万次跨 task 唤醒，调度开销吃掉吞吐，且
无法保证「所有成员看到同一顺序」；每群一个 actor 是中庸的甜点——
群内串行（顺序性免费）、群间并行（隔离免费）、task 数量 = 活跃群数
而不是成员数。选型的本质是**寻找串行化的最小必要范围**：顺序性
要求串行，那就把串行的范围压缩到恰好覆盖一个群。

**Q2：慢消费者你们是跳过消息，丢消息不是 bug 吗？**

答：是取舍，不是疏忽。备选方案是挂起等待（全群为一个人排队，
2 万人群里一个人卡住 = 每条消息慢一个人的恢复时间）或断开慢消费者
（体验灾难且重连风暴）。跳过的代价是「落后一队列的客户端反正也
追不上」，这是 Telegram 超大群「在线实时、离线不补」的同款语义：
**群消息的实时性是尽力而为的**。工程上关键的是把取舍做成显式的、
可观测的：`Skipped` 进原子计数、压测对账必查、账单写进文档——
「知道自己丢了什么」和「不知道丢了什么」是两种完全不同的系统。

**Q3：成员快照会不会有一致性问题？加人之后下条消息才生效，这算延迟生效吗？**

答：算，而且这个延迟是**有界的、一次性的**：REST 加人先改库
（真相源）再 `invalidate`（打脏标记），actor 在下条消息前重载——
最坏情况是「加人之后、下一条消息之前」这个窗口里新成员收不到，
但业务上不存在这个窗口的观测者（新成员自己也没在发消息）。真正的
一致性风险不在标记本身，而在**重载失败**：此时保旧快照，宁可
暂时投递范围不准也不空投（空快照 = 全群吞消息）。缓存一致性的
正确姿势不是消灭窗口，而是给每个窗口配上明确的降级语义。

**Q4：压测不连 DB、不走网络，这样的数据有意义吗？**

答：有，但意义是分层的。被测对象是**扇出引擎**（hub/actor/
fanout_one，全部生产代码），而热路径本来就不查库（快照只装载
一次）、sink 与生产同构（有界 mpsc + try_send）——排除项没有
一个落在被测路径上，这叫「同构替换」而不是「作弊」。它回答的
问题是「引擎本身几斤几两」（人均 ~1µs、单 actor 天花板 ~90 万
人次/秒）；「生产端到端多少」是另一个问题，由阶段 10 的含网络
场景回答。**一次压测只回答一个问题**——想一次测完所有因素的
压测，最后什么也说不清。

**Q5：门槛校验每条消息查一次库，你明明有成员快照，为什么不用？**

答：因为快照的一致性契约不覆盖门槛的需求。快照只存在于「actor
已孵化」的群，且它的失效策略是惰性的（下条消息前重载）；而门槛
要防「知道群 ID 就能发言」，必须对任何群 ID、任何时刻成立——
把缓存借给鉴权，等于把缓存的失效 bug 升级成安全漏洞。同一份
数据（成员表），扇出消费「尽力而为的投递范围」（可用缓存），
门槛消费「权限判断」（必须强一致），**数据消费方决定一致性级别**，
这是我在这个项目里学到的最值钱的一条架构判断。

---

*阶段 7 完成于：群扇出 actor（每群一 task + 成员快照写时失效）、
`try_send` 慢消费者隔离（Skipped 计数 + 压测验证零代价）、
`MemberSource` 依赖倒置（压测驱动真实引擎）、`not_member` 发送门槛、
im-bench group-fanout（2千/2万/10万三点实测：人均 ~1µs，单 actor
天花板 ~90-120 万人次/秒）、Vue 群聊界面（建群/拉人/群会话/发送者
名字）；workspace 测试全绿，clippy pedantic 零警告。*
