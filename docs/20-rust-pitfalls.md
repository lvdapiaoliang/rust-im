# 20 - Rust 全栈踩坑与填坑实录（含业务开发常见错误）

> 本文档收录 rust-im 项目从阶段 0 到收尾的**真实踩坑记录**：每一条都实际发生过、
> 实际排查过、实际修复过——没有一条是"假想中可能踩"的凑数条目。
> 写法统一为：**现象（表象/报错原文）→ 根因 → 修复 → 教训与适用边界**。
>
> 与其他文档的区别：01~19 讲"怎么把事情做对"，本文档讲"做错的那些瞬间"。
> 面试里"你踩过什么坑"比"你会什么"更有说服力——因为坑是编不出来的。

## 一、怎么读这份文档：坑的分级

不是所有坑都一样贵。按"发现成本"分三级：

| 级别 | 谁拦住了它 | 成本 | 本项目实例 |
|------|-----------|------|-----------|
| 便宜 | 编译器（E 系列错误） | 秒级 | `unwrap_or_default` 链式调用（§2.5）、闭包捕获（§7） |
| 中等 | 测试 / clippy | 分钟级 | base64 填充符（§4.4）、计数对账（§4.3）、`--fix` 插错位置（§2.2） |
| 昂贵 | 谁都没拦住，靠人肉推理 | 小时级起步 | ShutdownTx 静默杀服务（§3.1）、去重键复用丢消息（§4.1）、陈旧 rmeta 幻影错误（§2.1） |

三个规律（也是本文档的组织逻辑）：

1. **越贵的坑，表象与真因脱节越远**——§4 专门收集"静默 bug"；
2. **贵的坑几乎都跟"生命周期"有关**——对象什么时候死、键什么时候失效、缓存什么时候过期；
3. **工具的自动修复会引入新问题**——`--fix` 插错反引号（§2.2）、缓存陈旧（§2.1）。

---

## 二、工具链与构建

### 2.1 cargo check / IDE 报幻影类型错误，build 与 test 却全绿【阶段 9 后，2026-09-25】

**现象**：RustRover 里 [sink.rs](../crates/im-server/src/sink.rs) 报两个 E0308
"mismatched types"，提示 `TrySendError<_>`（tokio）与 `sink::TrySendError`
"相似但不同的类型"。但同一份源码 `cargo build` 成功、`cargo test` 72 项全绿。

**根因**：`cargo check`（rust-analyzer 同管线）只消费依赖 crate 的 `.rmeta`
（元数据），`cargo build` 用 `.rlib`（完整产物）——**两条管线的缓存是分开的**。
IDE 后台 check 与命令行构建并发竞争，im-transport 的 `.rmeta` 停留在
"加固有 `try_send` 方法之前"的旧版本。方法解析时元数据里没有固有方法，
编译器把 `ConnectionHandle::try_send(self, frame)` 绑定到 trait 方法上——
错误的类型推导产生了完全"合逻辑"的假报错。

**修复**：`cargo clean -p im-transport` 后重跑 check，错误消失，零代码改动。

**教训**：**同源码下 build/test 绿、check/IDE 红 = 缓存问题，不是代码问题**。
先怀疑缓存再怀疑代码；避免 IDE 后台 check 与命令行构建同时跑。
做了双 crate 最小复现验证代码结构没问题——排查幻影错误时，最小复现是
区分"代码问题"与"环境问题"的手术刀。

**第二次实遇（2026-09-25，阶段 14）**：同款剧本换个演员——quic_demo.rs
报 E0599「no method named `quic_server_config` found for struct
`TlsMaterial`」，而该方法真实存在（im-crypto tls.rs）、312 项测试全绿、
quic_demo 实跑通过——im-crypto 的 `.rmeta` 陈旧（记录的是加方法之前的
旧 API）。`cargo clean -p im-crypto -p im-transport` 重建后消失，零代码
改动。**同一个坑两次实遇说明它不是偶发**——写进排查清单第一条：
IDE 红 + 命令行绿 → 先 clean 依赖链再说，别改代码。

### 2.2 `cargo clippy --fix` 把反引号插错位置【阶段 9】

**现象**：12 个 pedantic `missing backticks` 警告，用
`cargo clippy --fix --allow-dirty` 自动修复后，一处文档注释变成了
`` `「`谁能进这间房 `` ——左书名号被包进了反引号里，中文标点被机械规则肢解。

**根因**：`--fix` 按警告行列位置机械插入反引号，不理解中文标点边界
（警告框定的"标识符"起止与中文排版的词边界不一致）。

**修复**：自动修完后逐个 diff 检查，手工修正错位处。

**教训**：`--fix` 之后的代码**必须像 review 人写的代码一样 review 一遍**——
自动修复的语义正确率不是 100%，尤其穿过非 ASCII 文本时。

### 2.3 pedantic 的文档纪律：`# Panics` 节与裸 cast【阶段 9】

**现象**：零警告门槛下，`sign_meeting_token` 因内部 `expect` 被要求补
`# Panics` 文档节；测试里的 base64 位拼装触发 4 个 `cast_possible_truncation`。

**修复与取舍**：
- 补 `# Panics`：写明"claims/header 序列化失败时 panic"，并注明与全项目
  「发号器保证」同款（不可达的 panic 是文档契约，不是隐患）；
- 测试里的 cast：`#[allow]` + 注释论证（"6 位值拼进 24 位窗口再按 8 位切，
  掩码外无位进高位"）——**allow 必须带理由**，裸 allow 等于关掉警报。

**教训**：pedantic 的价值不在"消除警告"而在**逼你把 unsafe-ish 的决定写下来**。
项目纪律：`unwrap/expect/allow` 三件套，每一个都要么可论证、要么带注释。

### 2.4 临时文件放仓库根目录，被提交带入远端【阶段 9】

**现象**：调试 clippy 输出中文乱码时 `Out-File` 了 `clippy-out.txt` /
`clippy2.txt` 在仓库根目录，用户当天中午自行提交时被带入
（dda0d6d），且已推送。

**修复**：工作区删除 + 下一次提交收尾；已推送的历史里文件仍在
（不改写已推送历史——保持现状原则）。

**教训**：**调试产物永远别落仓库根目录**——用 `%TEMP%`，或至少确认
`.gitignore` 覆盖。提交前 `git status` 是最后一道闸。

### 2.5 `unwrap_or_default()` 返回 String 后再链式 `unwrap_or_else`【阶段 9】

**现象**：编译错 E0599：`String` 没有 `unwrap_or_else` 方法。

