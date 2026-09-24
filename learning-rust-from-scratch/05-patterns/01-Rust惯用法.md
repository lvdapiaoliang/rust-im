# 01 - Rust 惯用模式：NEWTYPE、Typestate、RAII、Builder

> 这四种是「Rust 原生」的设计模式——GoF 书里没有，但每个 Rust 工程
> 都在用。它们也是面试「Rust API 设计品味」的最好证明。

## 一、NEWTYPE：零成本类型安全

```rust
/// 问题起点：裸类型到处传，语义靠变量名和注释
fn send_msg(to: u64, from: u64) { /* to 和 from 传反了编译器不管！ */ }

/// NEWTYPE：一行包装，错误关系变成类型错误
pub struct UserId(pub u64);
pub struct SessionId(pub u64);

fn send_msg2(to: UserId, from: UserId) { ... }
// send_msg2(SessionId(1), UserId(2))  // ❌ 编译错误！类别搞混不可能发生
```

进阶用法：

```rust
/// ① 加约束：String 没有 Eq/Hash（防注入语义的 TokenType）
pub struct AuthToken(String);
impl PartialEq for AuthToken { /* 常数时间比较，防时序攻击 */ }
impl std::fmt::Display for AuthToken {         // 日志脱敏！
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "AuthToken(****)")
    }
}

/// ② 区分单位（NASA 火星气候探测者号：单位搞混损失 3.27 亿美元）
pub struct Meters(pub f64);
pub struct Feet(pub f64);

/// ③ impl 重叠的解决方案：为 чужой类型本地实现 trait（孤儿规则的合作者）
struct SortedVec(Vec<i32>);
impl MyExtension for SortedVec { ... }          // Vec 直接 impl 会冲突，包一层就行
```

> 【Java】只能靠继承或 wrapper class 模拟，有运行时开销；
> Rust NEWTYPE 编译后与裸类型零差别（单层结构体直接解引用）。
> 【实战】rust-im 的 `UserId(u64)`、`Seq(u64)`、`FrameType(u8)` 全是 NEWTYPE——
> 「写 Rust 的第一品味」就是从随手 NewType 开始。

## 二、Typestate：把状态机编进类型系统

**目标：非法状态转换在编译期失败**，而不是运行时 if-else 地狱。

```rust
/// 反面教材：运行时状态检查（Java 的常态）
struct Conn {
    connected: bool,
    authenticated: bool,
}
impl Conn {
    fn send(&self, msg: &str) -> Result<(), Error> {
        if !self.connected { return Err(...); }        // 散落各处的防御
        if !self.authenticated { return Err(...); }
        ...
    }
}

/// Typestate：状态作为类型参数，方法只存在于正确的状态上
pub struct Disconnected;
pub struct Connected { stream: TcpStream }
pub struct Authenticated { stream: TcpStream, user_id: u64 }

pub struct Connection<State> {           // 状态是零大小的幽灵类型（marker）
    _state: std::marker::PhantomData<State>,    // PhantomData：只在类型层面存在
}

impl Connection<Disconnected> {
    pub fn connect(addr: &str) -> Result<Connection<Connected>, Error> {
        // 返回值类型就是状态转换的证明！
        Ok(Connection { _state: PhantomData })
    }
}

impl Connection<Authenticated> {
    pub fn send(&mut self, msg: &str) -> Result<(), Error> { ... }
    // send 只存在于 Authenticated 状态 → 未登录调用 send 是编译错误！
}

// Connection<Disconnected> 上没有 send 方法：
// let conn = Connection::<Disconnected>::new();
// conn.send("hi");   // ❌ no method named `send` —— 状态错误在编译期报
```

适用边界：状态数少且转换路径清晰（协议握手、FFI 句柄生命周期）。
状态多而复杂时退回 enum + match（见行为型篇）。rust-im 阶段 6 的 SDK
句柄正是 Typestate 的完美舞台：C 端拿到的 `*mut Conn<Authenticated>`，
天生不可在未认证时调用发送接口。

## 三、RAII：Rust 的第一设计模式

**Resource Acquisition Is Initialization**——资源生命周期 = 值的生命周期。

