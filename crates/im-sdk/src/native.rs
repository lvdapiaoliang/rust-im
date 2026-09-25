//! 类型状态（Typestate）Rust 原生 API：rlib 消费面的 SDK 形态。
//!
//! docs/17 §七立过账：**C ABI 形态下类型状态无法兑现**——调用方拿到的是
//! `void*`，强转、乱传、双 free 都在 C 的能力面内，Rust 编译期的类型区分
//! 到不了 C 那边。本模块是它在 Rust 原生 API（rlib 形态）里的兑现：
//! Tauri 桌面端的后端直接 `im-sdk = { path = ... }` 依赖时，消费的就是
//! 这套 API——**模式是否适用由消费方的类型系统决定**（同一份实现，
//! ABI 形态退化为运行时检查，rlib 形态才能升级为编译期保证）。
//!
//! # 模式：把状态机的「阶段」编码进类型
//!
//! ```text
//! TypedSdkClient<Disconnected> ──wait_connected──▶ TypedSdkClient<Connected>
//!        │  没有 send 方法                                 │  有 send
//!        └─ 未连接就发消息 → E0599 编译错误，挡在编译期
//! ```
//!
//! `send` 只存在于 `impl TypedSdkClient<Connected>` 块里——「未连接的
//! 句柄不能发消息」不再靠文档或运行时检查，而是**方法根本不存在**。
//! 状态转换消耗旧值、产出新值（`wait_connected(self) -> ...`），调用方
//! 手里的旧句柄随之作废——状态机的每次跃迁都被所有权系统记账。
//!
//! # 诚实边界：类型编码「协议阶段」，不编码「链路活性」
//!
//! `Connected` 类型证明的是**握手已完成**（服务端已发 `Connected` 事件、
//! session_id 已知），不是「网线此刻是通的」——断线重连是 `im-client`
//! 状态机的内部职责（`Disconnected` 事件只作通知），且断线后 `send`
//! 本来就合法（入离线队列、重连后补投，这正是 im-client 的语义）。
//! 类型状态收走的是更基本的错误：**连握手都没完成**（连 session_id
//! 都没有）就发消息。想用类型编码链路活性，就会撞上「类型不能随
//! 网络事件回退」的硬墙——那是运行时状态，别让类型系统背它背不动的锅。
//!
//! # 错误归还（fold-back）：失败路径把客户端还给调用方
//!
//! `wait_connected` 的错误是 `(TypedSdkClient<Disconnected>, i32)` 而不是
//! 干脆的 `Err(i32)`——超时的客户端**还是好的**（重连还在进行），调用方
//! 拿回去可以再等一次；握手被拒的客户端拿回去 `destroy`，资源不悬空。
//! 代价是错误类型带元组，换来「失败不吞资源」——所有权语言里，**把
//! 东西还回去比替调用方扔掉更诚实**。

use std::marker::PhantomData;
use std::time::{Duration, Instant};

use crate::core::{self, ImSdkEvent, SdkClient, EVENT_CONNECTED, EVENT_REJECTED};
use crate::error;
use crate::ffi;

/// 状态标记：尚未完成握手（`TypedSdkClient<Disconnected>`）。
///
/// 零大小类型——类型状态的成本全部发生在编译期，运行时一个字节不多。
/// 这个形态上**没有 `send`**（见模块文档的转换图）。
pub struct Disconnected;

/// 状态标记：握手已完成（`TypedSdkClient<Connected>`）。
///
/// 进入该形态的唯一路径是 [`TypedSdkClient::<Disconnected>::wait_connected`]
/// 成功——`send` 从此可用，`session_id` 从此有意义。
pub struct Connected;

/// 类型状态客户端：[`SdkClient`] 的 Rust 原生消费外壳。
///
/// 内部仍是阶段 11 的同步外观（[`SdkClient`]，FFI 也消费它）——本模块
/// 是**装饰器 + 类型状态**的组合：装饰一层状态泛型，不改动被装饰者。
///
/// ```compile_fail
/// /// 未连接的形态上没有 send——这一段编译不过（E0599），
/// /// 这正是本类型存在的全部意义，见 docs/17 §七的立账。
/// use im_sdk::native::{Disconnected, TypedSdkClient};
///
/// let client = TypedSdkClient::<Disconnected>::new("127.0.0.1:6000", 1, "demo", None)
///     .unwrap();
/// client.send(2, b"early hello").unwrap(); // ← 编译错误，消息发出去了才怪
/// ```
///
/// 状态参数走 `PhantomData<fn() -> State>` 而不是裸 `PhantomData<State>`：
/// 函数指针形态不「持有」State，State 的 Auto trait 约束（Send/Sync）
/// 不会反向传播到客户端结构体——标记类型该是零负担的，让它保持零负担。
pub struct TypedSdkClient<State> {
    /// 被装饰的同步外观（FFI 与 Rust 原生面共享同一实现）
    inner: SdkClient,
    /// 服务端会话 ID：`Connected` 形态才有意义，`Disconnected` 时恒 0
    session_id: u64,
    /// 状态标记（零大小）；fn 指针形态的理由见结构体文档
    _state: PhantomData<fn() -> State>,
}