```rust
// 写错的形状：var() 返回 Result<String, _>，unwrap_or_default() 已拿到 String
std::env::var(key).unwrap_or_default().unwrap_or_else(|_| ...)
```

**根因**：`unwrap_or_default()` 是 `Result` 的方法，调用后已经拿到 `String`——
而 `unwrap_or_else` 也是 `Result` 的方法，对 `String` 调用自然不存在。

**修复**：改成显式 match 闭包 `read(key, fallback)`，顺带把**空字符串也视为
未设置**——否则"环境变量配成空值"会伪装成"连一个空地址"。

**教训**：`unwrap_or_*` 家族全在 `Result/Option` 上；链式调用前先想清楚每一步
的返回类型。环境变量解析的边界情况（未设置 vs 空串）比 API 拼写更容易漏。

### 2.6 feature 集不同导致 cdylib 互相覆盖：UnsatisfiedLinkError 假象【阶段 11】

**现象**：JNI 冒烟报 `UnsatisfiedLinkError: 'java.lang.String im.sdk.Sdk.nativeVersion()'`
——库加载成功、符号找不到（比「库不存在」迷惑得多：明明 dll 在、路径对）。

**根因**：cargo 的构建缓存按 feature 集**整体区分**。先
`cargo build --features jni` 产出带 JNI 导出的 dll，随后
`cargo run --example demo_server`（不带 jni）重建了同一个
`im_sdk.dll`，**静默覆盖**成无 JNI 导出的版本。看 dll 时间戳才对上。

**修复/纪律**：SDK 打包入口（`cargo xtask sdk`）钉死 feature 集；
调试这类「符号找不到」先查产物时间戳是不是被别的构建命令动过。

### 2.7 edition 2024：`no_mangle` 是 unsafe 属性，必须写 `#[unsafe(no_mangle)]`【阶段 11】

8 处 `#[no_mangle]` 全部编译报错——edition 2024 把 `no_mangle` 升级为
unsafe attribute（它能把任意符号暴露给链接器，副作用不受隔离），
语法上必须显式承认风险。同类还有 `#[unsafe(no_mangle)]` 的兄弟
`export_name`/`link_section`。看到 `unsafe attribute` 字样的报错，
照着加 `unsafe(...)` 包裹层即可，不是设计问题。

### 2.8 rustls 0.23 默认 provider 是 aws-lc-rs：构建链失败面 + 进程全局边界【阶段 12】

**现象**：按默认 feature 引 rustls，Windows 上会拉起 aws-lc-rs 的
NASM/CMake 构建链——CI 与下游用户环境的失败面直接放大。

**修复**：两层。依赖健康：`default-features = false` + `features =
["ring", "std", "tls12", "log"]` 显式钉 ring（三大平台零外部构建
工具）；工程边界：配置一律 `builder_with_provider(...)` 显式传入
provider，**不调** `install_default_process_cryptography`——库不该
替宿主做 provider 决策，进程全局默认只能有一个人说了算（宿主可能
自己钉了别的 provider，覆盖是静默 bug）。

**教训**：密码学库的「默认 provider」是构建链决策不是密码学决策；
要发布给别人当依赖的库，任何进程全局状态都不要碰。

### 2.9 `b""` 字节串字面量只收 ASCII：一段中文明文测试爆 52 个编译错误【阶段 12】

`b"你好"` 直接编译错误——byte string literal 只允许 ASCII。含中文
的测试明文一律写 `"你好".as_bytes()`：语义完全相同（字符串字面量
本来就是 UTF-8 字节），字符集约束不同。

**教训**：`b"..."` 与 `"...".as_bytes()` 不是风格差异，是**字符集
差异**；看到一片 E0xxx 报错先检查是不是非 ASCII 混进了字节串。

### 2.10 rcgen 0.14 签发 API 变形：`Issuer` 新类型 + `signed_by` 变参【阶段 12】

**现象**：按 rcgen 0.12/0.13 的记忆写「证书对象调 `serialize_pem`」
与「`signed_by(key, issuer_cert, issuer_key)` 三参」都不在——0.14
把签发者身份收进 `Issuer::from_params(&ca_params, &ca_key)` 新类型，
叶子签发变两参 `signed_by(&key, &issuer)`，PEM 序列化搬家到
`cert.pem()`。

**根因**：0.x 语义化版本允许 minor 内 breaking change（还没承诺
稳定），证书库在大版本临近期频繁变形。

**教训**：报「方法不存在/参数不匹配」且是三方库代码时，**先查该
版本 changelog 再怀疑自己**；workspace 统一锁版本，让「文档记忆」
与「实际版本」至少有一个是确定的。

### 2.11 quinn 不收裸 rustls 配置 + connect 是两段式错误：三个 E0277 一批到位【阶段 13】

**现象**：把 `rustls::ServerConfig` 直接塞给
`quinn::ServerConfig::with_crypto(Arc::new(config))`、客户端同理，
两路都报 E0277「trait bound `rustls::ServerConfig: quinn::crypto::
ServerConfig` is not satisfied」；修完这个，`endpoint.connect(...)?`
又报第三个 E0277——`?` 不认 `ConnectError`。

**根因**（两个独立的知识点）：
1. quinn 要的是包装类型 `QuicServerConfig`/`QuicClientConfig`
   （路径 `quinn::crypto::rustls::*`）——它额外携带 QUIC 首包
   加密需要的初始套件（握手前的包也要加密，TLS 套件是握手后才有
   的事，鸡生蛋问题由固定 AES-128-GCM-SHA256 解决）。公开入口是
   `TryFrom<Arc<rustls::…>>`，转换会校验 TLS 1.3 已启用（QUIC 硬性
   要求）——没有公开的 `new`，**TryFrom 就是官方设计的入口**；
2. `connect()` 返回的不是连接而是 `Connecting`（握手 future），
   两步各自可错：参数错误在同步步（`ConnectError`）、握手失败
   在 await 步——**`?` 只认自己那一段的错误**，两处都要 `map_err`。

**教训**：包一层再收是加密/传输库的常见形态（额外携带上下文），
找公开构造函数时 **TryFrom/From 实现也是「构造器」**，先翻源码的
`impl TryFrom`；两段式 API 的错误处理要分段各管各的，`?` 不会
替你跨段转换。

---

## 三、async / Tokio 运行时

### 3.1 ShutdownTx 过早 drop：静默杀死服务端（双重坑）【阶段 2→3，最贵的一个】

**现象**：集成测试里客户端"永远连不上、无限 Disconnected 重连"；另一处是
每条"正常"连接的网关秒退。表象全部指向网络层，真因不在网络层。

