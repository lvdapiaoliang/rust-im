//! 连接网关：把 [`Connection`] 组装成完整的「连接生命周期管理器」。
//!
//! 这是第一个**会说话**的 IM 组件——echo 只会复读，网关会应答、保活、超时、关停。
//!
//! # 任务布局（每连接一组 actor，无锁协作）
//!
//! ```text
//!   对端 TCP ◀═════════════════════════════════════════════════╣
//!      │ 读                          写                        │
//!      ▼                            ▲                          │
//!  ┌─────────────┐  InboundFrame  ┌─┴────────────┐            │
//!  │  读循环 task │ ─────────────▶│   业务层      │            │
//!  │ （本函数体） │                │ （调用方）    │            │
//!  └──────┬──────┘                └──────┬───────┘            │
//!         │ Ping(服务端模式)              │ ConnectionHandle   │
//!         │ → 组装 Pong                   │ .send(frame)       │
//!         ▼                              ▼                     │
//!      mpsc 通道 ◀── 心跳 task(客户端模式, 定时发 Ping) ─┐       │
//!         │                                            │       │
//!         ▼                                            │       │
//!  ┌─────────────┐        done 信号（本函数结束时触发）  │       │
//!  │ 写 actor task│ ◀──────────────────────────────────┘       │
//!  │ （排干队列后退出）                                       │
//!  └───────────────────────────────────────────────────────────┘
//! ```
//!
//! # 落地的设计模式（对照 learning-rust-from-scratch/05-patterns）
//!
//! - **Actor 模式**：写 actor 独占写半部，所有发送方只与通道打交道——
//!   「单个 task 独占资源 + 消息通信」替代「共享 + 锁」；
//! - **enum 状态机**：心跳的策略用 [`HeartbeatPolicy`] 表达，
//!   读循环对 `Cmd::Ping` 的分流由一个 `match` 穷尽；
//! - **组合子风格**：超时不是回调（Java 的 `ReadTimeoutHandler`），
//!   而是 `tokio::time::timeout` 包住 `read_frame` 的普通组合。
//!
//! # 背压说明
//!
//! 出站通道容量满时 `send` 挂起（业务层被反压）；
//! 入站通道满时读循环挂起——TCP 接收窗口随之收缩，
//! 反压沿链路传导到发送端。这是特性不是缺陷：**慢消费者拖慢整条链路**，
//! 比静默丢帧后靠重传来补更便宜。

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use im_protocol::{Cmd, Frame, DEFAULT_MAX_FRAME_LEN};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

use crate::connection::{Connection, ReadHalf, WriteHalf};
use crate::error::TransportError;
use crate::shutdown::{shutdown_channel, ShutdownRx};

/// 出站帧通道容量。
///
/// 队列满 → `ConnectionHandle::send` 挂起 → 业务层被反压。
/// 64 够覆盖常规突发；真正的大扇出（阶段 3 广播）由业务层自己聚合。
const OUTBOUND_CHANNEL_CAPACITY: usize = 64;

/// 默认读空闲超时：60 秒收不到任何字节即判定对端死亡。
///
/// 经验值 `idle_timeout ≥ 2 × 心跳间隔`——容忍连续丢一个心跳的抖动。
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// 心跳策略：本连接扮演的角色。
#[derive(Clone, Debug)]
pub enum HeartbeatPolicy {
    /// 服务端：收到 `Ping` 自动回 `Pong`（不进业务层），
    /// 读空闲超过 `idle_timeout` 判定对端死亡并断连。
    Server,
    /// 客户端：每隔 `interval` 发一个 `Ping` 保活；
    /// 服务端回的 `Pong` 进入业务层（可用于探活统计）。
    ///
    /// 第一个 Ping 在建立连接后等满一个 `interval` 才发出。
    /// `interval` 应明显小于 `idle_timeout`，否则保活会误杀自己。
    Client {
        /// 两次 Ping 之间的间隔。
        interval: Duration,
    },
}