impl TypedSdkClient<Disconnected> {
    /// 创建客户端（起专属运行时 + 连接状态机），**不等待握手**。
    ///
    /// 与 [`crate::core::create`] 同参同义——直接复用而不是重写：
    /// 构造逻辑只有一份，类型状态只在外面加壳。
    ///
    /// # Errors
    /// 运行时创建失败时返回 [`error::ERR_INTERNAL`]（见被委托函数）。
    pub fn new(
        server_addr: &str,
        user_id: u64,
        token: &str,
        data_dir: Option<&str>,
    ) -> Result<Self, i32> {
        Ok(Self {
            inner: core::create(server_addr, user_id, token, data_dir)?,
            session_id: 0,
            _state: PhantomData,
        })
    }

    /// 等待握手完成：消耗 `Disconnected` 形态，产出 `Connected` 形态。
    ///
    /// 事件预算按截止时间精确递减（不是「每轮都等满 timeout」——
    /// 循环里每收一条非目标事件，剩余时间都要少一块，超时口径才准）。
    /// 握手期的偶发 `Disconnected`（闪断重连）被跳过继续等；`Rejected`
    /// 是终局（状态机不再重连），立即归还。
    ///
    /// # Errors
    /// - `(client, `[`error::ERR_TIMEOUT`]`)`：预算耗尽——客户端仍在
    ///   重连，拿回去可以再调一次本函数；
    /// - `(client, `[`error::ERR_HANDSHAKE_REJECTED`]`)`：握手被拒——
    ///   拿回去 `destroy`；
    /// - `(client, `[`error::ERR_STOPPED`]`)`：状态机已落幕（通道关闭）。
    pub fn wait_connected(
        mut self,
        timeout: Duration,
    ) -> Result<TypedSdkClient<Connected>, (TypedSdkClient<Disconnected>, i32)> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err((self, error::ERR_TIMEOUT));
            }
            match self.inner.poll_event(remaining) {
                Ok(Some(ev)) => {
                    // 每条事件都当场归还内存（宽结构的 data 裸指针不随
                    // drop 自动回收——「谁分配谁释放」在 Rust 面同样成立）
                    let verdict = classify(&ev);
                    ffi::reclaim_event(ev);
                    match verdict {
                        // 跃迁：消耗 self（部分移动 inner），产出 Connected 形态
                        Ok(session_id) => {
                            return Ok(TypedSdkClient {
                                inner: self.inner,
                                session_id,
                                _state: PhantomData,
                            });
                        }
                        // 终局拒绝：归还整个客户端
                        Err(Some(code)) => return Err((self, code)),
                        // 非目标事件（闪断等）：继续等，预算已在循环头扣减
                        Err(None) => continue,
                    }
                }
                // poll 超时（Ok(None)）：大概率 remaining 先到，循环头会收口；
                // 若 poll 提前返回也继续等剩余预算
                Ok(None) => continue,
                // 状态机落幕（或互斥违例）：归还，让调用方收尾
                Err(code) => return Err((self, code)),
            }
        }
    }
}

/// 握手期事件的分类：`Ok(session_id)` = 跃迁条件成立；
/// `Err(Some(code))` = 终局失败；`Err(None)` = 可跳过。
fn classify(ev: &ImSdkEvent) -> Result<u64, Option<i32>> {
    match ev.type_ {
        EVENT_CONNECTED => {
            debug_assert!(
                ev.data.is_null(),
                "Connected 事件按协议不带数据载荷（alloc_event 的编组表）"
            );
            Ok(ev.session_id)
        }
        EVENT_REJECTED => Err(Some(error::ERR_HANDSHAKE_REJECTED)),
        _ => Err(None),
    }
}

