//! 阶段 3/4 端到端集成测试：双客户端的完整故事线。
//!
//! 与 `src/client.rs` 内单元测试的区别：那些各自验证单一场景
//! （互发 / 离线补投 / 闪断重连 / 被拒停机 / 断线排队 / Ack 丢失重传），
//! 这里把整条业务故事线串成多幕——验证状态在多个阶段间流转时依然自洽：
//!
//! 1. **三幕主线**：在线互发 → 一方掉线期间离线暂存 →
//!    重连补投并恢复双向会话；
//! 2. **崩溃重启重传**（阶段 4）：发送方进程消失 → 同一本地库重启 →
//!    持久化重发表自动补发 → 接收方去重后体验上恰好收到一次。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use im_client::{ClientConfig, ClientEvent, ClientHandle, run_client};
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
///
/// `data_dir` 由调用方指定且同一用户重连必须复用同一目录：
/// `client_msg_id` 由本地库的单调计数器分配，换目录等于计数器
/// 归零——发出的新消息可能撞上接收方的去重窗口被静默丢弃
/// （生产语义里同一用户的库是持久的，这里对齐真实约束）。
async fn login(addr: SocketAddr, user_id: u64, data_dir: PathBuf) -> OnlineClient {
    let config = ClientConfig {
        data_dir: Some(data_dir),
        ..ClientConfig::new(addr.to_string(), user_id, "any")
    };
    let (events_tx, events_rx) = mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = im_transport::shutdown_channel();
    let handle = run_client(config, events_tx, shutdown_rx).await;
    OnlineClient { handle, events: events_rx, shutdown: shutdown_tx }
}

/// 等下一个业务事件：跳过两类过程噪音——连接后例行空 `SyncBatch`
/// （连接层噪音）与 `MessageQueued`（发送过程事件）。
async fn next_event(events: &mut mpsc::Receiver<ClientEvent>) -> ClientEvent {
    loop {
        let event =
            timeout(WAIT, events.recv()).await.expect("2s 内应收到事件").expect("客户端存活");
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
    handle.send_msg(to, Bytes::copy_from_slice(content)).await.expect("客户端存活");
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

    // 每个用户一个固定本地库目录（进程内唯一）：重连复用、
    // client_msg_id 单调递增（见 `login` 文档）
    let base = std::env::temp_dir().join(format!(
        "im-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时钟正常")
            .as_nanos(),
    ));
    let alice_dir = base.join("1");
    let bob_dir = base.join("2");

    // ── 第 1 幕：在线互发 ──
    let mut alice = login(addr, 1, alice_dir).await;
    let mut bob = login(addr, 2, bob_dir.clone()).await;
    expect_connected(&mut alice.events).await;
    expect_connected(&mut bob.events).await;

    // Alice → Bob：Bob 收到消息、Alice 收到 Ack，msg_id 一致
    alice.handle.send_msg(2, Bytes::from_static("第 1 幕：你好 Bob".as_bytes())).await.unwrap();
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

    // Bob → Alice：双向都要通
    bob.handle.send_msg(1, Bytes::from_static("第 1 幕：收到".as_bytes())).await.unwrap();
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

    let id1 = send_and_ack(&alice.handle, &mut alice.events, 2, b"offline 1").await;
    let id2 = send_and_ack(&alice.handle, &mut alice.events, 2, b"offline 2").await;
    assert!(id1 < id2, "同一机器的雪花 msg_id 应单调递增（{id1} < {id2}）");

    // ── 第 3 幕：Bob 回来，自动补投 + 恢复双向 ──
    let mut bob = login(addr, 2, bob_dir).await;
    expect_connected(&mut bob.events).await;

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
    bob.handle.send_msg(1, Bytes::from_static("第 3 幕：都收到了".as_bytes())).await.unwrap();
    match next_event(&mut alice.events).await {
        ClientEvent::Message(msg) => {
            assert_eq!(msg.from, 2);
            assert_eq!(msg.content, Bytes::from_static("第 3 幕：都收到了".as_bytes()));
        }
        other => panic!("Alice 应收到 Bob 的回归回复，实际 {other:?}"),
    }
}

