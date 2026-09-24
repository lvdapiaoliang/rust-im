# 12 - 阶段 5：Web 协议——REST、WS JSON 信封与双传输适配

> 对应代码：`crates/im-server/src/web/`（REST + WS 网关）、`crates/im-server/src/sink.rs`
> （FrameSink）、`web/`（Vue 前端）
> ｜ 前置阅读：`docs/04-protocol-design.md`（二进制协议）、`docs/06-server-arch.md`（会话核心）
> ｜ 学习配套：`learning-rust-from-scratch/`（trait、适配器模式）

## 一、这一章解决什么问题

阶段 0~4 的 IM 只有一个入口：TCP 长连接 + 二进制帧。浏览器进不来——
浏览器给不了原始 TCP，也发不了自定义二进制控制帧。阶段 5 打开第二入口：

| 问题 | 二进制路径的现状 | Web 路径的答案 |
|---|---|---|
| 浏览器怎么连？ | 原生 TCP，浏览器没有 | **WS 网关**（HTTP 升级，全双工文本/二进制帧） |
| 协议怎么表达？ | 定长头 + varint 二进制帧 | **JSON 信封** `{type, seq, ack, payload}` |
| 账号/好友/群组存哪？ | 内存（重启即失） | **PostgreSQL + sqlx**（迁移脚本进版本库） |
| 会话核心要重写吗？ | `serve_connection` 绑死 TcpStream | **FrameSink trait 抽取**：会话核心与传输解耦 |
| 雪花 ID 进 JS 会怎样？ | u64 没问题 | **超 `Number.MAX_SAFE_INTEGER`（2^53）静默丢精度**——全部串化 |
| 心跳怎么做？ | WS Ping/Pong 控制帧 | 浏览器**发不了**自定义控制帧——应用层 `ping`/`pong` 信封 |

一条主线贯穿全章：**传输可以翻译，语义必须复用**。路由、离线补投、
seq 去重、雪花 ID、单端登录——WS 路径与 TCP 路径走的是**同一套**
`SessionState` + `handle_frame`，只是帧的「皮肤」从二进制换成了 JSON。

## 二、总架构：双接入并存

```text
TUI 客户端（不变）                    浏览器（Vue 3 + TS）
    │ TCP + 二进制帧                    │ HTTPS REST（注册/登录/好友/群组/文件）
    ▼                                  │ WSS /ws?token=…（JSON 信封）
┌─────────────────┐                    ▼
│ im-transport    │         ┌─ im-server/src/web（axum）──────────┐
│ serve_connection│         │  REST API ──▶ sqlx ──▶ PostgreSQL   │
└────────┬────────┘         │  ws.rs：信封 ↔ Frame 翻译层          │
         │ Frame            │        │ Frame（同一格式）            │
         ▼                  └────────┼─────────────────────────────┘
  ┌───────────────────────────────────▼───────────────────────────┐
  │ 会话核心：SessionState::feed_seq（去重）+ handle_frame          │
  │ Sessions：分片路由 / 离线队列 / next_id（雪花，兼任 DB 发号器）  │
  └───────────────────────────────────────────────────────────────┘
```

两条路径在 `Frame` 这个类型上汇合。TCP 路径把字节流解码成 Frame；
WS 路径把 JSON 信封翻译成 Frame。**Frame 之后的世界完全共享**——
这就是阶段 5 前置重构（FrameSink 抽取）买来的架构自由度。

## 三、REST API（前缀 /api）

### 3.1 鉴权：Bearer 令牌

登录签发不透明令牌（DB 可撤销、带有效期），REST 与 WS 共用：

```text
Authorization: Bearer <token>
```

`AuthUser` 是一个 axum **提取器**：挂在处理器的非 body 参数上即完成
鉴权（提取器即中间件——对照 Java：相当于 Spring MVC 的
`@RequestHeader` 参数解析器 + HandlerInterceptor 合一，但零反射、
编译期类型安全）。

### 3.2 端点一览

