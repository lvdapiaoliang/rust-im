# 19 - QUIC 与挂载盘：多路复用传输 + IM 数据的文件系统视图（阶段 13）

> 阶段 13 交付两件看似不相关、内里同构的事：**QUIC**（把传输层从
> 「一条 TCP 一个世界」升级为「一条连接多路复用」）与**挂载盘**
> （把 IM 数据从「协议里的字节」翻译成「文件系统里的目录树」）。
> 同构点：都是**世界观翻译**——QUIC 在 UDP 上重建一个更好的传输
> 世界观，挂载盘在 IM 数据上叠一个操作系统的世界观。

## 一、本章目标

读完本章你应当能回答：

1. QUIC 与 TCP 的三处本质差异（多路复用 / 握手内建 / 流与连接分离），
   以及为什么 HTTP/2 在 TCP 上没解决的问题 QUIC 能解决；
2. 阶段 12 `GatewayStream` 泛型化付的钱怎么在阶段 13 收利息
   （QUIC 流零改动进网关）；
3. LRU 为什么是「哈希表 + 双向链表」的组合，怎么零 unsafe 手写；
4. 挂载盘怎么分三层（内核 VFS / 语义层 / 驱动接线），语义层为什么
   必须先钉死、怎么钉死；
5. 缓存一致性的纪律为什么是「先改数据、再失效缓存」。

代码入口：

| 内容 | crate / 文件 | 测试 |
|------|-------------|------|
| QUIC 材料配置（TLS 1.3 + ALPN） | `im-crypto/src/tls.rs`（`quic_server_config` / `quic_client_config` / `QUIC_ALPN`） | 24 项（与 TLS 共享账本） |
| QUIC 装配（quinn 包住） | `im-transport/src/quic.rs` | 4 项 + doctests |
| QUIC 全链路演示 | `im-server/examples/quic_demo.rs` | 实跑通过 |
| 手写 LRU | `im-mount/src/lru.rs` | 10 项 |
| 内存文件系统 | `im-mount/src/memfs.rs` | 8 项 |
| 目录项缓存 | `im-mount/src/dir_cache.rs` | 7 项 |
| IM → FS 视图映射 | `im-mount/src/im_view.rs` | 5 项 + crate 级 1 项 |

## 二、QUIC：概念与 Java 对照

### 2.1 TCP 的问题不是「不可靠」，是「一条道」

TCP 给了可靠与有序，代价是**全局有序**：一条连接一个字节流，
前面丢一个段，后面全到不了（哪怕接收缓冲早就空着）——这是
**传输层队头阻塞**（Head-of-Line Blocking）。HTTP/2 在一条 TCP
连接上多路复用 HTTP 请求，看似解决了应用层的连接数问题，实际把
队头阻塞原样继承：请求 A 的一个 TCP 段丢了，请求 B 的数据在
接收缓冲里躺着也解不开——**应用层多路复用逃不出传输层的队头阻塞**。

Java 工程师对照：Netty 生态里 `netty-incubator-transport-quic`
就是基于 quiche/quinn 同源的 native 绑定；Spring 里 HTTP/3 的
支持同样是 QUIC 底座。概念上真正的新东西只有一样：**把「流」
从字节流的隐式概念变成协议里的显式对象**。

### 2.2 QUIC 的解法：流是协议对象

QUIC v1（RFC 9000/9001）在 UDP 上重建传输：

- **流（stream）是显式的**：每条流独立编号、独立可靠、独立有序；
  流 A 丢包只阻塞流 A。流号低 2 位是方向与发起方标记——客户端
  主动开的流从 0 起，服务端主动开的从 1 起，双方同时开不撞号；
- **TLS 1.3 内建进握手**（RFC 9001）：没有「先 connect 再
  handshake」两步，QUIC 握手即加密握手——`connect` 返回时加密
  通道已就绪。ALPN 从「可选」变「强制」：QUIC 握手必须协商应用
  协议，不协商直接失败；
- **连接迁移**：连接的身份是 ConnectionId 不是四元组，手机从
  WiFi 切 4G，地址变了连接不断——TCP 做不到（四元组就是身份）。