**根因**：关停信号基于 tokio watch 通道，语义约定「**所有 sender drop =
视为已关停**」（`ShutdownRx::wait` 在 sender 全部消失时立即返回）。
两处踩坑：
1. 测试辅助函数 `let (addr, _sessions, _shutdown) = spawn_server(...)`——
   函数返回即 drop 唯一的 ShutdownTx → accept 循环当场退出；
2. 手动 accept 循环里 `_tx` 留在外层作用域、`rx` move 进连接 task——
   else 分支结束 `_tx` 即 drop → 这条连接的 `select!` 立即命中 shutdown 分支。

**修复**：
- 测试脚手架：`std::mem::forget(shutdown_tx)`（watch sender 极小，测试进程
  内泄漏无害）或返回 guard 让测试持有；
- 每连接通道：`tokio::spawn(async move { let _keep_alive = shutdown_tx; serve_connection(...).await })`
  ——**sender 随 task 保活**。

**教训**：任何「watch/广播通道 + sender 全 drop 视为关停」语义都会踩此坑
（含 CancellationToken 类设计）。判别特征：服务端"从未收到任何事件"且客户端
退避重连日志正常。**排查优先看信号对象的生命周期，再看网络层**。
"谁持有 sender"是资源所有权问题，编译器管不了 drop 的**时机**——
Rust 把内存安全兜底了，但"过早释放"依然是逻辑 bug。

### 3.2 `try_send` vs `send`：满则挂起如何变成全群反压【阶段 7 设计前的推演】

**现象（推演出的坑，写进了设计）**：2 万成员的群扇出若用 `send`（满则挂起），
任何一个慢消费者（出站通道打满不读）都会让扇出 actor 卡在对一个人 `await`——
其余 19999 人跟着排队。**一个人的慢变成全群的慢**。

**修复**：[`FrameSink::try_send`](../crates/im-server/src/sink.rs) 非阻塞投递 +
两态错误（`Full` = 跳过该接收者；`Closed` = 降级离线投递）。压测验证：
2000 个容量 1 的慢消费者混进 2 万人群，P99 扇出延迟与零慢消费者基本一致
（数据见 docs/13）。

**教训**：通道 API 的"满则挂起"是**把背压决定权留给调用方**——单聊场景这是
美德（消息不丢），扇出场景这是灾难（队头阻塞）。同一项目里两种选择都对，
关键是**看清调用方是谁**。

### 3.3 通用清单：spawn 的 `'static` 陷阱与 select! 的取消安全

未实际踩、但在代码评审时重点核查过的两个高频陷阱（面试常问）：

- **`tokio::spawn` 要求 `'static` future**：闭包捕获的引用活不过 task 生命周期——
  `async move` + 先 `clone` 再进闭包是标准解法。本项目所有 spawn 的闭包
  都按"拿所有权"纪律写；
- **`select!` 的取消安全（cancel safety）**：被取消分支的 future 会被丢弃——
  若某分支已经"读了一半"再被取消，下次重进会丢状态。心跳/读循环里只放
  **可重入**的操作（`read_frame`、`rx.recv` 这类"要么完成要么像没发生"的原语）。
  判别口诀：先 poll 内部状态再返回 Pending 的操作都不取消安全
  （典型反例：`read_exact` 读了一半）。

### 3.4 嵌套 runtime：block_on 桥接与 tokio 测试环境天然冲突【阶段 11】

**现象**：`#[tokio::test]` 里调 SDK 同步入口，进程直接 abort
（`STATUS_STACK_BUFFER_OVERRUN`，连 panic 消息都没有）。

**根因**：`SdkClient` 每实例一个专属 `tokio::Runtime`，同步入口内部
`block_on`——在 tokio 上下文里再建/再进 runtime 是硬禁（
"Cannot start a runtime from within a runtime"，abort 级）。

**修复/纪律**：这不是缺陷而是定位——SDK 的宿主（C/Java 进程）
**没有环境 runtime**。测试照真实调用方形态写：普通 `#[test]` +
服务端挂独立 runtime（`TestServer { _rt, addr, .. }`，drop 即停）。
测试形态 = 生产形态，反而是福。

### 3.5 `tokio::timeout` 三层语义写反：编译全绿、测试当场抓住【阶段 11】

`timeout(d, recv)` 的返回要剥两层：外层 `Result` 是**超时与否**，
内层 `Option` 是**通道关没关**——`Ok(Some(ev))` 收到事件；
`Ok(None)` recv 完成但通道关闭；`Err(Elapsed)` 超时。最初把
`Ok(None)` 当超时、`Err` 当通道关闭（直觉以为「Err = 异常 = 坏消息」），
导致客户端停止后 `ERR_STOPPED` 永远发不出来。

**纪律**：嵌套 `Result<Option<T>, E>` 的每个分支都要写测试覆盖
（空载荷事件的超时路径专门补了断言）；`timeout` 的语义是「包住一个
future 看它跑多久」，它自己**不区分**内层为什么完成——分支语义
要自己列全。

---

## 四、静默 bug：表象与真因脱节（最贵的一节）

### 4.1 去重键分配器与存储分离：去重机制反成丢消息凶器【阶段 4 e2e】

**现象**：e2e 测试中 Bob 重连后发的消息，Alice"永远收不到"。

**根因**：测试脚手架每次上线给客户端**全新临时目录**，本地库的
`client_msg_id` 单调计数器随目录归零。Bob 重连后新消息的 `client_msg_id=1`
撞上 Alice 去重窗口中的旧记录 `(from=Bob, client_msg_id=1)`——被当重复
**静默丢弃**。去重机制工作正常，恰好是它吞掉了消息。

**修复**：同一 user_id 跨重连/重启必须复用同一 data_dir（分配器与存储
同生命周期）；排查手段是给接收路径加"丢弃时打印"探针——静默丢弃类 bug
只能靠日志看见。

**教训**：所有「至少一次投递 + 接收端去重」架构（IM、MQ 幂等消费、同步协议）
都适用。**键的唯一性来自"单调计数器 + 持久存储"的绑定**——任何让两者分离
的操作（换目录、换库、清缓存）都会导致键复用。审查方法不是断点，
是审查"键的唯一性来源是否与存储同生命周期"。

### 4.2 雪花 ID 超过 JS Number 精度：JSON 数字串化约定【阶段 5 立的规矩】

**现象（提前避开的坑，立为全项目约定）**：雪花 ID 是 64 位整数，
最大可达 2^63-1；而 JavaScript `Number` 是 IEEE 754 双精度浮点，
**安全整数上限 2^53-1**。ID 直接作为 JSON number 下发前端，
`9007199254740993` 会变成 `9007199254740992`——静默错位，不报错。

