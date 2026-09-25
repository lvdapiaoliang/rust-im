# 18 - TLS 与 E2EE：rustls 传输加密 + Signal 双棘轮 + 类型状态原生 API

> 阶段 12 交付物：`im-crypto`（从 22 行占位符长成加密层：TLS 材料 +
> X3DH + 双棘轮，23 项测试）、`im-transport` 的 TLS 装配（Connection
> 泛型化——阶段 2 装饰器模式的兑现）、`im-sdk` 的类型状态 Rust 原生
> API（docs/17 §七立的账在这里还清）。桌面端 Tauri 壳走诚实边界
> （§五的账单），但壳里要跑的 Rust 后端 API 已经全部就位。
>
> 安全三条线的关系一句话：**TLS 防窃听（网络），E2EE 防服务器
> （平台），类型状态防自己（编译期）**。

## 一、本章目标

| 交付物 | 内容 | 验证 |
|---|---|---|
| `im-crypto` | TLS 材料（自签 CA + rustls 配置）+ `e2ee`（x3dh/ratchet） | 23 项测试全绿 |
| `im-transport::tls` | TlsAcceptor/TlsConnector 装配 + `GatewayStream` trait | 41 项测试全绿（阶段 2 起的存量回归一个不少） |
| `im-transport::connection` | `Connection<S = TcpStream>` 泛型化 | 既有调用点零改动（默认类型参数） |
| `im-server` | `serve_connection<S: GatewayStream>` 泛型化 + `tls_demo` | demo 实跑：TLS 握手 → 雪花 session_id → 中文逐字无损 |
| `im-sdk::native` | `TypedSdkClient<Connected>` 类型状态 API + 错误码 6 | 15 项测试 + compile_fail doctest |

验证口径：`cargo test --workspace` 全绿 + clippy 零警告 + fmt 干净；
`cargo run -p im-server --example tls_demo` 端到端实跑通过。

## 二、TLS：传输层加密

### 2.1 为什么现在做、怎么做

roadmap 阶段 2 就把「TLS 移至阶段 12 前置实现」写进了阶段表——传输
层加密不改变帧协议，只改变帧跑在什么字节流上，所以它天然是**后置
的**：先把明文协议跑稳（阶段 2~11 的全部测试都建立在明文 TCP 上），
再一次性把整个传输层换成加密流。做法不是「加一个 TLS 模块」，而是
**把连接层泛型化，让 TLS 成为可插拔的一种流**。

### 2.2 Connection 泛型化：装饰器模式的兑现

阶段 2 的文档说过：帧协议层是装饰器，TcpStream 是被装饰者。本阶段
把这句话变成类型签名：

```rust
pub struct Connection<S = TcpStream> { /* ... */ }
impl<S: AsyncRead + AsyncWrite + Unpin> Connection<S> {
    pub async fn read_frame(&mut self) -> ... { /* 帧逻辑，对流类型无感 */ }
    pub async fn write_frame(&mut self, frame: &Frame) -> ... { /* 同上 */ }
}
// TCP 专属能力（connect/peer_addr）留在专属 impl 块：
impl Connection<TcpStream> { pub async fn connect(...) -> ... }
```

`S = TcpStream` 默认类型参数是兼容性的关键：**既有调用点一行不改**
（写 `Connection` 还是 TCP 连接），只有 TLS 装配点显式写
`Connection<ServerTlsStream>`。装饰器模式在 Rust 里的自然形态就是
泛型 + trait 约束——比 Go 的接口包装少一层手动转发。

**半部拆分的取舍**：读写半部从 `TcpStream::into_split`（Owned 半部，
TcpStream 专属）换成 `tokio::io::split`（BiLock 半部，任何流通用）。
BiLock 无争用路径只是一次原子交换，近零成本；代价是 M1 压测基线
（99,969 连接，docs/16）建立在 Owned 半部上，**泛型化后未重测**——
诚实口径：M1 数字描述的是泛型化前的实现，BiLock 版的理论开销可
忽略但没实测背书，不冒充同一口径的数字。