0-RTT、拥塞控制的改进（默认 CUBIC 类的 NewReno 起步，可插拔）
是锦上添花，**核心洞察就一条：把流做成协议对象，队头阻塞在
传输层被拆掉**。

### 2.3 为什么 IM 也要 QUIC

IM 的业务形态天然多流：一条「连接」上跑消息、跑心跳、跑文件
传输、跑语音信令——TCP 形态下要么多条连接（NAT/耗电成本），要么
应用层多路复用（自己写分帧 + 继承队头阻塞，阶段 2 的帧协议就是
这么干的：所有 Cmd 混一条字节流，一个慢消费者堵住所有人的读循环，
阶段 7 的扇出隔离就是在给这个结构打补丁）。QUIC 把「每流一个
逻辑会话」变成传输层的原生能力。

## 三、代码走读：QUIC 装配层

### 3.1 分层：材料归 im-crypto，装配归 im-transport

与阶段 12 TLS 同一条切法：

```text
im-crypto::tls                    im-transport::quic
（材料：配置）                    （装配：把 quinn 类型包住）
──────────────                    ─────────────────
QUIC_ALPN 常量        ──────▶    QuicAcceptor::bind(addr, ServerConfig)
quic_server_config()  ──────▶      └ QuicServerConfig::try_from(cfg)
quic_client_config()  ──────▶    QuicConnector::connect(addr, server_name)
（TLS 1.3 only + ALPN）            └ 传 rustls 配置，不传 quinn 配置
```

**quinn 类型不越出装配层**——上游（demo/未来接入 im-client）只看
到 rustls 配置与自家句柄，`quinn::Endpoint` 换成别的 QUIC 实现
（如 quiche）时上游零改动。这与 docs/18 §三「材料与装配分离」
是同一纪律在第二个传输协议上的复用。

材料侧的新增只有一处：QUIC 配置钉 **TLS 1.3 + ALPN**。ALPN 写成
常量 `QUIC_ALPN = b"rust-im/1"` 并双端共用——协议名是双端契约，
一处定义防止漂移。

### 3.2 收利息：QuicStream 直接进网关

阶段 12 把 `run_gateway_connection` 从 `TcpStream` 泛型化成
`S: GatewayStream`（docs/18 §九立账：「QUIC 流实现同一个 trait
就能进网关」）。阶段 13 的兑付只用了两个 impl：

```rust
// im-transport/src/quic.rs（节选）
impl AsyncRead for QuicStream { /* 委托 recv 半部 */ }
impl AsyncWrite for QuicStream { /* 委托 send 半部；poll_shutdown = FIN */ }
impl GatewayStream for QuicStream {
    fn peer_addr(&self) -> Option<SocketAddr> {
        Some(self.conn.remote_addr())  // 动态取：连接迁移后地址会变
    }
}
```

心跳应答、读空闲超时、优雅关闭——阶段 2 写的全部连接生命周期
管理对 QUIC 流零改动复用。**泛型化是提前付款，trait 边界是收款
账户**：付款时不知道第二个实现什么时候来，账户保证来的时候
零改动（对照：如果网关写死 `TcpStream`，QUIC 接入要复制整套
生命周期代码——阶段 7 的扇出、阶段 10 的超时修复全部要改两份）。

值得多看一眼的细节：quinn 把双向流拆成 `SendStream` + `RecvStream`
两个半部，`QuicStream` 是**适配器**（Adapter）——把两个半部粘回
`AsyncRead + AsyncWrite` 的整流，对帧协议层伪装成「一条普通的流」。
`poll_shutdown` 映射到 QUIC 的 FIN：对端 `read_frame` 看到 EOF，
与 TCP 半关闭语义对齐，帧层的连接关闭检测原样可用。

### 3.3 测试表（真实 UDP loopback，零 mock）

