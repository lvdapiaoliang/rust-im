//! 测试脚手架（`#[cfg(test)]` 专用）：供 `core`/`native` 等模块的测试共用。
//!
//! 独立成模块而不是各测试文件各写一份：服务端装配（runtime + 监听地址
//! + Sessions 句柄）是每个端到端测试的同一套前奏——重复三遍之后，
//! 「谁持有服务端生命周期」的纪律就会开始走样。

use std::sync::Arc;
use std::time::Duration;

use im_server::{SessionConfig, StaticToken};
use tokio::runtime::Runtime;

use crate::core::ImSdkEvent;

/// 测试宿主：服务端任务的专属 runtime + 监听地址，存活到测试结束。
///
/// SDK 的同步入口内部 `block_on`，**不能在 tokio 上下文里调**
/// （「Cannot start a runtime from within a runtime」）——这恰恰是
/// 真实 C 调用方的常态：没有环境 runtime。所以测试一律用普通 #[test]，
/// 服务端挂在独立 runtime 上（drop 即停），SDK 从测试线程同步调用——
/// 与未来 JNI/C 消费者的调用形态完全一致。
pub(crate) struct TestServer {
    /// 服务端 accept/连接任务的宿主：存活到 `TestServer` drop，否则服务端随之停摆
    pub(crate) _rt: Runtime,
    pub(crate) addr: String,
    pub(crate) _sessions: im_server::Sessions,
    pub(crate) _shutdown: im_transport::ShutdownTx,
}

/// 起一个默认口令（"demo"）的测试服务端。
pub(crate) fn spawn_test_server(config: SessionConfig) -> TestServer {
    let rt = Runtime::new().expect("测试服务端运行时应能创建");
    let (addr, sessions, shutdown) = rt
        .block_on(async { im_server::spawn_server(config).await })
        .expect("测试服务端应能启动");
    TestServer { _rt: rt, addr: addr.to_string(), _sessions: sessions, _shutdown: shutdown }
}

/// 起一个只认口令 "right" 的测试服务端（握手被拒路径的专用对端）。
pub(crate) fn spawn_picky_server() -> TestServer {
    spawn_test_server(SessionConfig {
        authenticator: Arc::new(StaticToken { token: "right".to_string() }),
        ..SessionConfig::default()
    })
}

/// 拿到事件后安全地读字段（测试侧的「回调」就是它）。
///
/// 测试代码读裸指针也过一遍 allow：生产代码零 unsafe 的分层承诺
/// 只针对 src 主干；测试里的指针检查是 ffi 层契约的验收点。
#[allow(unsafe_code)]
pub(crate) fn borrow(ev: &ImSdkEvent) -> (i32, Vec<u8>) {
    let data = if ev.data.is_null() {
        Vec::new()
    } else {
        // SAFETY: alloc_event 契约——(data, data_len) 是有效的配对区间
        unsafe { std::slice::from_raw_parts(ev.data, ev.data_len) }.to_vec()
    };
    (ev.type_, data)
}

/// 端到端测试的默认等待预算（秒级足够本地回环；再大就是掩盖挂死）。
pub(crate) const WAIT: Duration = Duration::from_secs(5);