| 方法与路径 | 用途 | 鉴权 |
|---|---|---|
| POST /api/register | 注册（argon2 哈希口令） | 无 |
| POST /api/login | 登录 → `{token, expires_in_secs, user}` | 无 |
| GET /api/me | 当前用户信息 | Bearer |
| POST /api/friends/requests | 发好友请求 | Bearer |
| GET /api/friends/requests | 收到/发出的请求列表 | Bearer |
| POST /api/friends/requests/{id}/accept | 接受 | Bearer |
| POST /api/friends/requests/{id}/reject | 拒绝 | Bearer |
| GET /api/friends | 好友列表 | Bearer |
| DELETE /api/friends/{user_id} | 删除好友 | Bearer |
| POST /api/groups | 建群（创建者为 owner） | Bearer |
| GET /api/groups | 我的群 | Bearer |
| POST /api/groups/{id}/members | 拉人入群（仅 owner） | Bearer |
| POST /api/files | 上传（multipart，落盘 `data/files/`） | Bearer |
| GET /api/files/{id} | 带鉴权下载 | Bearer |
| GET /ws?token=… | 升级为 WebSocket | query 令牌（见四） |

### 3.3 错误格式与状态码映射

所有错误统一 JSON：`{"error": "<人话消息>"}`。仓储错误 → HTTP 状态码
的映射集中在 `From<XxxError> for ApiError`（错误映射集中一处，
处理器里只剩 `?`——对照 Java：`@ControllerAdvice` 全局异常处理的
Rust 形态）：

| 业务错误 | HTTP |
|---|---|
| 用户名已占用 | 409 Conflict |
| 口令错 / 令牌无效 | 401 Unauthorized |
| 请求/用户/群/文件不存在 | 404 Not Found |
| 给自己发好友请求 | 400 Bad Request |
| 非群主拉人 | 403 Forbidden |
| 文件超限 | 413 Payload Too Large |
| 雪花发号器不可用 | 503 Service Unavailable |
| DB 故障 | 500 Internal Server Error |

### 3.4 ID 串化约定（全栈一致）

63 位雪花 ID 超出 JS `Number.MAX_SAFE_INTEGER`（2^53），数字形态
在前端会**静默丢精度**（Telegram 网页端同样用字符串 ID）。约定：

- **出站**：所有 ID 字段一律 JSON 字符串（`"id": "378622113893847040"`）；
- **入站**：宽容接受字符串或数字（`serde_id` 的 visitor 两种都认）——
  与服务端互操的脚本/工具不必先串化。

实现是 `web/mod.rs` 的 `serde_id` 模块（`serialize` + 宽容 `deserialize`），
各仓储 DTO 用 `#[serde(with = "serde_id")]` / `serialize_with` 挂上。

## 四、WS 协议：JSON 信封

### 4.1 连接与鉴权

```text
GET /ws?token=<登录令牌>
```

鉴权发生在 HTTP 升级**之前**：坏令牌直接 HTTP 401，连 WebSocket 都
不建立（省一次握手往返；客户端拿到标准状态码而非协议内错误）。
缺 token 参数同样 401。

注意：**鉴权通过 ≠ 注册成功**。升级后仍可能撞「单端登录」
（同账号已在线），那是 `welcome` 信封里说的事。

### 4.2 信封结构

每个 WS 文本帧是一条信封（协议只有文本帧，Binary 忽略）：

```json
{
  "type": "msg",          // 信封类型（协议的「动词」）
  "seq": 3,               // 客户端上行序号：去重窗口的输入，单调递增
  "ack": 0,               // 帧级累计确认（与二进制帧头语义一致，可缺省）
  "payload": { … }        // 类型专属载荷
}
```

- `seq`：客户端自己维护、每个连接从 1 递增。重发用**同一个 seq**，
  服务端去重窗口据此挡下重复（与 TCP 路径同一 `DedupWindow`、
  同一 `feed_seq` 裁决：`InOrder`/`OutOfOrder` 放行，`Duplicate`/
  `TooFar` 静默丢弃）；
- 下行信封的 `seq` 是服务端侧序号，`ack` 回显对端累计确认。

### 4.3 类型全表

