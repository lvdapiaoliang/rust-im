# 07 - 阶段 4：客户端——消息可靠性、本地库与 TUI

> 对应代码：`crates/im-storage/`（存储引擎）、`crates/im-client/`（重传邮箱 /
> 去重 / reducer / TUI）、`im-protocol`（`client_msg_id`）
> ｜ 前置阅读：`docs/05-network-tokio.md`（重连/退避）、`docs/06-server-arch.md`（服务端视角的可靠性）
> ｜ 学习配套：`learning-rust-from-scratch/`（BTreeMap、trait、状态机）

## 一、这一层解决什么问题

阶段 3 结束时，客户端「能聊」，但可靠性承诺有一大片空白：

| 问题 | 阶段 3 的窟窿 | 阶段 4 的答案 |
|---|---|---|
| 消息发出后 Ack 丢失怎么办？ | 无重传——消息静默丢失 | **Outbox 重传邮箱**：指数退避重发直到 Ack |
| 客户端进程崩溃，在途消息呢？ | 无本地状态——全丢 | **持久化重发表**：重启恢复、重连补发 |
| 聊天历史存哪？ | 内存——关了就没 | **本地消息库**：自研简化版 LSM 引擎 |
| 断线期间漏掉的消息？ | 只有服务端离线队列兜底 | **同步游标持久化**：重启只补增量 |
| 重发必然带来重复，接收方怎么办？ | 无去重 | **`client_msg_id` 去重窗口** |
| UI 逻辑和协议逻辑纠缠？ | CLI 打印散在 main | **ChatState reducer + 事件流**（UI 零业务逻辑） |

一条主线贯穿全章：**「至少一次」发送 + 接收端去重 = 「恰好一次」的用户体验**。
这句话是所有消息系统的可靠性公式（Kafka、RocketMQ、微信同步协议全都如此），
本阶段把它从口号变成代码。

## 二、模块地图

```text
im-protocol  msg.rs         Msg 增加 client_msg_id（发送方本地去重键，服务端透传）
im-storage   engine.rs      简化版 LSM：追加段 + memtable + seal/compact + CRC 坏尾恢复
             store.rs       LocalStore 语义层：消息历史 / 同步游标 / pending 重发表
im-client    outbox.rs      重传邮箱：RTO 指数退避 + Ack 核销 + 上限放弃
             dedup.rs       接收去重窗口：HashSet + VecDeque FIFO 逐出
             chat.rs        ChatState reducer：事件流 → UI 状态的纯函数
             tui.rs         ratatui 三栏界面：渲染状态 + 按键翻译，零业务逻辑
             client.rs      连接状态机扩展：LocalState 聚合（store/outbox/dedup/cursor）
```

依赖方向没有变化：`im-client` 组合 `im-transport`（连接）与 `im-storage`（本地库），
二者互不认识——**组合而非纠缠**。

## 三、本地消息库：为什么要自己写一个 LSM

> roadmap 4.5：**B+ 树 / LSM 思想——本地消息库索引、写前日志（理解 SQLite/RocksDB 原理）**

### 3.1 写多读少的负载画像

聊天客户端的本地负载：**消息持续追加写入，偶尔翻历史（读）**。
`SQLite`（B+ 树）读优化、原地更新；RocksDB/LevelDB（LSM）写优化、追加 + 压实。
IM 客户端天然适合 LSM——这正是「为什么微信/Telegram 的本地库都是 KV 形态」的答案。

### 3.2 引擎的四个动作

```text
写 put/delete ──▶ 追加到活跃段（WAL 语义）+ memtable（BTreeMap）
                     │
     活跃段达到条数阈值 └─▶ seal：封存为只读段（建 HashMap 索引），开新段

读 get ──▶ memtable ──▶ 封存段索引从新到旧（HashMap 定位 + pread）
扫描 scan ──▶ 全部来源按版本序灌进 BTreeMap（后写覆盖先写），range 输出
压实 compact ──▶ 同 scan 收集，顺序写成一个有序新段（无 tombstone、无旧版本）
```

与生产 RocksDB 的刻意差距（教学取舍，逐一在代码注释里标注）：
没有 SST 块压缩与布隆过滤器；scan 是全量收集而非 k 路归并；
单 memtable 无 Immutable MemTable 层级；fsync 简化为每写必 flush
（进程崩溃不丢，掉电可能丢——本地聊天记录对这个级别的持久性足够，
换来每条消息微秒级落盘）。