/// 阶段 4 的集大成场景：发送方「进程崩溃」后，持久化重发表驱动补发，
/// 接收方去重后体验上恰好收到一次。
///
/// 故事线：Bob 发消息 → Alice 收到（消息确定到达服务端）→ Bob 不等
/// Ack 直接关停（Ack 生死由天，两种结局都合法）→ 同一本地库重启 →
/// 若未核销则重连补发 + Ack 迟到核销；若已核销则重发表本就是空的。
/// 收尾用「直接开 Bob 的本地库」验收终态：pending 必空（Ack 一定核销
/// 或补发后核销）、历史里消息一定转正。
#[tokio::test]
async fn crashed_client_resends_persisted_outbox_on_restart() {
    let (addr, _sessions, _shutdown) = im_server::spawn_server(SessionConfig {
        authenticator: Arc::new(AllowAll),
        ..SessionConfig::default()
    })
    .await
    .expect("服务应能启动");

    let base = std::env::temp_dir().join(format!(
        "im-e2e-crash-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时钟正常")
            .as_nanos(),
    ));
    let alice_dir = base.join("1");
    let bob_dir = base.join("2");

    let mut alice = login(addr, 1, alice_dir).await;
    expect_connected(&mut alice.events).await;

    // 发送 → 对方收到（消息确定进入服务端）→ 立刻杀进程（不等 Ack）
    let mut bob = login(addr, 2, bob_dir.clone()).await;
    expect_connected(&mut bob.events).await;
    bob.handle.send_msg(1, Bytes::from_static(b"crash before ack")).await.unwrap();
    match next_event(&mut alice.events).await {
        ClientEvent::Message(msg) => {
            assert_eq!(msg.content, Bytes::from_static(b"crash before ack"));
        }
        other => panic!("Alice 应收到消息，实际 {other:?}"),
    }
    bob.shutdown.trigger();
    drop(bob);
    sleep(SETTLE).await; // 等服务端注销路由（否则重连握手被「already online」拒）

    // 同一本地库重启：持久化的重发表被 Outbox::load 恢复，
    // 连接建立即 resends() 补发（若早已核销则重发表为空，补发也是空的）
    let mut bob = login(addr, 2, bob_dir.clone()).await;
    expect_connected(&mut bob.events).await;

    // Bob 终将收到 Ack：要么断线前已核销（重启后无事件），
    // 要么补发后新 Ack 到达。等 Ack 或一段安静（已核销分支）。
    loop {
        match timeout(Duration::from_millis(500), bob.events.recv()).await {
            // 安静半秒 = 没有补发，说明崩溃前 Ack 已核销
            Err(_elapsed) => break,
            Ok(None) => panic!("客户端不应退出"),
            Ok(Some(event)) => match event {
                ClientEvent::Ack { client_msg_id, .. } => {
                    assert_eq!(client_msg_id, 1, "Bob 的首条消息");
                    break;
                }
                ClientEvent::Disconnected
                | ClientEvent::Connected { .. }
                | ClientEvent::MessageQueued { .. }
                | ClientEvent::SyncBatch(_) => continue, // 过程事件
                other => panic!("Bob 不应收到 {other:?}"),
            },
        }
    }

    // Alice 侧「恰好一次」：即便 Bob 补发过，去重窗口也应拦下重复投递——
    // 两个分支都适用（补发发生时重复早已到达并被拦下，此刻事件流里
    // 若还有 Message 就是去重失败的铁证）
    match timeout(SETTLE, alice.events.recv()).await {
        Err(_elapsed) => {} // 安静：没有重复投递到 UI
        Ok(Some(ClientEvent::Message(msg))) => {
            panic!("重复投递未被去重：{msg:?}")
        }
        Ok(Some(_)) | Ok(None) => {} // 其他过程事件/通道存活，不足为证
    }

    // 终态验收（绕过客户端直接开库，防自说自话）：
    // pending 空（Ack 核销或补发后核销）+ 消息转正入历史
    bob.shutdown.trigger();
    drop(bob);
    sleep(SETTLE).await; // 等句柄释放（Windows 下立刻重开同目录可能撞锁）
    let mut store = im_storage::LocalStore::open(&bob_dir).unwrap();
    assert!(store.pending_all().unwrap().is_empty(), "重发表必须被 Ack 清空");
    let history = store.history(1, 10).unwrap();
    assert_eq!(history.len(), 1, "崩溃前的消息已转正");
    assert_eq!(history[0].content, Bytes::from_static(b"crash before ack"));
}