| type | 方向 | payload | 时机 |
|---|---|---|---|
| `welcome` | 下行 | `{session_id, reason}` | 升级后第一条：注册成功（`session_id` 为会话雪花 ID）或拒绝原因 |
| `msg` | 上行 | `{to, client_msg_id, content}` | 发消息；`from`/`msg_id` 由服务端裁决（伪造无效） |
| `msg` | 下行 | `{from, to, msg_id, client_msg_id, content}` | 实时投递 |
| `msg_ack` | 下行 | `{msg_id, client_msg_id}` | 服务端已落路由/离线队列 |
| `sync` | 上行 | `{since}` | 请求补投 `msg_id > since` 的离线消息 |
| `sync_resp` | 下行 | `{messages: [msg 载荷数组]}` | 离线补投应答（空数组也是事件：确认「没有漏」） |
| `ping` | 上行 | `{}` | 应用层心跳（浏览器发不了 WS 控制帧） |
| `pong` | 下行 | `{}`（`seq` 回显 ping 的 seq） | 心跳应答，前端按 seq 配对 |
| `error` | 下行 | `{code, message}` | 协议错误，**连接不断**（见 4.6） |

ID 字段（`to`/`from`/`msg_id`/`client_msg_id`/`session_id`/`since`）
在信封里同样**一律字符串**；入站宽容接受数字。

### 4.4 生命周期时序

```text
客户端                                服务端
  │  GET /ws?token=…                     │
  │ ──────────────────────────────────▶ │ 查库验令牌（401 = 不升级）
  │ ◀──────── HTTP 101 升级 ──────────── │
  │ ◀── {"type":"welcome","payload":     │ 注册路由 + 分配会话 ID
  │      {"session_id":"…","reason":""}} │   （撞单端登录 → reason:"already online"，
  │                                      │    客户端见非空 reason 即断开）
  │ ── {"type":"msg","seq":1,…} ───────▶ │ 信封→Frame→feed_seq 去重→handle_frame
  │ ◀── {"type":"msg_ack",…} ──────────  │
  │ ◀── {"type":"msg",…}（对端消息）──── │
  │ ── {"type":"ping","seq":n} ────────▶ │ 就地回 pong（回显 seq）
  │ ◀── {"type":"pong","seq":n} ───────  │
  │ ── {"type":"sync","payload":         │ 断线重连后补增量
  │      {"since":"<本地最大 msg_id>"}} ─▶│
  │ ◀── {"type":"sync_resp",…} ────────  │
  │ ◀─ WS Close ───────────────────────  │ 连接终结（unregister 收尾）
```

前端心跳纪律（`web/src/stores/ws.ts`）：30s 一跳，连续 2 次没收到
配对 pong 即判「半开连接」，主动断开触发重连（指数退避 1s → 15s 封顶）。

### 4.5 content 的不透明语义

服务端对消息内容始终是**不透明字节**：上行把 payload.content 整体
`serde_json` 序列化成字节存/投递；下行尝试反解成 JSON 值，失败则降级
`String::from_utf8_lossy` 字符串——**展示降级优于静默吞消息**。
阶段 6 的内容模型 `{"kind":"text"|"file"|"image"|"emoji", …}`
天然兼容：对服务端它只是一个会飞的 JSON 值。

### 4.6 错误语义：坏信封不断连

| code | 触发 | 连接 |
|---|---|---|
| `bad_envelope` | 文本帧不是合法 JSON / 缺 type | 保持 |
| `bad_payload` | 业务类型但载荷缺字段（如 msg 没有 to） | 保持 |
| `welcome.reason = "already online"` | 同账号已在线（注册失败） | 客户端见 reason 即断 |
| HTTP 401 | 令牌无效/缺失 | 连接未建立 |

「协议错误 ≠ 连接错误」：一条坏消息只该废掉它自己。丢连接是
断言级事故（对端死了、网络断了），用 `select` 里的 `None`/`Err`/
`Close` 表达——**错误分级是协议设计的基本功**。

## 五、双传输对照：同一语义的两种皮肤