### 2.3 GatewayStream：网关的依赖倒置

网关（心跳/超时/优雅关闭，阶段 2 的资产）不关心流是什么，但需要
`peer_addr` 打日志。于是定义：

```rust
pub trait GatewayStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    fn peer_addr(&self) -> Option<SocketAddr>;
}
```

`TcpStream` 与两种 TLS 流都实现它；`spawn_gateway`/`read_loop`/
心跳循环全部 `<S: GatewayStream>`——网关代码**零改动**复用。这是
依赖倒置的标准收益：网关定义自己要什么（trait），而不是接受
实现者给什么（具体类型）。

### 2.4 rustls 选型：为什么显式钉 ring

rustls 0.23 默认 provider 是 aws-lc-rs——密码学上更现代（更早的
AVX512 优化、FIPS 路径），但在 Windows 上要 NASM/CMake 走 CMake
构建链，CI 与用户环境的失败面大。选 ring：

```toml
rustls = { version = "0.23", default-features = false,
           features = ["ring", "std", "tls12", "log"] }
```

并且**不依赖进程默认 provider**：配置一律
`builder_with_provider(Arc::new(ring_provider::default_provider()))`
显式传入。库不该 `install_default_process_cryptography` 污染宿主——
宿主可能自己钉了别的 provider，进程全局只能有一个人说了算。

### 2.5 证书拓扑与材料层

im-crypto 只**造材料**（证书 + rustls 配置对象），im-transport::tls
只**装配**（TcpStream → TLS 握手 → Connection）——材料与装配分离，
装配层就不必依赖 rcgen，测试可以只装配不生成。

```text
自签 CA（演示口径，不落盘不接 KMS）
   └── 签发服务端叶子证书（SAN: localhost + 127.0.0.1）
         客户端信任锚只放 CA 公钥 → 叶子可单独轮换
```

三个设计点：

- `TlsMaterial` 不实现 `Clone`——**私钥不该能被复制**，所有权即边界；
- 握手失败归入 `TransportError::Io`：握手失败本质就是这条连接的
  IO 失败，不值得单设错误类别（错误模型按**处置方式**分类，不按
  发生位置分类）；
- 客户端 `ServerName` 与连接地址解耦传入——IP 直连 + 域名 SNI 是
  生产常见形态，别把它们焊死。

### 2.6 测试与 demo：五面镜子 + 一场全链路

im-transport::tls 的五项测试，每一项对一个具体的安全主张：

| 测试 | 证伪的目标 |
|---|---|
| TLS 帧往返 | 加密流上帧协议照常工作 |
| 不可信 CA 被拒绝 | 信任锚真实生效（不是「连上就算」） |
| SAN 不匹配被拒绝 | 服务器身份校验真实生效 |
| 裸服务端读到 `0x16 0x03` | 线上字节真是 TLS 记录层（不是明文裸奔） |
| TLS 流进网关心跳存活 | 网关复用是真的（心跳/优雅关闭对加密无感） |

`tls_demo`（im-server/examples）把话说满：服务端 `accept` 后多走
一步 `accept_stream`（TLS 握手），**其余与明文 serve 一字不差**——
`serve_connection<S: GatewayStream>` 让认证、路由、去重、优雅关闭
全部照常。实测输出：双端 TLS 握手成功、雪花 session_id
（378825973136625664）、中文消息逐字无损、干净关停。

## 三、E2EE：端到端加密（Signal 协议学习实现）

### 3.1 TLS 之上为什么还要一层

TLS 的信任边界在**服务器**：服务器解密一切、看到一切。对 IM 这是
不够的——服务器可能被攻破、被传唤、被内部作恶。E2EE 把信任边界
推到**端**：服务器只见密文与路由元数据。两层加密不是重复建设，
是**信任边界不同**的两道墙（Telegram 的 MTProto 只做传输层、Signal
做端到端——这是两家产品哲学的分水岭，也是面试高频题）。