**修复（约定）**：全项目所有超过 32 位的 ID 在 JSON 里一律**字符串化**
（serde `u64` ID 字段串化、阶段 9 的 JWT `sub` 也串化），前端拿 string。
TUI/二进制协议路径不受影响（不经过 JS）。

**教训**：跨语言边界的类型断崖（u64 → Number）是静默 bug 的沃土。
**在边界层定类型契约**（串化），而不是指望每种语言都小心。

### 4.3 压测计数对账：对不上即作废【阶段 7 方法论】

**做法**：扇出路径三路计数（delivered/skipped/offline）之和必须等于
`fanned × 成员数`，对不上直接报错退出、压测结果作废
（见 [im-bench report](../crates/im-bench/src/main.rs) 的对账断言）。

**教训**：压测工具自身必须有**守恒律校验**——性能数字可以慢，但不能建立在
"可能丢了消息"的地基上。这也是压测报告可信度的来源：数字旁边永远放着
计数对账行。

### 4.4 base64 解码器不认填充符：官方向量过、自家向量挂【阶段 9】

**现象**：JWT 结构自验测试失败："非法字符"。HMAC 三组 RFC 4231 向量全过，
base64url 编码的 RFC 4648 向量也过——偏偏自家签发的 token 解不开。

**根因**：测试给载荷段补了 `=` 填充再解码，而手写解码器遇 `=` 报非法字符。
加密库代码常按"教科书最简形态"实现，教科书不画的字符就不认。

**修复**：解码循环跳过 `=`（编码侧保持无填充输出，解码侧宽容——
与业界 JWT 实现行为一致）。

**教训**：手写编码类代码时，**编码/解码的不对称宽容度**要显式定契约
（编码严格、解码宽容）。自签自验的测试要故意喂"规范之外、现实之内"的输入。

### 4.5 接收窗静默楔死：超窗丢弃 + 心跳掩盖，连接健康而上行全死【阶段 10 压测抓出】

**现象**：weak-link 双向 10% 丢包 + 100ms RTT 下，200 条消息只确认 58 条、
到达率 29.5%——而理论放弃率是 2×10⁻⁶（差 5 个数量级）。

**根因链**（五步，每步有代码与数据背书）：
1. 帧级丢包在服务端 `DedupWindow`（位图窗口，尺寸 64）**前方留下洞**；
2. 洞**永不回填**：应用层重传用的是**新 seq**（seq 是帧序号不是消息 ID，
   `client_msg_id` 才是重传核销键）；
3. 洞后第 64 帧起全部 `Verdict::TooFar`（超窗）；
4. 原实现超窗**静默丢弃**（`continue`）——窗口永久楔死在洞口；
5. 最阴险：心跳 Ping/Pong 不参与 seq 去重（设计如此），连接表面健康、
   PING-PONG 正常，**上行业务帧全军覆没而无人报警**。

数据形状反向定位：二值分裂（142×8 重试全灭 + 58 首试即过 = 1,194 上行帧，
无中间态）且 58 ≈ 64（窗口）× 0.9（下行存活）——两个数字都指向去重窗口。

**修复**：`DedupWindow::resync(seq)`——超窗从"丢弃"升级为"重同步"
（以到达帧为新基准重建窗口；dedup 文档本来就建议这么做）。被跳过区间的
重复投递由业务层按 `client_msg_id` 去重兜底，与「至少一次」兼容。
修复后：200/200 确认、0 放弃、到达率 90.0% = 200×下行单次 0.9。

**教训**：
- **静默丢弃是静默失败的温床**：丢弃分支必须有自愈路径或计数器；
- **旁路通道（心跳）健康 ≠ 主通道（业务）健康**：监控要采样业务帧，
  不能只看心跳；
- **实测与理论差数量级时，拒绝采信"系统边界"结论**——两个判别实验
  （零丢包/零延迟）+ 数据形状比任何文档都诚实。完整排查过程见 docs/16 §5。

### 4.6 双棘轮状态归属三连坑：按序全绿、乱序炸穿【阶段 12】

**现象**：双棘轮单测第一轮 7 用例过 7（全按序）；补乱序用例后当场
2 挂（GCM 认证失败）——三个独立的状态归属缺陷被按序路径全部掩盖。

**三连根因**：
1. `dh_ratchet_recv_side` 推进后**忘写回** `self.dh_remote`——后续
   「对端是否又换了钥匙」的判断和旧链排空都依赖这个字段；
2. `skip_to` 只在函数内部推进链密钥**局部副本**、不回写，decrypt
   却用跳过前的旧 CK 派生本条消息的 MK——按序时 `from == until`
   循环体不执行，bug 隐形；乱序（2,0,1 投递）当场炸穿。修正：
   **按值收链、返回推进后的链**，调用方拿返回值派生；
3. 发起方首次收到回信时 `dh_remote=Some(SPK)` 但 `chain_recv=None`，
   「有旧远端公钥必有旧接收链」的 expect 前提不成立——旧链排空改
   `if let (Some, Some)` 双守卫。

**教训**：带内部状态的迭代助手，**要么自己管状态，要么把新状态
还给调用方**，「原地假设」两头不靠；状态机缺陷对按序用例的遮蔽力
超乎直觉——三个缺陷一个都抓不住，**乱序/交错用例是状态机的强制
测试项**。详见 docs/18 §3.4。

### 4.7 LRU 容量 0：先淘汰后插入的顺序让「插入即淘汰」落空【阶段 13】

**现象**：手写 LRU 的容量 0 边界用例挂了——按「先淘汰再插入」的
顺序写，容量 0 时表尾为空、淘汰落空，新条目照常插入「容量 0」的
缓存：**一个什么都不该存的缓存存了一条**。

**根因**：淘汰的前提是「有东西可淘汰」，而容量 0 的缓存永远为空
——「满时先淘汰」的思考习惯在容量为 0 的边界上前提不成立。

**修复**：把淘汰挪到插入之后——超容才踢。容量 0 时刚插入的项
自己就是表尾，「插入即淘汰」自然发生；容量 n 时先到 n+1 再踢回
n，两种情况同一条代码路径。代价可忽略：链表短暂超容一个节点。
顺带修正了对 slab 上界的认知：**稳定上界是容量+1**（首次淘汰
触发前多插了一个），不是容量。