| 环节 | TCP（二进制） | WS（JSON 信封） |
|---|---|---|
| 鉴权 | `Handshake` 帧 + 口令摘要 | HTTP 升级前查库（`?token=`） |
| 就绪通知 | `HandshakeAck` 帧 | `welcome`（复用 `HandshakeAck` 载荷） |
| 发消息 | `Cmd::Msg` 帧（varint + content 字节） | `{"type":"msg", …}`（content 为 JSON 值） |
| 回执 | `Cmd::MsgAck` 帧 | `msg_ack` 信封 |
| 离线补投 | `SyncReq`/`SyncResp` 帧 | `sync`/`sync_resp` 信封 |
| 心跳 | 网关层 Ping/Pong **控制帧** | 应用层 `ping`/`pong` **信封** |
| 去重 | `DedupWindow`（帧 seq） | 同一套（信封 seq → 帧 seq） |
| 单端登录 | `HandshakeAck::rejected` | `welcome.reason = "already online"` |
| 服务端推送 | 写 actor 队列 | `WsSink` → mpsc 出站通道（容量 64） |

翻译层（`web/ws.rs` 的 `envelope_to_frame` / `frame_to_envelope`）
是这张表的代码形态：上行「信封 → Frame」再进会话核心，下行
「Frame → 信封文本」再进 WS 写循环。传输层帧（Handshake/Ping/Pong/
SyncReq）下行时返回 `None`（WS 路径不发这种帧，`FrameSink::send`
按成功对待——语义是「这条我管了」）。

## 六、设计模式实战（对照 roadmap 4.5）

| 模式 | 在本阶段的形态 |
|---|---|
| **适配器（Adapter）** | 翻译层 + `WsSink`：会话核心面向 `FrameSink` 编程，WS 网关把它适配到浏览器；TCP 的 `ConnectionHandle` 是另一个适配器。**被适配的双方（JSON/二进制）互不知情** |
| **仓库模式（Repository）** | `web/account.rs` 等四个仓储：SQL 细节不出仓储，处理器只见领域错误（`AccountError` → `ApiError` 的映射也集中在 web 层）。Java 对照 Spring Data JPA，但无动态代理、无反射 |
| **策略隐于配置** | REST 的错误映射、WS 的出站容量（64）都是「一处定义、处处生效」的小配置点——策略模式不需要类爆炸也能落地 |
| **提取器即中间件** | axum 的 `AuthUser`：鉴权逻辑写在类型上，路由表自动套用。对照 Java 的 Filter/Interceptor 链，但顺序由参数类型推导、编译期检查 |

> 适配器是本阶段最重要的模式：它让「加一种传输」从「重写会话层」
> 变成「写一个翻译层」。阶段 8 的 WebRTC 信令、阶段 10+ 的 QUIC
> 都会走同一条路——**核心稳定、皮肤可换**。

## 七、测试策略

| 层 | 用例 | 数量 |
|---|---|---|
| 翻译层单元测试 | msg 信封往返 / content 序列化 / ID 宽容解析 / 未知类型拒绝 / 传输层帧不出站 | 6 |
| WS 集成（真 HTTP + 真 WS + 真 PG） | 坏令牌升级前 401 / welcome+msg 往返 / 单端登录拒绝 / 断线重连离线补投 / 坏信封存活 | 5 |
| REST 集成（真 PG） | 注册登录 / 好友全流程 / 群组 / 文件上传下载（multipart 边界） | 39（含全部 web 模块） |
| 前端 | `vite build`（vue-tsc 类型检查）通过 | — |
| 真实进程冒烟 | 起服务端 → 注册 → 登录 → /api/me，验证 ID 串化生效 | 手工 |

WS 集成测试用 `tokio-tungstenite` 做真客户端：不走 `FrameSink` 的
快捷通道，而是像浏览器一样收发文本信封——**被测对象是协议本身**。

## 八、开发中踩过的坑

- **`Number.MAX_SAFE_INTEGER`**：雪花 ID 以 JSON number 下发时，
  前端拿到的是丢过精度的值（比对永远不等）。解法不是「前端小心」，
  而是**协议层统一串化**——在边界上修，别让每个消费者自己防；