| 用例 | 验证什么 |
|------|---------|
| `frame_roundtrip_over_quic` | QUIC 之上跑既有帧协议：read_frame/write_frame 零改动 |
| `quic_stream_goes_through_gateway` | Ping 自动回 Pong：网关生命周期管理对 QUIC 流复用 |
| `streams_multiplex_without_head_of_line_blocking` | 流 A 挂住 10 秒，流 B 的 echo 畅通——TCP 上不可能的事 |
| `untrusted_ca_is_rejected_over_quic` | 证书校验在握手里真的在跑，不是「UDP 通了就算连上」 |

第三条是 QUIC 的存在意义测试：服务端在流 A 读到一帧后故意挂住，
同一连接的流 B 照常 echo。换成 TCP 单连接，字节流交织，前面的
字节不解完后面的帧到不了解码器——**用测试把协议的招牌性质
钉进 CI**，不是写在注释里。

### 3.4 quic_demo：两条会话流共用一条连接

`cargo run -p im-server --example quic_demo`

与 tls_demo 的对照就是阶段 13 的故事：

```text
TLS 形态（tls_demo）              QUIC 形态（quic_demo）
─────────────────                ─────────────────────
TCP accept（两条连接）           UDP accept_conn（一条连接）
├ Alice 连接 → serve_connection  ├ Alice 流 ──┐
└ Bob 连接   → serve_connection  └ Bob 流   ──┴→ serve_connection
                                  （每流一个会话，同一个函数）
```

Alice 与 Bob **不是两条连接，是同一条 QUIC 连接上的两条双向流**。
客户端脚手架 `QuicDemoClient::open_stream(&quic_conn)` 与
`TlsDemoClient::connect` 逐行相同（除了开流 vs 连接），会话核心
（`serve_connection`）一行没动——demo 实跑输出握手 session_id、
中文消息逐字无损、双向回信、优雅关停。

## 四、挂载盘：概念与代码走读

### 4.1 分尸视角：三层各管什么

「把 IM 数据挂成一个盘」（Telegram Desktop 同款功能）拆开：

```text
┌────────────────────────────────────────────────────┐
│ 内核 VFS            （操作系统，不归我们写）        │
│        ↓ 回调：lookup / readdir / read / getattr    │
├────────────────────────────────────────────────────┤
│ MemFs 语义层       路径、inode、目录项、错误分类    │  ← im-mount 交付
│        ↑ 数据源：IM 的联系人/会话/消息              │
├────────────────────────────────────────────────────┤
│ 驱动接线 FUSE(libfuse) / WinFsp                    │  ← 诚实边界
└────────────────────────────────────────────────────┘
```

驱动层依赖内核态组件（Linux 的 FUSE 设备 / Windows 的 WinFsp
驱动），是 unsafe + 平台胶水的领域。**本阶段刻意停在语义层**：
驱动接线只是把回调参数翻译成 `MemFs` 方法调用的纯胶水，但语义
与一致性（路径解析、目录项、错误分类、缓存纪律）必须先在可单测
的环境里钉死——先有可测试的语义，才有资格谈接线。

### 4.2 手写 LRU：哈希表管定位，双向链表管时序

roadmap 数据结构表立的账「挂载盘目录缓存：双向链表 + 哈希表，
阶段 3/13 手写」——阶段 13 兑现，`im-mount/src/lru.rs`：

```text
map: HashMap<K, slot 下标>        ← O(1) 定位
slots: Vec<Slot> + head/tail 索引  ← 双向链表管「谁最新谁最旧」

get(k)：map 找到 slot → 链表摘下 → 插回表头        O(1)
put(k,v)：更新或插入；超容 → 淘汰表尾（返回被逐出的对） O(1)
```

两个值得展开的工程决定：

**为什么用下标链表而不是 `Box` + 裸指针**：教科书 LRU 用裸指针
链 `Box<Node>`，拿 prev 指针要 unsafe。本项目 workspace 钉
`unsafe_code = "warn"`（SDK 层之外不写 unsafe 的纪律），所以节点
进 slab（`Vec<Option<Slot>>`），prev/next 存**下标**——零 unsafe
换来的是：淘汰/删除的槽位进 free-list 复用，容量稳定后 Vec 不再
增长（测试 `slot_reuse_keeps_links_consistent` 用 100 轮插入钉死：
slab 稳定在容量+1）。

