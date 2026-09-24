//! 阶段 3 端到端集成测试：双客户端的完整故事线。
//!
//! 与 `src/client.rs` 内单元测试的区别：那些各自验证单一场景
//! （互发 / 离线补投 / 闪断重连 / 被拒停机 / 断线排队），
//! 这里把整条业务故事线串成三幕——在线互发 → 一方掉线期间离线暂存 →
//! 重连补投并恢复双向会话——验证状态在多个阶段间流转时依然自洽：
//!
//! 1. **第 1 幕**：Alice 与 Bob 在线，互发一条（双向路由 + 消息级 Ack）；
//! 2. **第 2 幕**：Bob 掉线，Alice 连发两条——进离线队列，Alice 仍收到 Ack
//!    （发送方的体验与接收方在线与否无关）；
//! 3. **第 3 幕**：Bob 重新上线，自动同步按 `msg_id` 升序补投两条，
//!    随后回复 Alice——重连后的双向会话照常工作。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use im_client::{run_client, ClientConfig, ClientEvent, ClientHandle};
use im_server::{AllowAll, SessionConfig};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

/// 测试统一的等待上限：本地回环上任何正常交互都应远快于此。
const WAIT: Duration = Duration::from_secs(2);
/// Bob 掉线后等服务端注销路由的缓冲（TCP 关闭 → 网关退出 → 路由摘除）。
const SETTLE: Duration = Duration::from_millis(300);

/// 一个在线客户端：发送句柄 + 事件流 + 关停信号。
struct OnlineClient {
    handle: ClientHandle,
    events: mpsc::Receiver<ClientEvent>,
    shutdown: im_transport::ShutdownTx,
}

/// 上线一个客户端（AllowAll 认证下 token 随意给）。
async fn login(addr: SocketAddr, user_id: u64) -> OnlineClient {
    let config = ClientConfig::new(addr.to_string(), user_id, "any");
    let (events_tx, events_rx) = mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = im_transport::shutdown_channel();
    let handle = run_client(config, events_tx, shutdown_rx).await;
    OnlineClient {
        handle,
        events: events_rx,
        shutdown: shutdown_tx,
    }
}

/// 等下一个业务事件：跳过两类过程噪音——连接后例行空 `SyncBatch`
/// （连接层噪音）与 `MessageQueued`（发送过程事件）。
async fn next_event(events: &mut mpsc::Receiver<ClientEvent>) -> ClientEvent {
    loop {
        let event = timeout(WAIT, events.recv())
            .await
            .expect("2s 内应收到事件")
            .expect("客户端存活");
        match event {
            ClientEvent::SyncBatch(ref batch) if batch.is_empty() => {}
            ClientEvent::MessageQueued { .. } => {}
            other => return other,
        }
    }
}

/// 等到 Connected（其他事件直接失败——上线阶段不该有别的动静）。
async fn expect_connected(events: &mut mpsc::Receiver<ClientEvent>) {
    match next_event(events).await {
        ClientEvent::Connected { session_id } => assert_ne!(session_id, 0),
        other => panic!("应先握手成功，实际 {other:?}"),
    }
}

/// 发一条消息并等到对应 Ack（返回服务端裁决的全局 `msg_id`）。
async fn send_and_ack(
    handle: &ClientHandle,
    events: &mut mpsc::Receiver<ClientEvent>,
    to: u64,
    content: &[u8],
) -> u64 {
    handle
        .send_msg(to, Bytes::copy_from_slice(content))
        .await
        .expect("客户端存活");
    match next_event(events).await {
        ClientEvent::Ack { msg_id, .. } => msg_id,
        other => panic!("应收到 Ack，实际 {other:?}"),
    }
}