**教训**：边界条件（空/满/零/单元素）是数据结构的强制测试项，
和 docs/18 §3.4「乱序是状态机的强制测试项」同一条纪律；「先 X
再 Y」的顺序假设要在每个边界上重推一遍——**边界的语义可能与常规
路径完全相反**。

### 4.8 并行集成测试共用默认 `machine_id`：雪花 ID 撞库，偶发失败单跑必过【阶段 14 CI】

**现象**：`cargo test --workspace` 偶尔挂一个 web 集成测试，报
`23505 唯一约束违反 "friend_requests_pkey"，键 (id)=(…) 已存在`；
单独 `cargo test -p im-server --lib <那个测试>` 又必过。挂哪个、
挂不挂全看运气——典型的 flaky test 签名。

**根因**：雪花 ID 的唯一性契约是「**一个 `machine_id` 只属于一个
发号器**」（见 `snowflake::concurrent_generators_produce_unique_ids`——
那个测试特意给每个线程不同的 `machine_id`）。但 web 各测试模块的
脚手架都写 `Sessions::new(SessionConfig::default())`，`machine_id` 全是
默认值 1。Rust 测试默认**多线程并行**，于是同一进程里并存十几个
`machine_id=1` 的独立发号器；两个测试在同一毫秒各发**首号**
（sequence 都从 0 起）→ `assemble(同一时间戳, 1, 0)` 算出
**同一个 ID** → 先后 INSERT 撞主键。「单跑必过」正是因为单跑时没有别的
发号器跟它抢同一毫秒。

**修复**：测试脚手架统一走 `db::testing::test_sessions()`——进程级
`AtomicU64` 计数器给每个实例发不同 `machine_id`（`% 1024`，10 位槽
远超测试数量），从根上让并行发号器互不重叠。修复后连跑 5 次
全量并行测试全绿（修复前同一条命令就会偶发挂一个）。

**教训**：①「偶发失败 + 单跑必过」几乎总是**并行测试间的隐藏
共享状态**，先查「什么东西被多个测试默认为同一个值」；② 全局
唯一 ID 生成器进测试时，唯一性的**前提**（每实例独占 `machine_id` /
独占号段）必须显式满足，不能靠「测试之间应该不会那么巧」——
CI 三平台矩阵会把这种侥幸放大成反复无常的红。这也是阶段 14
CI 落地抓出的第一个真 bug：**把测试搬进 CI 的价值，一半在于逼出
本地侥幸通过的 flaky。**

---

## 五、前端与跨语言边界

### 5.1 命令式对象不进 Vue ref：深度代理破坏内部状态【阶段 8/9，两次落地】

**现象**：WebRTC `RTCPeerConnection`、LiveKit `Room` 这类对象放进 `ref()` 后
行为诡异（内部状态被 Vue 的响应式代理包住，库内部的 `this` 自省失效）。

**根因**：Vue 3 的 `ref`/`reactive` 对对象做**深度 Proxy 包装**——浏览器/三方库
拿到的不再是原始对象，身份比较（`===`）、内部弱引用、私有字段都会被破坏。

**修复（纪律）**：
- 命令式对象（`Room`、`RTCPeerConnection`、`MediaStreamTrack` 的容器）用
  **普通模块变量**持有，不进 ref；
- 进 ref 的只有**数据**（状态枚举、成员表、`MediaStream`）；
- DOM 桥接用命令式 watch：`watch(数据) → el.srcObject = stream`。

**教训**：响应式系统与命令式库是两种世界观，边界要显式管理。
判别口诀：**"会被 UI 观察的进 ref，被 API 驱动的进变量"**。

### 5.2 事件与 DOM 挂载的时序赛跑：双向兜底【阶段 9】

**现象**：MeetingView 里远端视频有时显示、有时黑屏——轨道订阅事件先到
还是 v-for 元素先挂，顺序不确定。

**修复**：两边都兜——模板 ref 回调（元素挂/卸时维护 `Map` 并立即挂流）+
`watch(remotes)`（数据变化时全表重挂）。任一先到都能收敛。

**教训**：UI 框架的生命周期回调与异步事件是**两个独立的时序源**，
"假设谁先谁后"就是 bug。双向兜底比用 `nextTick` 猜时序健壮得多。

### 5.3 状态的单一写入者纪律【阶段 9】

**做法**：meeting store 的 `remotes` 表**只由四个事件回调写入**
（TrackSubscribed 合入 / Unsubscribed 摘除 / ParticipantDisconnected 整卡删除），
UI 操作、清理路径都不得直接改它。

**教训**：并发状态多写入者 = 竞态的温床。宁可多写几个纯函数，
也不要让"谁都能改一下"。与后端扇出 actor 的"单一 task 独占状态"
（阶段 7）是同一条纪律在两种语言里的镜像。

### 5.4 打洞失败没有 TURN：换拓扑消灭一整类问题【阶段 8→9】

**现象（已知欠账）**：阶段 8 P2P WebRTC 在对称 NAT 下打洞失败率不可忽略，
本项目没部署 TURN（中继服务器有成本），docs/14 §八记为诚实欠账。

**修复**：阶段 9 切 SFU 拓扑（LiveKit）后，媒体本来就进服务器——
**TURN 的存在意义消失了**，欠账自动结清。

**教训**：有些问题不该被"解决"，该被**消灭**（换架构使问题不存在）。
对照：流量清洗解决 DDoS vs Anycast 架构稀释 DDoS。压测阶段（10）的
"调参扛量"与"改架构扛量"也是同一对选择。

### 5.5 Send 包装白做：闭包精确捕获只看字段路径，三连败后才找到唯一形态【阶段 11 FFI，最贵】

**场景**：事件泵要进 `std::thread::spawn` 的闭包，携带
`user_data: *mut c_void`（C 回调上下文）——裸指针不是 `Send`。

**三连败**：
1. newtype `struct UserData(*mut c_void)` + `unsafe impl Send`——
   仍报 E0277：闭包体里写 `user_data.0`，RFC 2229 精确捕获只捕获
   **字段路径**（还是那个裸指针），Send 包装白做；
2. 改模式解构 `let UserData(p) = user_data`——同样失败：解构被
   编译器归一化成字段捕获，不是「把整个结构体搬进去」；
3. **成功**：泵循环体提成独立函数 `pump_loop(events, callback,
   user_data)`，闭包只写 `move || pump_loop(events, callback,
   user_data)`——结构体按值**传参**，闭包被迫捕获完整结构体。