### 3.2 X3DH：异步世界的密钥协商

IM 的现实：**开聊时对方大概率不在线**。交互式协商（TLS 型）做不到，
X3DH 让 Bob 预先上传一捆预密钥，Alice 单方面算出共享根密钥 SK：

```text
Bob 上架：IK（长期身份）+ SPK（IK 签名背书的中期密钥）+ OPK（一次性）
Alice 开聊：拉取 PreKeyBundle → 验 SPK 签名 → 生成一次性 EK →

SK = HKDF(F ‖ DH1 ‖ DH2 ‖ DH3 ‖ [DH4])
     DH1 = DH(IK_A, SPK_B)  身份绑定
     DH2 = DH(EK_A, IK_B)   认证 Bob
     DH3 = DH(EK_A, SPK_B)  前向保密
     DH4 = DH(EK_A, OPK_B)  Bob 侧前向保密（OPK 存在时）
```

三个安全细节，每个都对应一类真实攻击：

1. **SPK 签名内容是 `key_id ‖ public`**：签名把 key_id 拴死在公钥上，
   服务器无法把 A 预密钥的签名搬到 B 预密钥上（跨键挪用）；
2. **签名必须验**：不验等于允许服务器掉包假预密钥（MITM at server）——
   `initiate` 的 `BadSignature` 错误全部意义在此，测试里用
   「Mallory 的预密钥 + Bob 的身份」组装伪造 bundle 验证拒绝；
3. **OPK 不对称时显式拒绝**：initiation 声称用了 OPK 但 Bob 已消耗，
   原型实现会静默跳过 DH4——两侧 SK 必然不同且无人报错。修正为
   直接拒绝：**宁可拒绝也不静默产出对不上的密钥，错误比不一致便宜**。

### 3.3 双棘轮：每条消息都在换锁

X3DH 给出根密钥后，双棘轮接管会话的生命周期。两个棘轮、两种保密：

```text
── 对称棘轮（每条消息）──        ── DH 棘轮（每轮往返）──
CK ─┬─ HMAC(0x01) → MK 加密这条   RK ←── HKDF(DH(新对, 对端公钥))
    └─ HMAC(0x02) → CK' 下一条     （先与旧对算 CKr，再生成新对算 CKs——
                                     接收侧两跳，两次洗根）
```

- **对称棘轮**：拿到第 N 条的 MK 推不出第 N-1 条（消息级前向保密）；
- **DH 棘轮**：某条链密钥泄露，下一轮往返把根密钥洗掉（轮级自愈）；
  双向都发消息才推进——「对话」本身就是密钥更新的节拍器。

两个工程细节值得记：

- **AAD = 消息头**：GCM 把头绑进认证——改序号/改公钥让标签必败，
  「这条密文属于棘轮的哪个位置」被密码学背书；
- **nonce 从 MK 派生、不上线路**：`HKDF(MK) → (key, nonce)`，每条
  消息的 nonce 随 MK 用后即焚。GCM 的 nonce 重用是灾难（两 nonce
  相同即泄漏认证密钥），从密钥派生让「换密钥必换 nonce」成为构造
  保证而不是纪律约定。

### 3.4 乱序容忍：跳过密钥缓存

网络重排让消息可能乱序到。解密方发现序号跳了，就把跳过序号的 MK
**先派生存进 `skipped`**（键带对端棘轮公钥——不同轮的旧链天然分桶），
迟到的旧消息用缓存解。上限 `MAX_SKIP = 100`：跳太多说明对端有 bug
或在搞 DoS 扩张内存，拒绝比包容便宜。与 im-transport 去重窗口的
「重同步而非丢弃」是同一防御思想的两种表达：**那边防丢消息，这边
防丢密钥**。