/// 三幕完整故事线（见模块文档）。
#[tokio::test]
async fn online_chat_then_offline_catchup_and_resume() {
    // 完整服务端：随机端口 + AllowAll。
    // shutdown_tx 持有到测试结束——「sender 全部 drop = 视为已关停」。
    let (addr, _sessions, _shutdown) = im_server::spawn_server(SessionConfig {
        authenticator: Arc::new(AllowAll),
        ..SessionConfig::default()
    })
    .await
    .expect("服务应能启动");

    // ── 第 1 幕：在线互发 ──
    eprintln!("[probe] act1 start");
    let mut alice = login(addr, 1).await;
    let mut bob = login(addr, 2).await;
    expect_connected(&mut alice.events).await;
    expect_connected(&mut bob.events).await;
    eprintln!("[probe] act1 both connected");

    // Alice → Bob：Bob 收到消息、Alice 收到 Ack，msg_id 一致
    alice
        .handle
        .send_msg(2, Bytes::from_static("第 1 幕：你好 Bob".as_bytes()))
        .await
        .unwrap();
    let bob_msg = match next_event(&mut bob.events).await {
        ClientEvent::Message(msg) => msg,
        other => panic!("Bob 应收到消息，实际 {other:?}"),
    };
    assert_eq!(bob_msg.from, 1);
    let ack1 = match next_event(&mut alice.events).await {
        ClientEvent::Ack { msg_id, .. } => msg_id,
        other => panic!("Alice 应收到 Ack，实际 {other:?}"),
    };
    assert_eq!(ack1, bob_msg.msg_id);
    eprintln!("[probe] act1 alice acked");

    // Bob → Alice：双向都要通
    bob.handle
        .send_msg(1, Bytes::from_static("第 1 幕：收到".as_bytes()))
        .await
        .unwrap();
    match next_event(&mut alice.events).await {
        ClientEvent::Message(msg) => {
            assert_eq!(msg.from, 2);
        }
        other => panic!("Alice 应收到回复，实际 {other:?}"),
    }

    // ── 第 2 幕：Bob 掉线，Alice 连发两条 ──
    // 关停信号 + 命令通道关闭（drop handle）双保险：client_loop 正常退出，
    // TCP 随之关闭
    bob.shutdown.trigger();
    drop(bob);
    // 等服务端走完收尾链：TCP 关闭 → 网关退出 → 路由注销。
    // 不等的话下一条消息可能撞上「路由还在、连接将死」的窗口
    // （生产环境靠阶段 4 的 Ack 重发兜底，测试里直接规避）。
    sleep(SETTLE).await;
    eprintln!("[probe] act2 bob gone, sending offline");

    let id1 = send_and_ack(&alice.handle, &mut alice.events, 2, b"offline 1").await;
    let id2 = send_and_ack(&alice.handle, &mut alice.events, 2, b"offline 2").await;
    assert!(
        id1 < id2,
        "同一机器的雪花 msg_id 应单调递增（{id1} < {id2}）"
    );

    // ── 第 3 幕：Bob 回来，自动补投 + 恢复双向 ──
    eprintln!("[probe] act3 bob back");
    let mut bob = login(addr, 2).await;
    expect_connected(&mut bob.events).await;
    eprintln!("[probe] act3 bob connected, waiting sync");

    let batch = match next_event(&mut bob.events).await {
        ClientEvent::SyncBatch(messages) => messages,
        other => panic!("Bob 应收到离线补投，实际 {other:?}"),
    };
    assert_eq!(batch.len(), 2, "断线期间的两条都应补投");
    assert_eq!(batch[0].msg_id, id1);
    assert_eq!(batch[1].msg_id, id2, "补投按 msg_id 升序");
    assert_eq!(batch[0].content, Bytes::from_static(b"offline 1"));
    assert_eq!(batch[1].content, Bytes::from_static(b"offline 2"));

    // 恢复双向：Bob 回复，Alice 立刻收到
    bob.handle
        .send_msg(1, Bytes::from_static("第 3 幕：都收到了".as_bytes()))
        .await
        .unwrap();
    match next_event(&mut alice.events).await {
        ClientEvent::Message(msg) => {
            assert_eq!(msg.from, 2);
            assert_eq!(msg.content, Bytes::from_static("第 3 幕：都收到了".as_bytes()));
        }
        other => panic!("Alice 应收到 Bob 的回归回复，实际 {other:?}"),
    }
}