**教训**：`unsafe impl Send` 只声明资格，**捕获分析决定实际穿越的是
什么**——两者要一起设计；当你「明明用了整个结构体」却报裸指针
错误，那是精确捕获在拆你的字段。唯一可靠形态：让使用方式不可拆分
（按值传参）。验证手段就是 `cargo check`，编译器不认，资格声明就是
废纸。

### 5.6 `c_void` 双胞胎：`std::os::raw::c_void` ≠ `std::ffi::c_void`【阶段 11】

两个路径的同名类型**不是同一个类型**（历史兼容产物），混用报不兼容。
FFI 层统一 `use std::ffi::{c_char, c_void}`（与 `core::ffi` 是同一批）。
同理警惕：`std::os::raw::c_char` vs `std::ffi::c_char`。报「明明都是
c_void 却不匹配」时，先查 import 路径。

### 5.7 JNI 三件套：modified UTF-8、GlobalRef、线程 attach【阶段 11】

三个都是 JNI 规范级陷阱，一次集成全部踩齐（完整讲解见 docs/17 §4.4）：

1. **modified UTF-8**：裸 `GetStringUTFChars` 给的是 CESU-8 变体（NUL
   双字节、增补字符非标准），与真 UTF-8 不兼容——JNI 最著名的坑。
   jni-rs 的 `get_string` 内部经 cesu8 解回标准 UTF-8；冒烟用中文
   消息验证全链路无损；
2. **GlobalRef**：局部引用出不了原生调用帧，泵线程长期持有 Java 回调
   必须全局引用，且最终**显式 delete**（GC 不替你管 native 侧的全局
   引用）。回收顺序即安全：先 join 泵，再回收事件桥，反了就是
   use-after-free；
3. **线程 attach**：事件泵是普通线程，JVM 不认识——回调前
   `attach_current_thread()`（AttachGuard，drop 自动 detach，DerefMut
   暴露 JNIEnv）。顺带记录 JDK 27 新行为：`System.loadLibrary` 触发
   restricted native access WARNING（未来默认 block），真实集成要加
   `--enable-native-access=ALL-UNNAMED`。

---

## 六、环境与协作（Windows / PowerShell）

### 6.1 PowerShell 5 没有 `&&`，npm 要用 `npm.cmd`

`&&` 是 PowerShell 7+ 的特性；PowerShell 5 里 `npm` 直呼会命中 `npm.ps1`
执行策略问题。解法：命令分开执行；npm 一律 `npm.cmd run build`。

### 6.2 终端 GBK 乱码 ≠ 文件坏：只信文件，不信管道

cargo / git 输出的中文在 PowerShell 管道里常显示为乱码（GBK/UTF-8 混流），
但**文件内容本身是对的**（Read/Grep 验证）。纪律：不修终端，判断依据
一律取自文件；需要存证时 `| Out-File -Encoding utf8` 再读文件。

### 6.3 `git commit -m` 的中文消息被 PowerShell 引号拆坏【2026-09-25】

**现象**：`git commit -m "phase-8-9: 文档收尾（docs/15 会议设计 + 三处同步）..."`
里的全角括号被 shell 解析拆坏，`error: pathspec '文档收尾（docs/15' did not match`
——中文消息变成了 pathspec 参数。

**修复**：提交消息写进临时文件，`git commit -F <file>`。从此本项目提交一律 -F。

### 6.4 本机无 Docker：deploy 配置写好、诚实记录未实机验证【阶段 9】

`deploy/docker-compose.yml` + `livekit.yaml` 按官方文档写好，但本机没有
Docker，未实机联调。docs/15 §七原话：**"写明没验证过什么，和验证过什么一样重要"**。
将来有 Docker 的机器上一条 `docker compose -f deploy/docker-compose.yml up -d`
即可补上冒烟。

### 6.5 Windows 动态端口池全局共享：源 IP 轮换换不来新端口【阶段 10 压测】

**现象**：10 万连接压测三跑停在 ~55,490（os error 10055），而源 IP
轮换已实证生效（`Get-NetTCPConnection` 看到 127.0.0.1/2/3 各 1000 条）。

**根因**：Linux 的临时端口按源地址分区（bind 不同源 IP 能成倍扩容）；
Windows 的动态端口池**全局共享**——三跑数字 55,497/55,487/55,490 全部
≈ 池容量 55,536，同一次调宽后的全局上限。同一错误码（10055）两张面孔：
第一跑是「单 IP 端口耗尽」（本机池被配置成 1024/13977），第三跑是
「全局池耗尽」——**错误码只告诉你哪类资源不够，不告诉你哪个资源不够**。

**解法**：显式 `bind(源地址, 源端口)` 不受动态池约束（池只管自动分配）。
每个源 IP 独享 20000..=65535 共 45,536 个端口，7 源 IP 容量 31.8 万。

**纪律**：
- `netsh int ipv4 set dynamicport` 改的是**系统全局状态**，压完必须还原
（本机已还原 1024/13977）；
- **两个平台的内核行为差异只能实验分辨，不能靠文档记忆**——判别
  实验设计与数字见 docs/16 §4.3。

### 6.6 PowerShell 5.1 按 ANSI 读 UTF-8 无 BOM 的 .ps1：中文串直接破坏语法【阶段 14】

**现象**：错字扫描脚本里写了中文 pattern（`'分尸'`），执行报
ParserError「表达式或语句中包含意外的标记」——报错回显里的字符串
已是 GBK 乱码（`鍒嗗案`），且乱码中混进了引号字符，把字符串
字面量拦腰拆断。

**根因**：Windows PowerShell 5.1 对**无 BOM 的 UTF-8 脚本文件**按
ANSI/GBK 解码（PS 7+ 才默认 UTF-8）——中文的 UTF-8 字节被按 GBK
两两错配，某些字节对恰好解出 `'` 等破坏语法的字符。脚本还没运行，
在**词法分析层**就死了。

**修复**：ps1 脚本一律纯 ASCII；需要中文 pattern 时用码位在运行时
构造：`[char]0x5206`（分）——**数据是数据、脚本是脚本，别让脚本
文件的编码决定数据的编码**。（或用 UTF-8 with BOM 保存脚本，
但工具链写文件时 BOM 不可控，码位构造更硬。）

**教训**：与 §6.2（GBK 乱码）同族但层级不同：§6.2 是**输出**乱码
（文件没坏，显示坏了），本条是**输入**乱码（脚本语法直接被破坏）。
跨编码边界的两条铁律：输出重定向到文件再读（§6.2），输入用码位
构造（本条）。

### 6.7 GitHub service container 只支持 Linux runner：放进三平台矩阵，windows/macos 腿直接失败【阶段 14 CI】