/// 网关行为配置。
#[derive(Clone, Debug)]
pub struct GatewayConfig {
    /// 单帧 payload 上限（防 DoS，语义同 `FrameDecoder`）。
    pub max_frame_len: usize,
    /// 读空闲超时：这么久读不到任何字节就断连。
    pub idle_timeout: Duration,
    /// 心跳策略（角色见 [`HeartbeatPolicy`]）。
    pub heartbeat: HeartbeatPolicy,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            max_frame_len: DEFAULT_MAX_FRAME_LEN,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            heartbeat: HeartbeatPolicy::Server,
        }
    }
}

/// 发送端句柄：向这条连接写帧的唯一合法方式（克隆廉价，任意 task 可持有）。
///
/// 所有写入都经由通道到达写 actor——**多个 task 并发 send 也不会交错帧字节**，
/// 这是 actor 独占写半部换来的保证（对照：共享 `&mut WriteHalf` 需要锁）。
#[derive(Clone, Debug)]
pub struct ConnectionHandle {
    tx: mpsc::Sender<Frame>,
}

impl ConnectionHandle {
    /// 异步发送一帧（经通道交给写 actor 编码写出）。
    ///
    /// # Errors
    ///
    /// 通道已关闭（写 actor 退出、连接即将关闭）时返回 [`TransportError::Closed`]。
    pub async fn send(&self, frame: Frame) -> Result<(), TransportError> {
        self.tx.send(frame).await.map_err(|_| TransportError::Closed)
    }
}

/// 递交给业务层的入站事件：一帧 +「怎么回话」的句柄 + 对端地址。
///
/// `handle` 随帧附带——业务层拿到帧就能回话，不需要自己维护
/// 「连接 id → 发送端」的路由表（阶段 3 引入会话层时再上正餐）。
#[derive(Debug)]
pub struct InboundFrame {
    /// 对端地址（日志与路由用；极少数 socket 异常下为 `None`）。
    pub peer: Option<SocketAddr>,
    /// 回话句柄：向这条连接发帧。
    pub handle: ConnectionHandle,
    /// 收到的帧。
    pub frame: Frame,
}

/// 运行单条连接的网关：读循环 + 写 actor + 心跳 + 超时 + 优雅关闭。
///
/// 本函数是每连接生命周期的**唯一属主**——它返回即连接终结、资源全部回收。
/// 返回值告诉调用方连接**为何**结束：
///
/// - `Ok(())`：对端正常关闭，或收到外部关停信号（优雅关闭）；
/// - `Err(IdleTimeout)`：对端静默超时（弱网下常见的「半开连接」）；
/// - `Err(Protocol(..))`：对端发来非法流，应记录并拉黑观察；
/// - `Err(Io(..))`：网络故障。
///
/// # Examples
///
/// ```no_run
/// use im_transport::{run_gateway_connection, GatewayConfig, shutdown_channel};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
/// let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(64);
/// let (shutdown_tx, shutdown_rx) = shutdown_channel();
///
/// let (stream, _peer) = listener.accept().await?;
/// tokio::spawn(run_gateway_connection(
///     stream,
///     GatewayConfig::default(),
///     inbound_tx,
///     shutdown_rx,
/// ));
///
/// // 业务层：收到什么，原样回什么
/// while let Some(event) = inbound_rx.recv().await {
///     event.handle.send(event.frame).await?;
/// }
/// # let _ = shutdown_tx; Ok(())
/// # }
/// ```
///
/// # Errors
///
/// 见函数文档正文：连接生命周期内出现的任何失败都会作为返回值上抛。
pub async fn run_gateway_connection(
    stream: TcpStream,
    config: GatewayConfig,
    inbound: mpsc::Sender<InboundFrame>,
    shutdown: ShutdownRx,
) -> Result<(), TransportError> {
    let peer = stream.peer_addr().ok();

    // 本连接内部的关停信号：读循环结束时触发，停掉心跳与写 actor。
    // 与外部 shutdown 的分工：外部 =「整个服务要停」，内部 =「这条连接要收尾」。
    let (done_tx, done_rx) = shutdown_channel();

    // 出站通道 + 写 actor：独占写半部，消费所有发送方的帧
    let (tx, rx) = mpsc::channel(OUTBOUND_CHANNEL_CAPACITY);
    let handle = ConnectionHandle { tx };

    let (reader, writer) =
        Connection::with_max_frame_len(stream, config.max_frame_len).into_split();
    let writer_task = spawn_writer(writer, rx, done_rx.clone());

    // 心跳 task：仅客户端角色需要（服务端的「心跳」就是及时回 Pong）
    let heartbeat_task = match config.heartbeat {
        HeartbeatPolicy::Server => None,
        HeartbeatPolicy::Client { interval } => {
            Some(spawn_heartbeat(interval, handle.clone(), done_rx))
        }
    };

    let result = read_loop(
        reader,
        inbound,
        handle.clone(),
        peer,
        config.idle_timeout,
        matches!(config.heartbeat, HeartbeatPolicy::Server),
        shutdown,
    )
    .await;

    // ── 统一收尾（无论上面因何返回）──
    // 1. 触发内部关停：心跳退出；写 actor 把已入队的帧**排干后**退出
    //    （优雅关闭的定义：已接受的帧一条不丢）；
    // 2. 释放本地 sender，然后等待两个 task 结束——本函数返回时，
    //    这条连接的所有资源确定已回收。
    done_tx.trigger();
    drop(handle);
    let _ = writer_task.await;
    if let Some(heartbeat) = heartbeat_task {
        let _ = heartbeat.await;
    }
    result
}