**为什么泛型边界是 `K: Clone`**：哈希表和链表节点各需要一份 key
（淘汰表尾时要从 map 里删对应项，此刻 key 只在节点手里）——插入
时 clone 一份，如实写进泛型边界，而不是用 unsafe 挪动。**诚实的
泛型边界比聪明的 unsafe 更符合学习项目的定位**。

顺带的边界课：最初实现是「先淘汰再插入」，容量 0 时表尾为空、
淘汰落空，新条目照样进了「容量 0」的缓存——测试
`zero_capacity_evicts_immediately` 抓住后改为「插入后淘汰」：
容量 0 时刚插入的项自己就是表尾，「插入即淘汰」与容量 n 走同一条
代码路径。**边界条件（空/满/单元素）是数据结构的强制测试项**，
与 docs/18 双棘轮的「乱序是状态机的强制测试项」同一纪律。

### 4.3 MemFs：FUSE 回调的纯逻辑部分

`im-mount/src/memfs.rs` 的方法名刻意与 FUSE/WinFsp 回调对齐：

| 本模块方法 | FUSE 回调 | `WinFsp` 侧 | 说明 |
|-----------|----------|-----------|------|
| `MemFs::lookup` | `lookup` | `GetFileInfoByPath` | 路径 → 属性 |
| `MemFs::read_dir` | `readdir` | `FindFiles` | 目录项列表（字典序） |
| `MemFs::read` | `read` | `ReadFile` | 读文件内容 |
| `mkdir` / `write_file` | `mkdir`/`mknod`+`write` | `Create` | 构建/更新视图 |

配套的错误类型 `FsError` 按 **POSIX errno 分类**命名
（`NotFound`≈`ENOENT`、`NotADirectory`≈`ENOTDIR`、
`IsADirectory`≈`EISDIR`）：挂载盘的错误最终要穿过驱动层报给
操作系统，对齐内核的错误分类，驱动胶水就是纯翻译；自己发明分类，
胶水层就得写「错误翻译的错误处理」。

inode 的角色一句话：**路径是给人看的，inode 是给系统用的**
（真实 FS 里它是磁盘寻址句柄，内存版是 `BTreeMap` 的 key，
语义角色相同）。目录的孩子表用 `BTreeMap` 不是为了性能——是为了
readdir 顺序稳定（字典序），真实文件系统也保证列举顺序可预期。

### 4.4 DirCache：LRU 的落地与缓存一致性的纪律

`im-mount/src/dir_cache.rs` 把三个教学点钉在一起：

1. **缓存什么**：目录项列表（读多写少，explorer 反复列同一目录）；
   **不缓存什么**：文件内容（IM 视图的文件都很小，直读）、
   属性查询（O(路径深度) 的纯内存遍历，失效成本覆盖不了收益）——
   **不是所有东西都该缓存，「不缓存」本身是设计决定**；
2. **失效纪律**：先改 `MemFs`、后失效缓存，顺序反了就是脏读
   窗口。纪律不靠自觉——`CachedFs` 把「写操作 → 失效父目录」
   封装在一个类型里，调用方没有机会只改数据不失效缓存；
   失效的粒度是**父目录**（改动影响的是父目录的列表）；
3. **错误不缓存**：miss 时真实读取的错误穿透，不把错误存起来
   （目录后来出现了必须能读到——测试 `errors_are_not_cached`）。

计数器选型的小细节：命中/未命中用 u32 不是 u64——`f64::from(u32)`
精确无损（u64 会丢尾数，`as f64` 又吃 clippy 精度警告），饱和加法
封顶 42 亿次，观测计数够用十辈子。**类型选择是性能、精度与纪律
的三方权衡，不是越大越好**。

### 4.5 im_view：世界观翻译

`im-mount/src/im_view.rs` 把 IM 的领域模型固定映射成 FS 树：

```text
IM 世界观                          FS 视图（layout 模块的契约）
──────────                        ─────────────────────────
联系人 alice     ──────▶  /contacts/alice.txt      名片
会话历史（按人） ──────▶  /history/alice/2026-09-24.log
共享文件         ──────▶  /files/<uuid>.bin         原始字节
```