impl TypedSdkClient<Connected> {
    /// 发一条消息：`Ok` 只表示命令已入队（断线排队、送达以 `Ack` 事件为准）。
    ///
    /// **这个方法只存在于 `Connected` 形态**——类型状态的全部价值
    /// 就浓缩在这一个 impl 块上。
    ///
    /// # Errors
    /// 客户端已退出（握手被拒后/destroy 后）返回 [`error::ERR_STOPPED`]。
    pub fn send(&self, to: u64, content: &[u8]) -> Result<(), i32> {
        self.inner.send(to, content)
    }

    /// 服务端会话 ID（握手完成时从 `Connected` 事件取得）。
    #[must_use]
    pub fn session_id(&self) -> u64 {
        self.session_id
    }
}

impl<State> TypedSdkClient<State> {
    /// 轮询一条事件（两种形态都可用——握手前后都有事件要消化）。
    ///
    /// # Errors
    /// 见 [`SdkClient::poll_event`]（超时是 `Ok(None)`，通道关闭是
    /// [`error::ERR_STOPPED`]）。
    pub fn poll_event(&mut self, timeout: Duration) -> Result<Option<ImSdkEvent>, i32> {
        self.inner.poll_event(timeout)
    }

    /// 拆壳：拿回内层 [`SdkClient`]（比如要切换到回调模式装配事件泵时）。
    ///
    /// 拆壳即放弃类型状态保证——此后回到「运行时检查」的世界。
    /// 给出这个出口是因为回调模式的装配点（`take_events`/`attach_pump`）
    /// 定义在内层；出口收窄在显式调用上，不会顺手漏掉。
    #[must_use]
    pub fn into_inner(self) -> SdkClient {
        self.inner
    }

    /// 销毁客户端（触发关停 → join 事件泵 → drop 运行时，顺序见内层）。
    pub fn destroy(self) {
        self.inner.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::EVENT_MESSAGE;
    use crate::testutil::{borrow, spawn_picky_server, spawn_test_server, TestServer, WAIT};

    /// 主线：new → wait_connected → 双客户端互发，send 只在 Connected 上可用。
    #[test]
    fn typed_flow_connects_then_sends() {
        let srv: TestServer = spawn_test_server(im_server::SessionConfig::default());
        let addr = srv.addr.clone();

        let alice = TypedSdkClient::new(&addr, 1, "demo", None)
            .unwrap()
            .wait_connected(WAIT)
            .expect("默认口令的握手应当成功");
        assert!(alice.session_id() > 0, "Connected 事件应携带非零 session_id");

        let bob = TypedSdkClient::new(&addr, 2, "demo", None)
            .unwrap()
            .wait_connected(WAIT)
            .expect("Bob 同样应握手成功");

        // 类型状态的主线证明：这里写的是 alice.send——
        // 换成握手前的形态，这行根本编译不过（见结构体文档的 compile_fail）
        alice.send(2, "你好，类型状态的桌面端".as_bytes()).unwrap();

        let ev = bob.poll_event(WAIT).unwrap().unwrap();
        let (ty, data) = borrow(&ev);
        assert_eq!(ty, EVENT_MESSAGE);
        assert_eq!(data, "你好，类型状态的桌面端".as_bytes());
        crate::ffi::reclaim_event(ev);

        alice.destroy();
        bob.destroy();
    }

    /// 握手被拒：`wait_connected` 归还客户端 + `ERR_HANDSHAKE_REJECTED`，
    /// 归还的客户端可以干净销毁（失败路径不吞资源）。
    #[test]
    fn rejected_handshake_returns_client_for_destroy() {
        let srv = spawn_picky_server();
        let addr = srv.addr.clone();

        let client = TypedSdkClient::new(&addr, 1, "demo", None).unwrap();
        let (client, code) = client
            .wait_connected(WAIT)
            .expect_err("错误口令的握手必须失败");
        assert_eq!(code, error::ERR_HANDSHAKE_REJECTED);
        // 归还的客户端仍能干净销毁——错误路径的资源闭环
        client.destroy();
    }

    /// 超时归还：预算耗尽时客户端完好归位（重连还在进行），可再等或销毁。
    #[test]
    fn timeout_hands_back_live_client() {
        // 端口 9（discard 服务）在本机开发环境几乎必然无人监听：
        // 连接失败 → 状态机持续重连 → 预算内不会有 Connected 事件
        let client = TypedSdkClient::new("127.0.0.1:9", 1, "demo", None).unwrap();
        let (client, code) = client
            .wait_connected(Duration::from_millis(300))
            .expect_err("无人监听的地址必然等不到握手");
        assert_eq!(code, error::ERR_TIMEOUT);
        client.destroy();
    }
}