实现里踩过一个典型 bug 值得立此存照：`skip_to` 只在函数内部推进
链密钥的局部副本、不回写，调用方却用**跳过前的旧 CK** 派生本条
消息的 MK。按序解密时 `from == until`，循环体不执行，bug 完全
隐形；乱序用例（2,0,1 投递）当场炸穿。修正：`skip_to` 按值收链、
**返回推进后的链**，调用方拿返回值派生。教训：**带内部状态的迭代
助手，要么自己管状态，要么把新状态还给调用方，「原地假设」两头
不靠**（docs/20 收录）。

### 3.5 测试：性质测试替代官方向量

诚实修正：Signal 双棘轮规范（V3.3+）**没有发布官方测试向量**
（规范文档只有伪代码；libsignal 的测试数据不在规范里）。原计划
「官方向量验证」不可行，改为性质测试——密码学协议的可验证性本来
就该靠性质而非单一向量：

| 性质 | 测试 |
|---|---|
| 两侧收敛 | X3DH 双方 SK 相同；组合测试里棘轮互解 |
| 新鲜性 | 两次协商 SK 互异；同明文 10 条密文互异 |
| 攻击拒绝 | 伪造 bundle / 签名移植 / key_id 错位 / 密文篡改 / 头篡改 / 错投会话 |
| 乱序容忍 | 2,0,1 投递全解；MAX_SKIP 超限拒绝 |
| 状态机对称 | 交替往返 5 轮，每轮都推 DH 棘轮 |
| 全生命周期 | `full_flow_x3dh_then_double_ratchet`：X3DH 的 SK 原样喂进棘轮，双向往返 |

组合测试是单测相加给不了的：单测各自绿，**接线**（SK 交接 + SPK
公钥/私钥分给两侧）也得证明对得上。

### 3.6 学习版的诚实边界

- **信任模型是 TOFU**：Bob 不校验 Alice 的 IK_A 真伪（生产里靠
  SAFETY NUMBER 指纹比对或信任链）。学习版里服务器可以给 Bob
  掉包假的 Alice 身份——MITM 风险明确记档，不装作不存在；
- AAD 绑定消息头而非双方身份公钥（身份绑定在 X3DH 层完成一次）；
  不含头部加密（PQXDH/ADOC 等进阶形态不在本阶段）；
- 私钥擦除靠 `ZeroizeOnDrop`/手动 `Drop`（`RatchetState` 含
  HashMap 不满足派生约束，手动逐字段擦——宁可显式三行，不为派生
  换自定义容器）。

## 四、类型状态 Rust 原生 API（im-sdk::native）

### 4.1 docs/17 的账，还清了

阶段 11 实做后承认：**C ABI 形态下类型状态无法兑现**——`void*`
到不了 C 的类型系统，编译期保证降级为运行时检查。但同一份实现在
Rust 原生 API（rlib 形态）里可行。本阶段交付 `TypedSdkClient`：

```text
TypedSdkClient<Disconnected> ──wait_connected──▶ TypedSdkClient<Connected>
       │ 没有 send 方法                                │ 有 send
       └─ 未连接就发消息 → E0599 编译错误
```

`send` 只存在于 `impl TypedSdkClient<Connected>` 块——「未连接的
句柄不能发消息」不再靠文档或运行时检查，**方法根本不存在**。状态
转换消耗旧值产出新值（`wait_connected(mut self)`），旧句柄随跃迁
作废——状态机的每次跳变都被所有权系统记账。