### 3.3 WAL 的崩溃恢复契约

追加写的段文件在进程崩溃时最坏情况是**尾部半条记录**。打开时逐条
CRC 校验重放，遇到第一个坏记录即截断其后所有内容：

> 已确认写入的数据不丢，未完成的写丢弃——这正是数据库 WAL 的契约。

帧格式 `len:u32 + crc32:u32 + record` 的定长头让恢复扫描免于 varint
对齐歧义；CRC 复用 `im_protocol::crc32`（查表法，阶段 1 的代码第二次服役）。

### 3.4 key 设计：零填充定宽编码

```text
m/{peer}/{msg_id:020}    正式消息（按会话前缀，msg_id 零填充 20 位）
p/{client_msg_id:020}    发送中的重发表条目
c/sync_cursor            离线同步游标
c/client_seq             client_msg_id 分配计数器
```

`{msg_id:020}` 零填充是关键技巧：**字典序 = 数值序**，BTreeMap 的
range 扫描直接就是「按时间升序取最近 N 条」。对照 Java：如果用
`String.format("%020d")` 拼进 SQLite 主键，是同一个思想——
让排序语义在编码层就正确。

### 3.5 版本序覆盖：省掉时间戳

同一 key 多次写，读时「最新的赢」。判定新旧靠**遍历顺序**
（段代数递增、段内偏移递增）而非每条记录存 8 字节时间戳——
LSM 的免费午餐：追加顺序天然就是写入时间序。

## 四、发送侧：Outbox 重传邮箱

### 4.1 为什么 TCP 可靠不够

TCP 的可靠只到内核缓冲区：消息「写成功」不代表对端收到，对端收到
不代表 Ack 回得来。「至少一次」要求每条消息在**本地持久化的重发表**
里待到被 Ack 核销为止。这是 TCP 自身超时重传机制在应用层的翻版——
端到端原则：连接层的可靠性承诺覆盖不了「服务端已收到但 Ack 丢失」的窗口。

### 4.2 状态机

```text
enqueue ──发送──▶ [等 Ack，RTO 计时中]
                     │ 超时：attempts+1，指数退避后重发
                     ├─ Ack 到达：核销转正（pending → 正式消息）
                     └─ attempts 达上限：放弃，上报 SendFailed
断线：RTO 冻结；重连后 resends() 全量补发（RTO 重置——新连接新起点）
```

三个值得展开的取舍：

- **退避公式** `rto × 2^(attempts-1)`，封顶 30s，移位上限 16 防溢出。
  刻意**不加抖动**（连接级重连用 full jitter 防风暴）：消息级重传
  在同一连接内逐条独立计时，风暴规模受在途数限制，且确定性退避
  让超时行为可测试——**可测性本身也是设计约束**；
- **线性表而非堆**：在途消息量级是「用户手速」（个位数），O(n) 扫描
  比 `BinaryHeap`（删除 O(n)）更简单且常数更小——数据结构选型跟着
  量级走，不跟「高级感」走；
- **上限放弃**：「至少一次」不等于「无限重」。无限重发会拖垮客户端
  与服务端；达到上限后把决定权交回业务层（重发/放弃/提示），
  `SendFailed` 事件让 UI 显示红色感叹号。

### 4.3 Ack 核销与「转正」

Ack 到达 → 从重发表落盘删除 → 写入正式消息（挂在与 `to` 的会话里）。
核销键是 `client_msg_id`——**跨重发稳定**（重发会换新 `msg_id`，
服务端每次都裁决新全局 ID，只有客户端本地键不变）。迟到的重复 Ack
是幂等 no-op。

## 五、接收侧：去重窗口 + 先落盘再上抛

### 5.1 去重键为什么是 client_msg_id

发送端「至少一次」的代价是接收端会看到重复：同一条消息可能经
实时投递、离线补投、重发投递三条路径各到一次。服务端 `msg_id`
在重发后会换新值，**不能**当去重键；只有发送方生成的
`client_msg_id` 跨重发稳定——这就是 p4-1 给协议加这个字段的原因。