映射约定集中在 `layout` 模块的一个函数里，不散落在调用方各自
拼路径——布局是**双端契约**（一旦有 grep/备份脚本依赖它，改布局
就是 breaking change），与协议字段同一性质。

三个语义决定：构建幂等（同批数据两次构建逐字节相同——投影的
确定性是可测试性的前提）；同日消息**先聚合再落盘**（按
(peer, date) 聚成整段日志，不逐条覆写）；**只读投影**——数据
主权在 IM 侧，从 FS 侧改文件再「同步回」IM 是双向冲突合并的
产品级坑，学习项目止步于单向。

### 4.6 诚实边界：本机无 WinFsp 驱动

真挂载需要：Windows 装 WinFsp（驱动 + MSI）、Linux 启用 FUSE。
本机无 WinFsp——与阶段 9 LiveKit/Docker 同一处理：**机制交付、
环境如实记录**。接线方案（装好驱动后的胶水形状）：

```text
WinFsp FSD → 回调线程 → [翻译层] → MemFs::lookup / read_dir / read
                             ↑ unsafe 与平台绑定的部分全部圈在这里
```

语义层已按回调形状切好方法、错误已按 errno 分类，接线时不需要
再碰任何逻辑。不装作挂载过了——`cargo test -p im-mount` 31 项
全绿是本阶段的真实验证口径。

## 五、模式与算法账单

| 模式/结构 | 落点 | 一句话 |
|----------|------|--------|
| 适配器（Adapter） | `QuicStream`（收发半部 → 整流） | 两个只读只写半部拼成一个 AsyncRead+AsyncWrite |
| 外观（Facade） | `QuicAcceptor`/`QuicConnector` | quinn 类型不越出装配层 |
| 依赖倒置的利息 | `GatewayStream` 的第二个实现 | 泛型化付款、trait 收款，网关零改动 |
| 哈希表 + 双向链表 | `LruCache`（slab 下标版，零 unsafe） | O(1) 的 get/put/淘汰 |
| 契约集中 | `im_view::layout` | 布局映射一处定义，双端契约 |
| 纪律用类型封装 | `CachedFs` | 写 → 失效不可拆开，不靠自觉 |

## 六、已知边界与取舍（诚实账单）

- **QUIC 未接入 im-client 主链路**：传输层（装配 + 网关）与 demo
  已就绪，客户端默认传输仍为 TCP+TLS。QUIC 切换涉及重连策略
  （Backoff 形态）、连接迁移的会话恢复——留待需要时接入，
  不装作「已经全量 QUIC」；
- **连接迁移与 0-RTT 未暴露**：quinn 能力面有，本模块 API 未开
  （0-RTT 需 TLS 1.3 session ticket，迁移需 endpoint 级配置）；
- **弱网对比未做**：im-bench 的 weak-link 是 TCP 形态，QUIC 在
  10% 丢包下对 TCP 的实测对比是记在账上的欠项（需要把
  weak-link 的丢包/延迟注入移植到 UDP 侧）；
- **挂载盘只读**：单向投影，双向同步不做（§4.5）；
- **失效纪律是单线程口径**：`CachedFs` 演示「先改数据再失效」
  的顺序纪律，多线程下失效前夜的读还有竞态——那时才轮得到
  epoch/版本号方案，本阶段不预支复杂度。

## 七、下一步（阶段 14 预告）

开源工程化：CI 矩阵（多平台构建 + 全量测试 + clippy/fmt 门槛）、
版本与发布（workspace 版本统一 bump、CHANGELOG）、文档站。
阶段 13 的 QUIC 与挂载盘都留了「接线层」——CI 把「任何平台一键
构建」变成机器保证，和 rustls 钉 ring 是同一条「能被任何人构建」
的价值观。

## 八、面试题与标准回答

**Q1：HTTP/2 已经多路复用了，QUIC 解决的到底有什么不同？**