**compile_fail doctest 把承诺锁死**：文档里放一段标注
` ```compile_fail ` 的示例（Disconnected 形态上调 send），rustdoc
每轮测试都验证它**编译不过**——「编译期保证」本身成了被测试的
性质，不是自我声明。

### 4.2 三个设计点

1. **`PhantomData<fn() -> State>` 而非裸 `PhantomData<State>`**：
   函数指针形态不「持有」State，State 的 Auto trait 约束不会反向
   传播到结构体——标记类型该是零负担的，让它保持零负担；
2. **错误归还（fold-back）**：`wait_connected` 的错误是
   `(TypedSdkClient<Disconnected>, i32)` 而非干净的 `Err(i32)`——
   超时的客户端还是好的（重连进行中），拿回去可以再等；被拒的拿
   回去 destroy。所有权语言里，**把东西还给调用方比替他扔掉更
   诚实**（clippy 的 `result_large_err` 警告在此被显式 allow 并
   注明理由——大小是设计，不是疏忽）；
3. **`session_id` 只在 Connected 形态可读**：跃迁时从 `Connected`
   事件取出存进字段，`Disconnected` 时恒 0 且 getter 不存在——
   状态携带「该状态才有的数据」，类型状态不止是门禁。

诚实边界：**类型编码「协议阶段」，不编码「链路活性」**。`Connected`
证明握手已完成，不是「网线此刻是通的」——断线重连是 im-client
状态机的内部职责，断线后 send 本来就合法（离线排队、重连补投）。
类型状态收走的是更基本的错误：连握手都没完成就发消息。想用类型
编码链路活性会撞上「类型不能随网络事件回退」的硬墙——运行时
状态，别让类型系统背它背不动的锅。

### 4.3 分层归位

```text
C 宿主 ──▶ ffi.rs（void*，运行时检查）──┐
                                        ├──▶ SdkClient（core.rs，同一实现）