- **浏览器发不了自定义 WS 控制帧**：`WebSocket` API 只能发文本/二进制
  数据帧，Ping/Pong 由浏览器内核自动处理且不可配对。心跳只能上移到
  应用层信封（这也是行业普遍做法的由来，不是将就）；
- **鉴权时机的三段论**：令牌无效（升级前 401）/ 单端登录（welcome
  拒绝）/ 连接中断（select 退出）。三种「失败」发生在三个层次，
  客户端要能分别感知——混为一谈的协议会让前端写出一堆猜谜代码。

## 九、下一步（阶段 6 预告）

地基已就绪，但「好友」还只是数据库里的两张表：

- 好友请求全流程上线：WS 推送 `friend_request` / `friend_accepted`
  事件（服务端主动推送的第一批真实场景），**仅好友间可发消息**（服务端校验）；
- 消息内容模型 `{"kind":"text"|"file"|"image"|"emoji", …}` 在前端落地：
  表情 picker、文件消息（上传得 fileId → 气泡元数据 → 点击下载）；
- 服务端 content 依旧不透明——**协议演进不要求核心重写**，这正是
  不透明语义的设计回报。

## 十、面试题与标准回答

**Q1：为什么不直接给 WebSocket 也用二进制协议？**

答：能（浏览器 `ArrayBuffer` 发二进制帧没问题），但收益不成比例。
二进制的价值在**带宽与编解码成本**（每字段省字节），这对移动端
海量并发有意义；而 Web 路径的消息频率是人手速级，瓶颈根本不在
协议体积。JSON 信封换来的是：浏览器原生调试（DevTools 直接看）、
前端零编解码代码、协议文档即 JSON Schema。更关键的是会话核心
收到的都是 `Frame`——**翻译成本被隔离在网关一层**，将来要换回
二进制（比如弱网优化的移动 Web）也只是替换翻译层。选型跟
瓶颈走，不跟「高级感」走。

**Q2：雪花 ID 超出 JS 安全整数范围，你们怎么处理的？为什么不在前端解决？**

答：协议层统一串化：所有 JSON 接口的 ID 字段一律字符串，入站宽容
接受两种形态。不在前端「小心处理」的原因：那要求每个消费者都记得
调 BigInt——只要有一个忘了，就是静默数据错误（比对不等、查库
 miss），比崩溃难查十倍。**在边界上修一次，好过在每个节点上修
N 次**。这也是为什么序列化协议设计里「大整数用字符串」是默认
惯例（Twitter/X 的 snowflake API、Telegram 的 MTProto web 层同理）。

**Q3：`FrameSink` 抽取后，TCP 与 WS 路径怎么共享去重窗口？**

答：去重窗口住在 `SessionState` 里，输入是帧级 `seq`。TCP 路径
seq 来自二进制帧头；WS 路径 seq 来自信封的 `seq` 字段，翻译层
组装 Frame 时带上。两条路径调的是同一个 `feed_seq`，裁决逻辑
（`InOrder`/`OutOfOrder`/`Duplicate`/`TooFar`）只有一份——
**可靠性语义不允许两份实现**，否则「WS 版 bug」和「TCP 版 bug」
会成为两个并行的测试矩阵。

**Q4：REST 鉴权用 Bearer 头，WS 为什么用 query 参数？**

答：WebSocket 的鉴权发生在 HTTP 升级请求上，而浏览器 `WebSocket`
API **不允许自定义请求头**——这是平台限制，不是设计偏好。工程上
的折衷就是 `?token=`。代价要认清：query 会进访问日志/代理日志，
所以令牌必须短时效 + 可撤销（我们存 DB，登出即删），生产环境
再配合一次性 ticket（REST 换一次性 ticket，WS 用 ticket 连接）
把泄漏窗口压到秒级——这是阶段 14 工程化的候选优化项。

---

*阶段 5 完成于：FrameSink 传输解耦、sqlx 迁移（7 张表）、REST 十四端点、
WS 网关（JSON 信封 + 应用层心跳）、Vue 3 前端骨架（登录/会话/聊天 +
自动重连）；web 模块 50 测试全绿，clippy pedantic 零警告。*