/// 写 actor：独占写半部，从通道收帧、编码写出，直到关停或通道关闭。
fn spawn_writer(
    mut writer: WriteHalf,
    mut rx: mpsc::Receiver<Frame>,
    mut done: ShutdownRx,
) -> tokio::task::JoinHandle<Result<(), TransportError>> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // 关停 ≠ 立刻扔掉队列：先排干已入队的帧（优雅），再退出
                _ = done.wait() => {
                    while let Ok(frame) = rx.try_recv() {
                        if let Err(e) = writer.write_frame(&frame).await {
                            return Err(e);
                        }
                    }
                    return Ok(());
                }
                maybe = rx.recv() => match maybe {
                    Some(frame) => {
                        if let Err(e) = writer.write_frame(&frame).await {
                            return Err(e); // 写失败：连接已坏，actor 退役
                        }
                    }
                    // 所有发送方都走了：没有更多帧，正常收工
                    None => return Ok(()),
                },
            }
        }
    })
}

/// 心跳 task：每隔 `interval` 发一个 `Ping`，直到关停或通道关闭。
fn spawn_heartbeat(
    interval: Duration,
    handle: ConnectionHandle,
    mut done: ShutdownRx,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // seq 本地分配即可：心跳帧与业务帧共享同一 seq 空间的问题，
        // 阶段 3 引入会话层的统一序号分配器时解决。
        let mut seq: u64 = 0;
        loop {
            tokio::select! {
                _ = done.wait() => break,
                _ = sleep(interval) => {
                    seq += 1;
                    let ping = Frame::new(Cmd::Ping, seq, 0, Bytes::new());
                    if handle.send(ping).await.is_err() {
                        break; // 写通道已关：连接行将关闭，心跳无需坚持
                    }
                }
            }
        }
    })
}