Rust 宿主 ─▶ native.rs（类型状态，编译期保证）──┘
```

`TypedSdkClient` 是**装饰器 + 类型状态**的组合：装饰一层状态泛型，
被装饰的 `SdkClient` 一行不改（FFI 面完全不受影响，`im_sdk.h` 只
追加错误码 6）。内存契约也同源：native 面消费事件经
`ffi::reclaim_event`（unsafe 仍集中在 ffi.rs，native 保持全安全
代码）——「谁分配谁释放」跨语言通用，只是回收函数按消费面各配一个。

## 五、Tauri 桌面端：诚实边界（壳不真搭的账单）

roadmap 阶段 12 的标题是「桌面端（Tauri）+ E2EE」。实做后拆成两问：
桌面端到底承诺什么技术内容？壳的成本与它成正比吗？

**桌面端真正要学的技术承诺**是：桌面端后端如何以 Rust 原生形态
消费 SDK（长连接、事件泵、生命周期）——这正是 §四交付的内容，
Tauri 壳只是它的消费者。壳本身的增量价值（窗口化、系统托盘、原生
通知、自启动）全是 **OS 集成层**的活，不触碰 IM 协议栈——而项目
里真正承载 UI 的是阶段 5~9 的 Vue 前端，Tauri 壳里加载它是唯一
自然路径。

**成本账单**（不搭壳的理由，逐条可查证）：

- tauri 2.x 依赖树（wry/tao/webview2-com/…）冷编译 15+ 分钟起，
  与本阶段协议层交付的收益不成比例；
- Windows 壳要 WebView2 运行时判定 + 签名 + 更新管线，每条都是
  部署工程问题，属阶段 14（开源工程化/CI）的射程；
- 项目先例：阶段 9 LiveKit 部署同样走「机制交付 + 环境诚实跳过」
  （docs/15），本阶段沿用同一纪律。

**「如果搭」的蓝图**（机制已全部就位，路径留给后续）：

```rust
// tauri 后端：TypedSdkClient 就是为这一刻设计的
#[tauri::command]
async fn send_message(state: State<AppState>, to: u64, content: String) -> Result<(), String> {
    state.client.lock().unwrap()   // AppState 里是 TypedSdkClient<Connected>
        .send(to, content.as_bytes())
        .map_err(|c| error_string(c).to_owned())
}
// 事件泵：轮询线程 → window.emit("im-event", ...)，前端 event loop 按帧消化
// （docs/17 Q5 说的「GUI 主循环天然适合轮询」——poll_event + emit 即可）
```

前端零改动复用 Vue 构建产物（`dist/web`），`tauri.conf.json` 指向
它；Rust 侧唯一的胶水就是上面十几行 command + 一条泵线程。

## 六、测试与验证

| 包 | 用例 | 要点 |
|---|---|---|
| im-crypto | 23 项：tls 3 + x3dh 8 + ratchet 9 + error 2 + lib 1 | 攻击用例四类（伪造/移植/篡改/错投）+ 乱序 + 组合 |
| im-transport | 41 项（含 tls 5） | 既有 36 项回归零改动——泛型化的兼容性证据 |
| im-server | 全部存量 + tls_demo 实跑 | serve_connection 泛型化对明文路径零影响 |
| im-sdk | 15 项 + compile_fail doctest | 类型状态主线/拒绝归还/超时归还；错误码表全覆盖 |
| workspace | `cargo test --workspace` 全绿 | clippy 零警告 + fmt 干净收口 |

## 七、设计模式实战（对照 roadmap 4.5）

| 模式 | 在本阶段的形态 |
|---|---|
| **装饰器** | `Connection<S>`：帧协议装饰任何字节流，TCP 是默认被装饰者，TLS 是新顾客——阶段 2 的 UML 在阶段 12 变成泛型签名 |
| **类型状态** | `TypedSdkClient<Disconnected/Connected>`：状态机阶段进类型，`send` 的存在性即门禁；compile_fail doctest 让「编译期保证」本身被测试 |
| **依赖倒置** | `GatewayStream` trait：网关声明自己要什么，TcpStream/TLS 流来实现——网关代码零改动复用 |
| **错误归还** | `wait_connected` 失败交还 `(client, code)`：失败不吞资源，所有权语义替调用方兜底 |
| **零大小标记** | `Disconnected`/`Connected` + `PhantomData<fn() -> State>`：类型状态零运行时成本的标准形态 |

## 八、已知取舍（诚实的账单）

- **TLS 证书是演示口径**：进程内生成、不落盘、不接 KMS/ACME——
  生产要 Let's Encrypt 或私有 CA 管线，属部署工程；
- **M1 压测口径**：99,969 连接基线建立在 Owned 半部上，BiLock 版
  未重测（理论近零，无实测背书不冒充）；
- **E2EE 信任模型 TOFU**：MITM 风险记档未解决（SAFETY NUMBER 是
  产品功能不只是密码学）；
- **错误码 6 追加**：`IM_SDK_ERR_REJECTED` 数值已进 `im_sdk.h`，
  发布后只增不改（错误码即 API）；
- **`skip_to` 状态归属**：按值收链返回新链——多拷一次 32 字节，
  换「状态归属无歧义」，值。

## 九、下一步（阶段 13 预告，已兑现）

传输层拼图的最后一块：QUIC（quinn）——TLS 1.3 内建、0-RTT、多路
复用、连接迁移，以及挂载盘设计（IM 协议与 QUIC stream 的映射）。
阶段 12 的 `GatewayStream` 已经把网关对流类型的依赖抽干净，QUIC
流实现同一个 trait 就能进网关——泛型化在这先付了一笔，后面连着
收利息。docs/19 立此为证。

**阶段 13 已兑现（见 docs/19）**：多路复用与网关零改动收利息如期
兑付（`QuicStream` 实现 `GatewayStream`，测试钉死无队头阻塞）；
两处如实对账：0-RTT 与连接迁移的 API 未暴露（quinn 能力面有，
本模块未开，docs/19 §六记录）；挂载盘交付的是语义层（im-mount：
IM 数据 → FS 视图映射），预告里「IM 协议与 QUIC stream 的映射」
的措辞以实际交付口径为准。

## 十、面试题与标准回答

**Q1：已经有 TLS 了，为什么 IM 还要 E2EE？**

答：信任边界不同。TLS 的终点是服务器——服务器看得见全部明文，
服务器的攻破/传唤/内部作恶都是单点。E2EE 把终点推到对话双方：
服务器只剩密文与路由元数据。两层不是重复，是两道面对不同威胁
模型的墙；Telegram（传输层）与 Signal（端到端）的产品哲学分野
就在这条线上。引申：E2EE 之后服务器还能做什么？路由、离线暂存
密文、群成员管理——**能做的都在「不该知道内容」的约束下设计**。

**Q2：双棘轮的两个棘轮分别解决什么？少一个行不行？**

答：对称棘轮（每条消息 CK→MK）解决消息级前向保密：拿到第 N 条
的密钥推不出之前之后的消息密钥。DH 棘轮（每轮往返）解决轮级
自愈：某条链密钥泄露后，对方一下轮回复就把根密钥洗掉。少对称
棘轮，一条密钥泄露拖垮整链；少 DH 棘轮，双方密钥永不再换。两者
配合的关键洞察是**通信的双向性本身就是更新密钥的节拍器**。

**Q3：GCM 的 nonce 为什么从消息密钥派生而不是随机发过去？**

答：nonce 重用对 GCM 是灾难（同 key+nonce 两条密文 XOR 即撕开
认证）。从 MK 派生（`HKDF(MK) → (key, nonce)`）让「换密钥必换
nonce」成为构造保证——对称棘轮保证每条消息 MK 唯一，nonce 唯一
免费搭车；随机 nonce 则要自己管计数器/防重放，纪律成本转嫁给
每个实现者。**能用构造表达的安全性质，不要用纪律表达**。

**Q4：类型状态模式什么时候适用？**

答：两个前提。状态迁移路径**有限且单向**（未连接→已连接，不是
「随时来回跳」）；消费方类型系统能承载这个区分。第二个前提是
docs/17 的核心教训：同一份实现，C ABI 面只能降级为运行时检查
（`void*` 无处安放类型参数），rlib 面才能升到编译期——**模式是否
适用由消费方的类型系统决定，不由实现方的意愿决定**。还要诚实
划定编码范围：类型编码协议阶段（握手完成与否），不编码链路活性
（断线是运行时事件，类型不能随网络回退）。

**Q5：rustls 为什么显式钉 ring provider？**

答：两层理由。依赖健康：aws-lc-rs（默认）在 Windows 要 NASM/
CMake 构建，CI 与下游用户环境的失败面大；ring 的构建链在三大
平台零外部工具。工程边界：配置用 `builder_with_provider` 显式
传入，不 `install_default_process_cryptography` 抢进程全局——
库不该替宿主做 provider 决策，进程全局的默认只能有一个人说了算。

**Q6：跳过密钥缓存的键为什么要带公钥？**

答：缓存的目的是「旧链迟到的消息也能解」。旧链的标识不是序号
（每条链都从 0 开始数）而是**生成那条链时的对端棘轮公钥**——
带公钥做键，不同轮的旧链天然分桶，DH 棘轮推进后按公钥整桶丢弃
即可。如果只按序号做键，不同轮的同序号消息会在缓存里相撞。键
的设计要看「什么在唯一标识这批数据」，不是「什么是手头的序号」。

---

*阶段 12 完成于：`im-crypto`（TLS 材料 + X3DH + 双棘轮，23 项测试
全绿）、`im-transport`（Connection 泛型化 + GatewayStream + TLS
装配，41 项测试全绿 + tls_demo 实跑）、`im-sdk::native`
（TypedSdkClient 类型状态 API + 错误码 6，15 项测试 + compile_fail
doctest）、Tauri 壳诚实边界（§五账单 + §五蓝图）。踩坑收录于
docs/20：skip_to 状态归属、dh_remote 忘写回、b 字节串非 ASCII、
ZeroizeOnDrop 与 HashMap、Hmac trait 歧义、rcgen 0.14 API、
aws-lc-rs 构建链、rustls provider 边界。*