**现象**：ci.yml 里 `test` job 用 `matrix.os: [ubuntu, windows, macos]`
三平台跑测试，为兑现「CI 配真库补盲区」在 job 级别挂了
`services: postgres`。本地 YAML 校验、`cargo test` 全绿，push 后
Actions 上 windows/macos 两条腿**在跑任何 step 之前就失败**：
windows 报 `Container operations are only supported on Linux runners`，
macos 报 `docker: command not found`。

**根因**：GitHub 的 service container 依赖 Docker，而**托管 runner 里
只有 Linux 提供 Docker**——windows/macos runner 根本没有容器运行时。
`services:` 是 **job 级别**的键，被 matrix 每条腿无条件继承；它不是
「连不上就优雅跳过」，而是 job 启动阶段拉容器就报错，**整个 job 直接挂**
（比测试失败更早，连 checkout 都到不了）。

**修复**：把「验平台工具链」和「跑真库测试」拆成两个 job——
`test`（三平台矩阵，**不挂 service**，DB 测试靠 `pool_or_skip` 空转跳过）
+ `test-postgres`（**ubuntu-only**，挂 postgres service + DATABASE_URL
真跑 DB 测试）。`services` 无法按 matrix 值条件化，只能靠拆 job 隔离。

**教训**：这与 §6.4（本机无 Docker）是同一个物理约束的两张面孔——
**容器 = Linux 专属运行时**。凡「配了容器/service」的 CI 步骤，默认只在
Linux runner 成立；跨平台矩阵里挂 service，等于给非 Linux 腿判死刑。
CI 配置的「本地全绿」只证明 YAML 合法 + 命令能跑，**证明不了 runner
平台能力**——这正是 §6.4「诚实记录未实机验证」纪律要防的盲区，push 后
WebFetch Actions 页面才抓出来。

**同批 push 抓出的第二个缺陷**（同一条元教训）：pages.yml 引用了
不存在的 action `peaceiris/action-mdbook`（少一个 s），build job 2s 就报
`Unable to resolve action ... repository not found`——正确名是
`peaceiris/actions-mdbook`。本地 `mdbook build` 全绿只证明 mdBook 本身
能跑，**证明不了 workflow 里引用的第三方 action 名字对**。

**第三个缺陷**（改对 action 名后才暴露）：build 进到 `actions/configure-pages`
报 `Get Pages site failed ... Not Found`——仓库的 GitHub Pages 还没开（Source
未选 GitHub Actions）。先试了 `configure-pages` 的 `enablement: true` 想凭
`pages: write` 自动开启，实机又报 `Create Pages site failed: Resource not
accessible by integration`——**首次创建 Pages 站点需 admin 权限，而
GITHUB_TOKEN 最高只到 `pages: write`**，workflow 根本无法自建。这不是
配置 bug，而是一条硬人工边界（同 §6.4 本机无 Docker）：**必须由仓库
管理员手动进 Settings → Pages → Source 选 GitHub Actions 开一次**，开启后
workflow 才能部署。已回退 enablement、将此前置如实写进 pages.yml 注释。
**三个缺陷是递进的**：修好一个才能跑到下一个（容器报错遮住了一切→
action 名错遮住了 build→build 过了才轮到 Pages 未开）；但前两个是
我能改的配置 bug，第三个是只能人工做的仓库设置。

三个 bug 归一条元教训：CI 配置的每个外部引用（runner 能力、action 名、
image tag、Pages 开启状态）都属实机验证边界，push 后必须回看 Actions
页面，别拿「本地全绿」当「线上能跑」；而其中有些（如首开 Pages）是
人工管理员才能做的一次性设置，workflow 代不了劳——这种只能诚实记为待办。

---

## 七、Rust 业务开发常见错误速查表

> 这一节是"没在本项目踩过、但每个 Rust 业务开发者都会遇到"的高频错误。
> 按错误码/场景组织，一条一句话根因 + 正确姿势。详细的原理讲解见 docs/01、02。

### 7.1 编译器错误码 Top（便宜级的坑，背下来省时间）

| 错误码 | 报错原文关键词 | 一句话根因 | 正确姿势 |
|--------|--------------|-----------|---------|
| E0382 | use of moved value | 所有权已被转移还继续用 | `clone()` / 重排语句 / 借用代替拥有 |
| E0507 | cannot move out of … behind a shared reference | 只有 `&` 却想拿走 | 返回引用 / `clone` / 改 API 传值 |
| E0502/E0499 | cannot borrow as mutable (more than once) | 可变借用与其他借用共存 | 拆作用域；NLL 下先想"借用何时结束" |
| E0597 | `x` does not live long enough | 引用活得比被引用者久 | 缩短借用范围 / 拥有所有权 / 生命周期参数 |
| E0277 | `*x` cannot be sent between threads safely | 类型或其成员非 `Send`（`Rc`/`RefCell`/裸指针） | 换 `Arc`/`Mutex`；FFI 裸指针手写 unsafe impl 前先证明 |
| E0599 | no method named … found for struct `String` | 方法在 `Result/Option` 上，调用在解包后的值上（见 §2.5） | 链式调用前明确每步类型 |

### 7.2 运行时与逻辑陷阱（贵级的坑，重点记）

1. **整数 cast 截断/回绕**：`as` 是静默截断（`u64 as u32` 越界回绕）。
   业务代码纪律：跨宽度转换一律 `try_from().context(...)`，仅在论证后 `#[allow]`。
2. **字符串按字节索引 panic**：`&s[10..15]` 切在 UTF-8 字符中间直接 panic。
   用 `char_indices` / `s.get(a..b)`；记住 `String` 是字节串不是字符数组。
3. **`std::sync::Mutex` 跨 `.await` 持锁**：guard 不是 `Send`，且持锁跨挂起
   会把互斥变长阻塞。要么锁不跨 await（临界区内不做异步），要么用
   `tokio::sync::Mutex`（并自问真的需要在锁内异步吗）。
4. **同步阻塞调用进 async 上下文**：`std::thread::sleep`、同步 IO、重 CPU
   循环会饿死 worker 线程。用 `tokio::time::sleep` / `spawn_blocking`。
5. **`unwrap` 的纪律**：业务路径禁止裸 `unwrap`；`expect("到达这里的唯一条件")`
   把不可达假设写进消息。`unwrap_or_default` 吞错误经常是把"失败"伪装成
   "空值"——比 panic 更难查（又一个静默坑）。
6. **serde 的精度与变体**：u64 超 2^53 串化（§4.2）；枚举加变体时旧数据
   反序列化会挂——`#[serde(other)]` 或版本字段要有预案。