### 5.2 有界去重窗口

`HashSet`（O(1) 查重）+ `VecDeque`（FIFO 逐出）：窗口满了淘汰最旧的。
带时间戳的 LRU 是过度设计——重复的时效性由重传 RTO（秒级）决定，
1024 条的 FIFO 足以覆盖任何现实的重传窗口。

### 5.3 先落盘，再上抛 UI

```rust
fn ingest(&mut self, msg: Msg) -> Result<Option<Msg>, StorageError> {
    if !self.dedup.admit((msg.from, msg.client_msg_id)) {
        return Ok(None);          // 重复：静默丢弃
    }
    self.store.append_incoming(&msg)?;   // 先落盘
    if msg.msg_id > self.cursor {
        self.cursor = msg.msg_id;        // 游标推进
        self.store.set_sync_cursor(self.cursor)?;
    }
    Ok(Some(msg))                 // 再上抛 UI
}
```

顺序是刻意的：崩溃宁可重收（去重窗口 + 服务端补投兜底），
不可丢——**「先写日志再变更状态」的 WAL 思想第三次出现**。
（第一次：引擎的追加写；第二次：Outbox 先持久化再发送。）

## 六、ChatState reducer 与 TUI

### 6.1 UI 状态 = fold(事件流)

```text
ClientEvent 流 ──▶ ChatState::on_event(纯函数) ──▶ ChatState ──▶ 渲染
按键            ──▶ ClientHandle 命令 / 视口状态
```

Redux 风格的 reducer：任何时刻的状态完全由「事件序列 + 初始状态」决定。
收益：**UI 无状态**——TUI 只做两件事（渲染状态、把按键翻译成命令），
渲染逻辑零分支噪声；**可测试**——测试 reducer 不需要起服务端/客户端/
定时器，「喂事件序列、断言状态」就是全部。

reducer 内部还有一层防御性幂等（同 `(from, client_msg_id)` 只记一次）：
事件流上游已有去重窗口，reducer 再兜底一层——**reducer 幂等 =
事件重放永远安全**，这是事件溯源风格的前提。

### 6.2 出站消息的三态渲染

`MessageQueued`（转圈）→ `Ack`（送达 ✓）或 `SendFailed`（失败 ✗）。
核销索引同样是 `client_msg_id`——从协议层到 UI 层，这个键贯穿始终。

### 6.3 ratatui 三栏

```text
┌────────┬──────────────────────┐
│ 会话    │ 消息区（当前会话）      │
│ 列表    │  ▶ 我发的（已送达）     │
│        │  ~ 我发的（发送中）     │
│        │  ✗ 我发的（失败）       │
│        │  < 对方发的             │
│        ├──────────────────────┤
│        │ > 输入框               │
└────────┴──────────────────────┘
```

按键极简：`Tab` 切会话、`/to <id>` 新会话、Enter 发送、
PageUp/Down 翻历史、`/quit` 退出。crossterm 的 `EventStream`
（async 事件流）让按键等待直接进 `tokio::select!`——
与客户端事件流同台 select，没有轮询。

## 七、算法 / 数据结构 / 设计模式落点（对照 roadmap 4.5）

| 落点 | 实战内容 |
|---|---|
| B+ 树 / LSM 思想 | `engine.rs`：追加段 + memtable + seal/compact，对照 RocksDB 讲差距 |
| WAL（写前日志） | 引擎追加写、Outbox 先持久化再发送、接收先落盘再上抛——三处同一思想 |
| CRC32 + 截断恢复 | 复用阶段 1 的查表法；坏尾丢弃的崩溃恢复语义 |
| 零填充定宽编码 | key 编码让 BTreeMap 字典序 = 时间序，range 即分页 |
| 指数退避（无抖动版） | `outbox.rs`：RTO × 2^n 封顶 30s，可测性优于防风暴 |
| HashSet + VecDeque | 有界去重窗口：O(1) 查重 + FIFO 逐出 |
| 线性表 vs 堆的选型 | 在途消息量级分析后放弃 BinaryHeap——选型跟量级走 |
| reducer 模式 | `chat.rs`：事件驱动纯状态机（事件溯源的内存版） |
| 观察者（通道版） | `ClientEvent` 流贯穿：连接状态机 → reducer → TUI |
| 借用式 API 设计 | Outbox 不持有 LocalStore，每次调用传 `&mut`——收发两侧共用一个库实例 |