```rust
pub struct Guard { acquired: bool }
impl Guard {
    pub fn acquire() -> Self {  // 获取资源 = 构造值
        Self { acquired: true }
    }
}
impl Drop for Guard {           // 释放资源 = Drop（任何路径都会执行！）
    fn drop(&mut self) {
        // 关连接、放锁、发遥测、刷缓冲……
    }
}

fn f() {
    let g = Guard::acquire();
    if early_case { return; }    // 提前返回？drop 执行 ✅
    if let Err(e) = risky() { panic!("{e}"); }   // panic？unwind 时 drop 执行 ✅
}                                // 正常结束？drop 执行 ✅
```

> 【Java】try-with-resources 需要**每个**使用点记得写 try；
> RAII 是**定义点**一次保证，所有使用点（包括你三个月后新写的 return）自动安全。
> 这就是 rust-im 里 TcpStream/File/Channel 零手动 close 的原因。
> Go 的 defer、C++ 的 RAII 同族——Java 工程师转 Rust 后回不去的第一名。

**RAII 组合出的一切**：MutexGuard（锁）、SinkScope（tracing）、
`scopeguard` crate（任意回调的 scope guard）、测试里的临时目录。

## 四、Builder：Rust 唯一常用的「创建型」

复杂构造（多字段 + 可选 + 校验 + 泛型上下文）的标准答案：

```rust
pub struct ServerConfig {
    pub bind: String,
    pub workers: usize,
    pub max_conn: usize,
    pub heartbeat_ms: u64,
}

pub struct ServerConfigBuilder {
    bind: Option<String>,          // 必填字段用 Option 追踪
    workers: usize,                // 有默认值的直接存值
    max_conn: usize,
    heartbeat_ms: u64,
}

impl Default for ServerConfigBuilder {
    fn default() -> Self {
        Self { bind: None, workers: 8, max_conn: 65_536, heartbeat_ms: 30_000 }
    }
}

impl ServerConfigBuilder {
    pub fn bind(mut self, addr: impl Into<String>) -> Self {   // 链式：消费 self 返回 self
        self.bind = Some(addr.into());
        self
    }
    pub fn workers(mut self, n: usize) -> Self { self.workers = n; self }

    /// 终结方法：build。必填缺失 → Result 错误（不是 panic！）
    pub fn build(self) -> Result<ServerConfig, ConfigError> {
        let bind = self.bind.ok_or(ConfigError::MissingField("bind"))?;
        Ok(ServerConfig {
            bind,
            workers: self.workers,
            max_conn: self.max_conn,
            heartbeat_ms: self.heartbeat_ms,
        })
    }
}

// 使用（对标 Java 的 lombok @Builder，但编译期防漏字段）：
let cfg = ServerConfigBuilder::default()
    .bind("0.0.0.0:8080")
    .workers(16)
    .build()?;
```

进阶变体：

```rust
// derive 生态：derive_builder（省手写）
// typestate builder：字段必填也做成类型状态（Builder<NoBind> → Builder<HasBind>）
//   —— reqwest/axum 的 API 就是这个流派，错误提前到「调用处少写一行就编译不过」
```

## 五、错误即类型（错误模型的模式化）

```rust
/// 库层：一个错误 enum 覆盖所有失败模式 + #[non_exhaustive] 留扩展
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]                       // 未来加变体不是 breaking change！
pub enum TransportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("handshake failed at stage {stage}")]
    Handshake { stage: &'static str },
    #[error("heartbeat timeout after {0}ms")]
    Timeout(u64),
}
// 调用方可以 match 做决策：Timeout → 重连；Handshake → 换凭证重试
// —— 错误类型就是 API 的一部分（这是 Rust 和 Java 异常哲学的最大分野）
```

## 练习

1. 给 rust-im 设计 `SessionId`/`GroupId`/`MsgId` 三个 NEWTYPE，
   实现一个「组消息发给错误对象类型」必然编译失败的函数签名。
2. 把上面的 Connection Typestate 补完整（三个状态、两次转换），写测试证明
   「未连接时 send 无法编译」（编译失败也是测试——`compile_fail` 测试或注释说明）。
3. 手写 ServerConfigBuilder 后换 derive_builder 重写，对比代码量与灵活性。

## 自测

1. NEWTYPE 的三个用途？为什么说是零成本？
2. Typestate 与 enum 状态机的选择边界？
3. RAII 相比 try-with-resources 的本质优势？
4. Builder 的 build 为什么返回 Result？

下一篇：[02-创建型与结构型.md](02-创建型与结构型.md)