7. **`Rc`/`RefCell` 混进 async**：编译器用 E0277 拦 `spawn`，但 `LocalSet`
   路径不拦——错误延迟到运行时。多线程默认值：`Arc` + `Mutex`/原子量。
8. **`clone` 不是免费动词**：`Arc::clone` 是引用计数（便宜），
   `String/Vec::clone` 是深拷贝（贵）。热路径 clone 前想清楚是哪种。
9. **trait 方法同名歧义**：`new_from_slice` 在 `Mac` 与 `KeyInit`
   两个 trait 上同名，`Hmac<Sha256>::new_from_slice(...)` 报
   「多个适用项」——用全限定调用 `<Hmac<Sha256> as Mac>::new_from_slice`
   把意图写死。多个 trait 定义同名方法时，裸调用等于把解析权交给
   编译器去猜。
10. **`Zeroize` 派生不认非 Zeroize 字段**：`RatchetState` 含
    `HashMap`（不满足 `Zeroize`），`#[derive(ZeroizeOnDrop)]` 直接
    拒——手动 `impl Drop` 逐字段 `zeroize()`。宁可显式三行，不为
    派生换自定义容器；敏感结构的擦除路径要过目**每一个**字段。
11. **固有方法遮蔽 trait 方法**：quinn 的 `SendStream`/`RecvStream`
    各有自己的固有 `poll_write`/`poll_read`（返回 quinn 专属错误
    类型），直接 `.poll_read(cx, buf)` 解析到固有方法，tokio 的
    `AsyncRead` impl 不可见——类型对得上但返回类型对不上，报
    E0308。全限定 `<quinn::RecvStream as AsyncRead>::poll_read(...)`
    写死意图。与 #9 同源：**方法解析的名字查找，固有方法优先于
    trait 方法**——同名时意图必须显式。
12. **返回引用的省略生命周期绑错对象**：`fn split_parent(&self,
    path: &str) -> Result<(Ino, &str), _>` 的返回 `&str` 实际只是
    入参的切片，但省略规则把它绑到 `&self`——后续 `&mut self`
    操作全部 E0502（与不可变借用打架）。显式 `fn split_parent<'a>(
    &self, path: &'a str) -> Result<(Ino, &'a str), _>`。
    **生命周期标注不是美学问题，是借用检查器的合同**：省略规则只在
    简单情形下猜对，「一个入参切片穿越整个方法体」时要亲笔写。

### 7.3 错误处理的分层（本项目约定）

- **库/领域层**：`thiserror` 定义错误枚举（`TransportError`、`GroupError`）——
  错误是类型，可 match、可文档化；
- **应用层**：`anyhow` + `.context("在做什么时失败")` ——给底层错误补上
  业务语境再上抛；
- **FFI 边界（阶段 11 已落地）**：错误码模型，`Result` 不跨 C ABI——错误
  在边界翻译成机器可读码（docs/17 §4.1），未知码兜底串保向前兼容。

对照 Java：checked exception ≈ thiserror（类型化、强制处理），
RuntimeException ≈ anyhow（带上下文的动态错误），但 Rust 把"抛"变成
"返回"，调用链上每一环都要显式决定接不接（`?` 或 `match`）。

---

## 八、防坑纪律盘点（本项目的工程约定）

把上面所有坑反向提炼，就是这套纪律（多数已写进各文档，此处汇总）：

1. **验证三件套才算完成**：`cargo test --workspace` 全绿 + clippy 零警告 + fmt 干净；
2. **压测数字必须带守恒律对账**（§4.3）；
3. **ID 跨 JSON 边界一律串化**（§4.2）；
4. **键分配器与存储同生命周期**（§4.1）；
5. **关停信号 sender 随 task 保活**（§3.1）；
6. **命令式对象不进响应式系统**（§5.1）；
7. **并发状态单一写入者**（§5.3）;
8. **`allow/unwrap/expect` 必须带论证**（§2.3）；
9. **自动修复产物必须人工 review**（§2.2）；
10. **诚实记录未验证项**（§6.4）——没验证过什么，和验证过什么一样写清楚；
11. **提交消息走文件**（§6.3），临时文件不落仓库根目录（§2.4）；
12. **SDK 打包钉死 feature 集**（§2.6）；跨线程的裸指针上下文用
    「按值传参」的闭包形态（§5.5）。

## 九、阶段 12~14 增补位

阶段 12 的坑已入账：rustls provider 构建链与进程全局边界（§2.8）、
`b""` 非 ASCII（§2.9）、rcgen 0.14 签发 API 变形（§2.10）、双棘轮
状态归属三连坑（§4.6）、Hmac trait 歧义与 Zeroize 擦除（§7.2 #9/#10）。

阶段 13 的坑已入账：quinn 配置包装与两段式 connect 错误（§2.11）、
固有方法遮蔽 trait 方法（§7.2 #11）、LRU 容量 0 淘汰顺序（§4.7）、
返回引用的省略生命周期绑错对象（§7.2 #12）。

阶段 14 的坑已入账：幻影错误第二次实遇（§2.1 补笔——quic_demo
E0599，同款根因不同 crate）、PowerShell 5.1 按 ANSI 读 UTF-8 无 BOM
脚本（§6.6）、并行集成测试共用默认 `machine_id` 撞雪花 ID（§4.8——
CI 落地抓出的第一个真 bug）、GitHub Actions 实机验证抓出的三个配置
缺陷（§6.7——① service container 只支持 Linux runner，三平台矩阵挂
postgres service 让 windows/macos 腿启动即失败，已拆 job 修复；② pages.yml
的 action 名写成 `peaceiris/action-mdbook` 少个 s，报 repository not found，
已改 `actions-mdbook`；③ Pages 未开启，configure-pages 报 Get Pages site
failed，已加 `enablement: true` 自动开启）。CI/文档站的 YAML 本地校验
（js-yaml）、mdbook build 本地全绿只能证明配置合法，证明不了 runner
平台能力、第三方 action 名与 Pages 开启状态——三个缺陷都是 push 后
WebFetch Actions 页面逐个抓出来并修掉的（递进：修一个才能跑到下一个）：
CI（fmt/clippy/三平台矩阵/ubuntu-only postgres）已线上转绿（4m55s），
Docs 依次修完 action 名与 Pages 开启后待复跑确认（与 §6.4 同一纪律：
未实机验证项 push 后必须回看）。

后续阶段踩到的新坑按同格式追加（阶段 14 工程化的坑进对应节）。坑是
项目最有生命力的文档——**宁可文档变厚，不可经验失传**。