答：所在的层不同。HTTP/2 的多路复用在应用层，底下还是一条 TCP
字节流——传输层丢一个段，接收缓冲里早就到达的其他流数据一样
解不开，队头阻塞被原样继承。QUIC 把「流」做成传输层协议对象：
每条流独立编号、独立可靠、独立有序，流 A 丢包只阻塞流 A。
一句话：**应用层多路复用重新分配了「用哪条连接」，传输层多路
复用拆掉了「谁堵谁」**。本项目的测试
`streams_multiplex_without_head_of_line_blocking` 把这个差异
做成了可执行断言：流 A 挂 10 秒，流 B 的 echo 必须畅通。

**Q2：QUIC 为什么把 TLS 1.3 拉进握手？ALPN 为什么从可选变强制？**

答：QUIC 整个传输层都在 UDP 用户态实现——加密不能像 TCP 那样
「套一层 TLS 在外面」（没有内核 TCP 栈帮忙，握手、记录层都得
自己定义），RFC 9001 干脆把 TLS 1.3 作为握手的有机组成部分：
QUIC 握手即加密握手，`connect` 返回即加密通道就绪。ALPN 强制是
防御性设计：QUIC 与 QUIC 之外的东西共享 UDP 端口空间（如 DNS、
STUN），客户端必须声明「我说的是哪个应用协议」，服务端不认识
当场拒绝——没有这层协商，「连上了但说什么语言全靠猜」。本项目
把 ALPN 写成常量 `QUIC_ALPN` 双端共享：协议名是双端契约，
一处定义防止漂移。

**Q3：手写 LRU 怎么做到 O(1) 又零 unsafe？**

答：两个结构各司其职：`HashMap<K, slot 下标>` 管 O(1) 定位；
双向链表管时序（命中提到表头，淘汰从表尾）。零 unsafe 的关键
是把教科书的「Box + 裸指针」换成 **slab + 下标**：节点进
`Vec<Option<Slot>>`，prev/next 存下标，删除的槽位进 free-list
复用——借用检查全程在场。代价：key 要 Clone（哈希表和链表节点
各一份，淘汰表尾时才找得到要删的 map 项）。工程上的选择：
**诚实的泛型边界换不写 unsafe**，学习项目里这笔账划算。

**Q4：缓存的失效为什么「先改数据、再失效缓存」？反过来不行吗？**

答：看谁产生脏读。先失效后改数据：失效到改完之间，任何一次读
都是 miss、走真实数据源——**拿到的是旧但一致的数据**，最坏多
一次穿透。先改数据后失效：改完到失效之间，缓存里是旧数据且还
会被命中——**脏读**。所以顺序必须是先改再失效（多线程下失效
前夜的读仍有竞态，那是 epoch/版本号方案的地界）。失效之后，
纪律靠什么维持？靠**类型封装**：`CachedFs` 把「写 → 失效」放进
同一个方法，调用方没有机会只改数据不失效——纪律写在代码里，
不写在注释里。

**Q5：挂载盘的错误处理为什么要对齐 errno？**

答：错误的最终去处决定了分类法。挂载盘的错误要穿过驱动层报给
内核（内核把它翻译成 `ENOENT` 之类的 errno 再给用户态进程），
上游还有一层「错误翻译」：自己发明的分类 → errno。语义层直接
按 errno 分类（`NotFound`/`NotADirectory`/`IsADirectory`），
驱动胶水就是查表直译。引申：**错误按「处置方式」分类，不按
「发生位置」分类**——这与本项目传输层把 QUIC 各种错误收口成
IO 错误是同一条原则：下一层只关心「这个错误我怎么办」。

---

*阶段 13 完成于：`im-transport`（QUIC 装配，45 项测试全绿 +
quic_demo 实跑）、`im-mount`（手写 LRU + 内存 FS + 目录缓存 +
IM 视图映射，31 项 + doctests 全绿，零 unsafe、零内部依赖）、
`im-crypto`（QUIC 配置：TLS 1.3 + ALPN，24 项全绿）。踩坑收录于
docs/20：quinn 配置包装与两段式连接错误（§2.11）、固有方法
遮蔽 trait 方法（§7.2 #11）、LRU 容量 0 淘汰顺序（§4.7）、
返回引用的省略生命周期绑错对象（§7.2 #12）。*