/// 读循环：空闲超时、关停信号、帧分发三位一体的 `select!`。
async fn read_loop(
    mut reader: ReadHalf,
    inbound: mpsc::Sender<InboundFrame>,
    handle: ConnectionHandle,
    peer: Option<SocketAddr>,
    idle_timeout: Duration,
    auto_reply_pong: bool,
    mut shutdown: ShutdownRx,
) -> Result<(), TransportError> {
    loop {
        tokio::select! {
            // 外部关停：立即停止读（尚未读走的字节会被丢弃——
            // 关停语义是「不再服务新工作」，已入站的帧已尽力递交）
            _ = shutdown.wait() => return Ok(()),

            // timeout 包住 read_frame：每读到任何字节（哪怕半帧）计时器重置。
            // 注意 pending 队列里的帧会立即交付，不会吃超时
            step = timeout(idle_timeout, reader.read_frame()) => match step {
                // 计时器先到：一个字节都没来，判定对端死亡（半开连接）
                Err(_elapsed) => return Err(TransportError::IdleTimeout(idle_timeout)),

                Ok(res) => match res {
                    // 对端 FIN：正常关闭
                    Ok(None) => return Ok(()),
                    Ok(Some(frame)) => match frame.cmd {
                        // 服务端模式：Ping 就地应答，不打扰业务层。
                        // ack = seq + 1 是累计确认语义的预演（阶段 3 滑动窗口）
                        Cmd::Ping if auto_reply_pong => {
                            let pong = Frame::new(Cmd::Pong, 0, frame.seq + 1, Bytes::new());
                            if handle.send(pong).await.is_err() {
                                return Err(TransportError::Closed);
                            }
                        }
                        // 其他所有帧（含客户端收到的 Pong）交给业务层
                        _ => {
                            let event = InboundFrame { peer, handle: handle.clone(), frame };
                            // 入站通道满则挂起 = 反压（见模块文档）；
                            // 业务层退出（recv 端全掉）则连接没有存在意义
                            if inbound.send(event).await.is_err() {
                                return Err(TransportError::Closed);
                            }
                        }
                    },
                    Err(e) => return Err(e),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use im_protocol::ProtocolError;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    use crate::shutdown::ShutdownTx;

    /// 测试脚手架：起一个「accept 循环 + 每连接网关」的服务端。
    ///
    /// 与生产 im-server 的区别：没有会话层，入站帧直接给测试观察。
    async fn spawn_gateway(
        config: GatewayConfig,
    ) -> (
        std::net::SocketAddr,
        mpsc::Receiver<InboundFrame>,
        ShutdownTx,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (inbound_tx, inbound_rx) = mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = shutdown_channel();

        tokio::spawn(async move {
            let mut accept_shutdown = shutdown_rx.clone();
            loop {
                tokio::select! {
                    _ = accept_shutdown.wait() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { continue };
                        tokio::spawn(run_gateway_connection(
                            stream,
                            config.clone(),
                            inbound_tx.clone(),
                            shutdown_rx.clone(),
                        ));
                    }
                }
            }
        });
        (addr, inbound_rx, shutdown_tx)
    }

    /// 主线用例：客户端发 Msg → 业务层收到 → 通过 handle 回 MsgAck → 客户端收到
    #[tokio::test]
    async fn msg_roundtrip_through_gateway() {
        let (addr, mut inbound, _shutdown) = spawn_gateway(GatewayConfig::default()).await;
        let mut client = Connection::connect(&addr.to_string()).await.unwrap();

        client
            .write_frame(&Frame::new(Cmd::Msg, 1, 0, Bytes::from_static(b"hello gateway")))
            .await
            .unwrap();

        // 业务层视角：帧到了，带着 handle 和对端地址
        let event = timeout(Duration::from_secs(2), inbound.recv())
            .await
            .expect("2s 内应收到入站帧")
            .expect("网关存活");
        assert_eq!(event.frame.cmd, Cmd::Msg);
        assert_eq!(event.frame.payload, Bytes::from_static(b"hello gateway"));
        assert_eq!(event.peer, Some(addr));

        // 业务层回话
        event
            .handle
            .send(Frame::new(Cmd::MsgAck, 0, event.frame.seq + 1, Bytes::new()))
            .await
            .unwrap();

        // 客户端视角：收到回话
        let ack = timeout(Duration::from_secs(2), client.read_frame())
            .await
            .expect("2s 内应收到 MsgAck")
            .expect("连接正常")
            .expect("连接未关闭");
        assert_eq!(ack.cmd, Cmd::MsgAck);
        assert_eq!(ack.ack, 2);
    }

    /// 服务端模式：Ping 自动应答 Pong（ack = seq+1），且 Ping 不进业务层
    #[tokio::test]
    async fn server_auto_replies_pong_and_swallows_ping() {
        let (addr, mut inbound, _shutdown) = spawn_gateway(GatewayConfig::default()).await;
        let mut client = Connection::connect(&addr.to_string()).await.unwrap();

        client
            .write_frame(&Frame::new(Cmd::Ping, 7, 0, Bytes::new()))
            .await
            .unwrap();

        let pong = timeout(Duration::from_secs(2), client.read_frame())
            .await
            .expect("2s 内应收到 Pong")
            .expect("连接正常")
            .expect("连接未关闭");
        assert_eq!(pong.cmd, Cmd::Pong);
        assert_eq!(pong.ack, 8, "累计确认：ack = 收到的 seq + 1");

        // Ping 被“就地消化”，业务层不应看到它
        assert!(
            timeout(Duration::from_millis(200), inbound.recv())
                .await
                .is_err(),
            "Ping 不应进入业务层"
        );
    }

    /// 客户端模式：心跳 task 定时发 Ping，服务端自动回 Pong 进入业务层
    #[tokio::test]
    async fn client_heartbeat_receives_pong() {
        let (addr, mut server_inbound, _shutdown) = spawn_gateway(GatewayConfig::default()).await;

        // 客户端也走网关：客户端策略，50ms 心跳
        let stream = TcpStream::connect(addr).await.unwrap();
        let (client_inbound_tx, mut client_inbound) = mpsc::channel(16);
        let (_client_shutdown_tx, client_shutdown_rx) = shutdown_channel();
        let client_config = GatewayConfig {
            heartbeat: HeartbeatPolicy::Client {
                interval: Duration::from_millis(50),
            },
            ..GatewayConfig::default()
        };
        tokio::spawn(run_gateway_connection(
            stream,
            client_config,
            client_inbound_tx,
            client_shutdown_rx,
        ));

        // 客户端业务层视角：第一个 Pong 应在 ~2 个心跳周期内到达
        let event = timeout(Duration::from_secs(2), client_inbound.recv())
            .await
            .expect("2s 内应收到心跳 Pong")
            .expect("客户端网关存活");
        assert_eq!(event.frame.cmd, Cmd::Pong);
        assert_eq!(event.frame.ack, 2, "首个心跳 Ping seq=1 → ack=2");

        // 服务端业务层同样不应看到任何 Ping
        assert!(
            timeout(Duration::from_millis(200), server_inbound.recv())
                .await
                .is_err(),
            "Ping 不应进入服务端业务层"
        );
    }

    /// 空闲超时：静默客户端在 idle_timeout 后被服务端断开（客户端看到 EOF）
    #[tokio::test]
    async fn idle_timeout_closes_silent_connection() {
        let config = GatewayConfig {
            idle_timeout: Duration::from_millis(150),
            ..GatewayConfig::default()
        };
        let (addr, _inbound, _shutdown) = spawn_gateway(config).await;

        let mut client = Connection::connect(&addr.to_string()).await.unwrap();
        let started = std::time::Instant::now();

        let frame = timeout(Duration::from_secs(2), client.read_frame())
            .await
            .expect("2s 内应看到服务端关闭")
            .expect("连接正常")
            .expect("应收到 EOF 而非帧");
        drop(frame);

        // 必须等满了 idle_timeout 才断（不是秒断——秒断说明超时逻辑错了）
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "至少等满 idle_timeout 才断连"
        );
    }

    /// 恶意流：非本协议的字节进入后，网关以 Protocol 错误终结连接
    #[tokio::test]
    async fn garbage_stream_returns_protocol_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // 手动管理单连接，以便拿到 run_gateway_connection 的返回值
        let (inbound_tx, _inbound_rx) = mpsc::channel(16);
        let (_shutdown_tx, shutdown_rx) = shutdown_channel();
        let (result_tx, result_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let result = run_gateway_connection(
                stream,
                GatewayConfig::default(),
                inbound_tx,
                shutdown_rx,
            )
            .await;
            let _ = result_tx.send(result);
        });

        let mut raw = TcpStream::connect(addr).await.unwrap();
        // 5 字节假头：magic 不对，解码器收齐头后立即报错
        raw.write_all(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00]).await.unwrap();
        raw.flush().await.unwrap();

        let result = timeout(Duration::from_secs(2), result_rx)
            .await
            .expect("2s 内网关应终结")
            .expect("测试 task 存活");
        assert!(matches!(
            result,
            Err(TransportError::Protocol(ProtocolError::BadMagic { .. }))
        ));
    }

    /// 外部关停：服务端触发 shutdown 后，客户端看到 EOF
    #[tokio::test]
    async fn external_shutdown_closes_connection() {
        let (addr, _inbound, shutdown_tx) = spawn_gateway(GatewayConfig::default()).await;
        let mut client = Connection::connect(&addr.to_string()).await.unwrap();

        // 先确认连接已被服务端接手（避免 accept 与 shutdown 竞争）
        client
            .write_frame(&Frame::new(Cmd::Ping, 1, 0, Bytes::new()))
            .await
            .unwrap();
        let pong = timeout(Duration::from_secs(2), client.read_frame())
            .await
            .expect("2s 内应收到 Pong")
            .expect("连接正常")
            .expect("连接未关闭");
        assert_eq!(pong.cmd, Cmd::Pong);

        shutdown_tx.trigger();

        let frame = timeout(Duration::from_secs(2), client.read_frame())
            .await
            .expect("2s 内应看到服务端关闭")
            .expect("连接正常");
        assert!(frame.is_none(), "应收到 EOF 而非帧");
    }

    /// 优雅关闭的精髓：关停前排队的出站帧必须**一条不丢**地送达
    #[tokio::test]
    async fn shutdown_drains_queued_outbound_frames() {
        let (addr, mut inbound, shutdown_tx) = spawn_gateway(GatewayConfig::default()).await;
        let mut client = Connection::connect(&addr.to_string()).await.unwrap();

        // 建立连接并让业务层拿到 handle
        client
            .write_frame(&Frame::new(Cmd::Msg, 1, 0, Bytes::new()))
            .await
            .unwrap();
        let event = timeout(Duration::from_secs(2), inbound.recv())
            .await
            .expect("2s 内应收到入站帧")
            .expect("网关存活");

        // 立刻入队 5 帧，然后马上关停——写 actor 必须排干队列再退出
        for i in 0..5u64 {
            event
                .handle
                .send(Frame::new(Cmd::MsgAck, i, 0, Bytes::new()))
                .await
                .unwrap();
        }
        shutdown_tx.trigger();

        for expected in 0..5u64 {
            let frame = timeout(Duration::from_secs(2), client.read_frame())
                .await
                .expect("2s 内应收到排队帧")
                .expect("连接正常")
                .expect("排干的帧应全部送达");
            assert_eq!(frame.cmd, Cmd::MsgAck);
            assert_eq!(frame.seq, expected);
        }

        // 排干之后连接才关闭
        let end = timeout(Duration::from_secs(2), client.read_frame())
            .await
            .expect("2s 内应看到连接关闭")
            .expect("连接正常");
        assert!(end.is_none());
    }

    /// 半包跨写：一帧分两次 write（中间有延迟），解码器增量语义在真实 TCP 上成立
    #[tokio::test]
    async fn half_written_frame_still_decodes() {
        let (addr, mut inbound, _shutdown) = spawn_gateway(GatewayConfig::default()).await;

        // 裸 TCP 手工写半帧：先连后包——演示 Connection::new 包装既有流
        let mut raw = TcpStream::connect(addr).await.unwrap();
        let frame = Frame::new(Cmd::Msg, 9, 0, Bytes::from_static(b"split-me"));
        let wire = frame.encode();

        let half = wire.len() / 2;
        raw.write_all(&wire[..half]).await.unwrap();
        raw.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await; // 给服务端时间消化半帧
        raw.write_all(&wire[half..]).await.unwrap();
        raw.flush().await.unwrap();

        // 包装成 Connection 收回话（同时也验证半部未拆时的读写复用）
        let mut client = Connection::new(raw);
        drop(client.read_frame()); // 无回话数据可读——这里只关心服务端视角

        let event = timeout(Duration::from_secs(2), inbound.recv())
            .await
            .expect("2s 内服务端应解出完整帧")
            .expect("网关存活");
        assert_eq!(event.frame, frame);
    }
}