## 八、测试策略

| 层 | 用例群 | 数量 |
|---|---|---|
| im-storage | 坏尾截断 / 覆盖语义 / seal+compact / 重启恢复 / key 编码 | 13 单元 + doctest |
| im-client | Outbox 生命周期（核销/重复 Ack/退避/上限放弃/重启恢复）| 6 单元 |
|  | 去重窗口（首见放行/重复拦截/满窗逐出） | 2 单元 |
|  | ChatState reducer（出站三态/入站幂等/批量入库） | 4 单元 |
|  | TUI 输入解析（命令/内容/忽略）+ 会话轮转 | 4 单元 |
|  | 连接状态机：互发 / 离线补投 / 闪断 / 被拒 / 排队 / Ack 丢失重传 / 事件顺序 / 落盘验证 | 8 单元 |
| e2e 集成 | 三幕故事线 + **崩溃重启重传**（持久化重发表闭环） | `tests/e2e.rs` × 2 |

`crashed_client_resends_persisted_outbox_on_restart` 是本阶段的集大成场景：
Bob 发消息后不等 Ack「进程崩溃」→ 同一本地库重启 → 重连自动补发 →
Alice 去重后体验上恰好收到一次 → 直接开 Bob 的本地库验收终态
（pending 必空、消息转正入历史）。断言方式是「绕过被测对象直接查库」，
防止客户端自说自话。

## 九、开发中踩过的坑

- **`client_msg_id` 撞车（本阶段最深刻的 bug）**：e2e 测试的脚手架给
  每次上线的客户端分配全新临时目录——Bob 重连后本地计数器归零，
  新消息的 `client_msg_id=1` 撞上 Alice 去重窗口里 act 1 见过的
  `(from=2, client_msg_id=1)`，被当重复**静默丢弃**。表象是
  「Alice 死活收不到 Bob 的回复」，与消息丢失的表象完全相同。
  **教训一：去重键的分配器必须与存储同生命周期**（换库 = 计数器归零
  = 键复用 = 静默丢消息）。**教训二：静默丢弃的 bug 要靠「键的唯一性
  来源」审查，而不是断点能发现的**；
- **Windows 文件句柄竞争**：engine 的两个测试（坏尾截断、compact）
  偶发失败一次、重跑稳定全绿——Windows 下文件句柄释放与
  Defender 扫描的竞争。**教训：Windows 上做存储层测试，
  删除旧目录前留足句柄释放时间**；
- **`u32::try_from` 替代 `as` 截断转换**：帧长度头 `body.len() as u32`
  在 64 位平台是静默截断点，clippy（`cast_possible_truncation`）
  逼着改成 `try_from + expect`——**转换的失败语义必须显式**；
- **测试里的「双结局」断言**：制造 Ack 丢失时，Ack 可能刚好在杀连接前
  到达——「已核销」与「补发后核销」两种结局都合法。断言写成
  「终态收敛」（重发表必空）而不是「中间路径」（必须重发），
  竞态才不会把测试变成抽签。

## 十、下一步（阶段 5 预告）

客户端至此拿到了完整的可靠性闭环，但产品还只有命令行入口。
项目计划在此处重排：阶段 5~9 新增 Web 接入与社交功能，
原压测计划顺延为阶段 10。

阶段 5（Web 接入与持久化）先做传输解耦：抽取 `FrameSink` trait，
让 TCP 二进制路径与 Web WS 网关（JSON 信封协议）共用同一套
会话核心（路由 / 离线补投 / seq 去重 / 雪花 ID），并引入
PostgreSQL 持久化与 Vue 3 前端。本章的 Outbox/去重/游标
语义在 WS 路径原样生效——**传输可以翻译，语义必须复用**。
弱网模拟与压测（P99 < 10ms、10 万并发连接）推迟到阶段 10：
先有正确性，再谈性能。

---

*阶段 4 完成于：Outbox 重传邮箱、自研简化 LSM 本地库、ChatState reducer、
ratatui 三栏 TUI、崩溃重启重传 e2e；全 workspace 测试全绿，
clippy pedantic 零警告。*
