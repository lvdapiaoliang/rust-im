//! weak-link：用户态弱网模拟——帧级丢包/延迟/乱序注入 + 可靠性实测。
//!
//! # 测什么（可靠性里程碑：100ms RTT + 10% 丢包 → 到达率）
//!
//! 真实链路最难的三个字是"可重现"：公网上做丢包实验，两次跑的结果
//! 没法比较。所以把弱网**搬进用户态**——在客户端与服务端之间架一个
//! 帧级代理，按确定性随机模型注入：
//!
//! - **丢包**（`--loss-permille`）：整帧丢弃，概率可复现（xorshift64*，
//!   同一种子同一序列——"概率丢包模型"是 roadmap §4.5 的算法落点）；
//! - **延迟**（`--rtt`）：单向注入 RTT/2，模拟物理距离；
//! - **乱序**（`--jitter`）：0..jitter 的均匀抖动叠加在延迟上——
//!   不同帧的到达时刻交错，自然产生乱序（调度侧用**小顶堆**按时出队，
//!   roadmap §4.5 的第二个落点）。
//!
//! # 为什么是帧级而不是字节级
//!
//! 字节级丢包会把流内字节撕开一个洞，后续所有帧都会校验失败——
//! 那测的是"协议如何死于字节损坏"，不是"应用如何在帧丢失下自愈"。
//! IM 的可靠性语义（seq 去重、client_msg_id 核销、指数退避重传）
//! 全部以帧为单位，所以注入也以帧为单位。
//!
//! # 被测路径是真实的
//!
//! 不 mock 客户端：两个真正的 [`im_client::run_client`]（完整的连接状态机、
//! 重发表、本地库、接收去重）穿过弱网代理连到真正的 [`im_server`]。
//! 压测工具只做三件事：发消息、数事件、对账。
//!
//! 学习文档：`docs/16-perf.md`（到达率与延迟分布的实测数据归档在那里）。

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Args;
use im_client::{ClientConfig, ClientEvent, ClientHandle};
use im_protocol::{Cmd, DEFAULT_MAX_FRAME_LEN, Frame, Handshake, HandshakeAck, Payload};
use im_server::{SessionConfig, StaticToken};
use im_transport::{Connection, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::histogram::LatencyHist;

// ────────────────────────────────────────────────────────────────
// 确定性随机源：xorshift64*（概率丢包模型的引擎）
// ────────────────────────────────────────────────────────────────

/// xorshift64*：一次移位三连 + 乘法混合，64 位状态零依赖。
///
/// 选它而不是 `rand` crate：弱网注入只需要"均匀、便宜、可复现"——
/// `next_u64` 约 3ns 且无分配；同一种子给出同一序列，
/// 任何一次实验都能精确重放（比统计质量更重要的是**可重现**）。
pub struct XorShift {
    /// 零状态会让移位序列死在 0，构造时强制非零。
    state: u64,
}

impl XorShift {
    /// 以种子构造（0 归一为奇数常数，避免零状态）。
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self { state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed } }
    }

    /// 下一个 64 位值（Marsaglia xorshift64* 常数）。
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// [0, n) 均匀取值：模偏差 < n/2^64，对千分位档的丢包率完全可忽略。
    #[must_use]
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

// ────────────────────────────────────────────────────────────────
// 弱网链路：单向泵（读侧注入，调度侧按时出队）
// ────────────────────────────────────────────────────────────────

/// 单向链路配置。
#[derive(Debug, Clone, Copy)]
struct LinkCfg {
    /// 基础单向延迟。
    delay: Duration,
    /// 附加抖动上限（0 = 不抖动即不乱序）。
    jitter: Duration,
    /// 丢包率（千分之几；1000 = 全丢）。
    loss_permille: u32,
    /// 本方向的随机种子（可复现的关键）。
    seed: u64,
}

/// 单向链路计数（报告与对账）。
#[derive(Debug, Default)]
struct LinkStats {
    /// 成功转发的帧数。
    forwarded: AtomicU64,
    /// 被丢弃的帧数（注入的丢包）。
    dropped: AtomicU64,
    /// 穿过本方向的 Msg 帧（含重传——放大倍数的分子）。
    msg_seen: AtomicU64,
}

impl LinkStats {
    fn bump(&self, counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// 双向统计的持有者（代理返回给场景读数）。
#[derive(Debug, Default)]
pub struct ProxyStats {
    /// 上行（客户端 → 服务端）。
    up: LinkStats,
    /// 下行（服务端 → 客户端）。
    down: LinkStats,
}

/// 一个弱网代理：监听本地端口，把流量注入损伤后转发给上游。
pub struct Proxy {
    /// 客户端应连接的地址。
    pub addr: SocketAddr,
    /// 双向统计（场景随时读，代理 task 持另一半 Arc）。
    pub stats: Arc<ProxyStats>,
}

/// 启动代理（返回即开始监听；task 的生命周期跟随进程）。
///
/// # Errors
///
/// 端口绑定失败时返回 IO 错误。
pub async fn spawn_proxy(
    upstream: SocketAddr,
    up_cfg: LinkCfg,
    down_cfg: LinkCfg,
) -> std::io::Result<Proxy> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let stats = Arc::new(ProxyStats::default());
    let accept_stats = Arc::clone(&stats);
    tokio::spawn(accept_loop(listener, upstream, up_cfg, down_cfg, accept_stats));
    Ok(Proxy { addr, stats })
}

/// accept 循环：每条连接两个方向各起一对泵 task。
async fn accept_loop(
    listener: TcpListener,
    upstream: SocketAddr,
    up_cfg: LinkCfg,
    down_cfg: LinkCfg,
    stats: Arc<ProxyStats>,
) {
    let mut conn_seq: u64 = 0;
    loop {
        let Ok((client, _)) = listener.accept().await else {
            return; // 监听器被收走（进程收尾）
        };
        let Ok(server) = TcpStream::connect(upstream).await else {
            continue; // 上游不可达：丢弃这条客户端连接（其读侧自然 EOF）
        };
        conn_seq += 1;
        // 每条连接每方向独立随机源：种子混合连接序号与方向标签——
        // 同一次运行内各连接的丢包序列互不相同，但整场实验仍可复现
        let up = LinkCfg { seed: up_cfg.seed ^ (conn_seq << 1), ..up_cfg };
        let down = LinkCfg { seed: down_cfg.seed ^ (conn_seq << 1) ^ 1, ..down_cfg };
        spawn_direction(client, server, up, down, &stats);
    }
}

/// 架起一个方向：读侧 task（注入丢包/算到达时刻）+ 调度侧 task（按时出队写出）。
fn spawn_direction(from: TcpStream, to: TcpStream, cfg: LinkCfg, stats: &LinkStats) {
    let reader = Connection::with_max_frame_len(from, DEFAULT_MAX_FRAME_LEN).into_split().0;
    let writer = Connection::with_max_frame_len(to, DEFAULT_MAX_FRAME_LEN).into_split().1;
    let (tx, rx) = mpsc::channel::<(Frame, tokio::time::Instant)>(256);
    tokio::spawn(pump_in(reader, tx, cfg, stats));
    tokio::spawn(pump_out(rx, writer, stats));
}

/// 读侧泵：帧到达 → 判丢包 → 算到达时刻 → 交给调度。
///
/// **取消安全说明**：本 task 从不被取消（它拥有整条方向的输入半部），
/// `read_frame` 的内部缓冲不必考虑半读状态——这是把"读"与"调度"
/// 拆成两个 task 的原因（select 循环里直接 read_frame 会踩取消安全的坑，
/// docs/20 §3.3）。
async fn pump_in(
    mut reader: ReadHalf,
    tx: mpsc::Sender<(Frame, tokio::time::Instant)>,
    cfg: LinkCfg,
    stats: &LinkStats,
) {
    let mut rng = XorShift::new(cfg.seed);
    loop {
        match reader.read_frame().await {
            Ok(Some(frame)) => {
                if frame.cmd == Cmd::Msg {
                    stats.bump(&stats.msg_seen);
                }
                if u64::from(cfg.loss_permille) > 0 && rng.below(1000) < u64::from(cfg.loss_permille) {
                    stats.bump(&stats.dropped);
                    continue; // 整帧丢弃：流上不留任何痕迹（帧级注入的本质）
                }
                // 到达时刻 = 现在 + 基础延迟 + [0, jitter) 抖动（抖动才产生乱序）
                let extra = if cfg.jitter.is_zero() {
                    Duration::ZERO
                } else {
                    // as_millis 是 u128：毫秒级抖动上限对 u64 毫无截断风险，try_from 收口
                    let jitter_ms = u64::try_from(cfg.jitter.as_millis()).expect("抖动毫秒数装得下 u64");
                    Duration::from_millis(rng.below(jitter_ms))
                };
                let ready = tokio::time::Instant::now() + cfg.delay + extra;
                if tx.send((frame, ready)).await.is_err() {
                    return; // 调度侧已退（写半部死亡）
                }
            }
            _ => return, // EOF（对端关闭）或 IO 故障：本方向收工
        }
    }
}

/// 调度侧泵：小顶堆按到达时刻出队，相等时刻按入队序（FIFO 保底）。
///
/// 堆元素 `(到达时刻, 入队序, 帧)`：序号作平局裁决——抖动为零时
/// 严格保序（纯延迟链路不引入人为乱序），抖动非零时到达时刻交错，
/// 乱序自然发生且可复现。
async fn pump_out(
    mut rx: mpsc::Receiver<(Frame, tokio::time::Instant)>,
    mut writer: WriteHalf,
    stats: &LinkStats,
) {
    let mut heap: BinaryHeap<Reverse<(tokio::time::Instant, u64, Frame)>> = BinaryHeap::new();
    let mut seq: u64 = 0;
    let mut rx_open = true;
    loop {
        if !heap.is_empty() {
            let due = heap.peek().expect("非空堆必有顶").0.0;
            tokio::select! {
                item = rx.recv(), if rx_open => match item {
                    Some((frame, ready)) => {
                        seq += 1;
                        heap.push(Reverse((ready, seq, frame)));
                    }
                    None => rx_open = false,
                },
                _ = tokio::time::sleep_until(due) => {
                    let Reverse((_, _, frame)) = heap.pop().expect("非空堆必能弹出");
                    if writer.write_frame(&frame).await.is_err() {
                        return; // 写半部死亡：丢弃余下帧，方向收工
                    }
                    stats.bump(&stats.forwarded);
                }
            }
        } else if rx_open {
            match rx.recv().await {
                Some((frame, ready)) => {
                    seq += 1;
                    heap.push(Reverse((ready, seq, frame)));
                }
                None => rx_open = false,
            }
        } else {
            return; // 堆已排空且读侧已关：在途帧全部送达，方向收工
        }
    }
}

// ────────────────────────────────────────────────────────────────
// 可靠性场景：两个真实客户端穿过弱网，数事件、对账、出报告
// ────────────────────────────────────────────────────────────────

/// `weak-link` 场景参数。
#[derive(Debug, Args)]
pub struct WeakLinkArgs {
    /// 发送的消息条数
    #[arg(long, default_value_t = 200)]
    pub messages: usize,
    /// 每方向丢包率（千分之几；100 = 10%）
    #[arg(long, default_value_t = 100)]
    pub loss_permille: u32,
    /// 模拟往返延迟（毫秒，平分到两个方向）
    #[arg(long, default_value_t = 100)]
    pub rtt_ms: u64,
    /// 抖动上限（毫秒；> 0 时会产生乱序）
    #[arg(long, default_value_t = 0)]
    pub jitter_ms: u64,
    /// 随机种子（同种子 = 可精确重放的同一实验）
    #[arg(long, default_value_t = 20_260_925)]
    pub seed: u64,
    /// 客户端重传 RTO（毫秒）
    #[arg(long, default_value_t = 300)]
    pub retry_timeout_ms: u64,
    /// 单条消息最大尝试次数（含首次）
    #[arg(long, default_value_t = 8)]
    pub retry_max_attempts: u32,
    /// 整场实验的墙钟上限（秒）
    #[arg(long, default_value_t = 120)]
    pub deadline_secs: u64,
    /// 消息载荷字节数
    #[arg(long, default_value_t = 64)]
    pub payload_bytes: usize,
}

/// 场景的可调内核（CLI 参数与测试参数汇到这里）。
struct ReliabilityCfg {
    messages: usize,
    up: LinkCfg,
    down: LinkCfg,
    retry_timeout: Duration,
    retry_max_attempts: u32,
    deadline: Duration,
    payload_bytes: usize,
    /// 客户端本地库根目录（两个用户各占一个子目录）。
    data_base: PathBuf,
}

/// 对账结果（场景报告与测试断言共用同一份事实）。
struct ReliabilityOutcome {
    /// 发出（入重发表）的消息数。
    sent: usize,
    /// 服务端确认（Ack）数。
    acked: usize,
    /// 客户端放弃（重传耗尽）数。
    failed: usize,
    /// 接收端**去重后**实收的消息数。
    received: usize,
    /// 发送端经历的断线次数（握手被丢包 → 重连）。
    reconnects: u64,
    /// 上行穿过的 Msg 帧总数（含重传——放大倍数的分子）。
    upstream_attempts: u64,
    /// 端到端延迟分布（发送入队 → 接收端实收）。
    latency: LatencyHist,
}

/// 场景主体：起服务 + 架代理 + 双客户端 → 发 N 条 → 对账。
///
/// # Errors
///
/// 任一客户端握手被拒（不应发生——口令正确）或装配失败时提前退出。
async fn run_reliability(cfg: ReliabilityCfg) -> Result<ReliabilityOutcome> {
    anyhow::ensure!(cfg.messages >= 1, "至少发一条消息");
    anyhow::ensure!(cfg.loss_permille <= 1000, "丢包率不能超过 1000‰");

    // ── 装配：真实服务端 + 弱网代理 ──
    let server_cfg = SessionConfig {
        authenticator: Arc::new(StaticToken { token: "weak".to_string() }),
        ..SessionConfig::default()
    };
    let (server_addr, _sessions, server_shutdown) =
        im_server::spawn_server(server_cfg).await.context("启动被测服务端")?;
    let proxy =
        spawn_proxy(server_addr, cfg.up, cfg.down).await.context("架设弱网代理")?;

    // ── 双客户端：真实 run_client（重传/去重/落盘全套）穿过代理 ──
    // data_dir 纪律（docs/20 §4.1）：同一用户全程一个目录——去重键的
    // 唯一性来自"分配器与存储同生命周期"。开场清目录 = 换一场新实验，
    // 但**运行中**（含重连）绝不换。
    for user in ["alice", "bob"] {
        let dir = cfg.data_base.join(user);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("建本地库目录 {dir:?}"))?;
    }

    let (alice, mut alice_events) =
        spawn_client(proxy.addr, 1, &cfg).await.context("启动 alice 客户端")?;
    let (bob, mut bob_events) =
        spawn_client(proxy.addr, 2, &cfg).await.context("启动 bob 客户端")?;

    // ── 等两端就位（握手帧本身要过弱网——丢包时靠客户端重连重试）──
    let mut alice_ready = false;
    let mut bob_ready = false;
    let ready_deadline = Instant::now() + Duration::from_secs(20);
    while !(alice_ready && bob_ready) {
        if Instant::now() > ready_deadline {
            anyhow::bail!("20s 内未完成双端握手（丢包 {permille}‰ 下重连应能成功）", permille = cfg.up.loss_permille);
        }
        tokio::select! {
            ev = alice_events.recv() => match ev {
                Some(ClientEvent::Connected { .. }) => alice_ready = true,
                Some(ClientEvent::Disconnected) => {} // 重连中：继续等
                Some(_) => {}
                None => anyhow::bail!("alice 客户端意外退出"),
            },
            ev = bob_events.recv() => match ev {
                Some(ClientEvent::Connected { .. }) => bob_ready = true,
                Some(ClientEvent::Disconnected) => {}
                Some(_) => {}
                None => anyhow::bail!("bob 客户端意外退出"),
            },
        }
    }

    // ── 发送：N 条背靠背入队（重发表自己管可靠性，场景只管数数）──
    let content = bytes::Bytes::from(vec![b'w'; cfg.payload_bytes]);
    for _ in 0..cfg.messages {
        alice.send_msg(2, content.clone()).await.context("消息入队")?;
    }

    // ── 事件对账：t0 以 MessageQueued 为准（那是 client_msg_id 的生日）──
    let mut t0: HashMap<u64, Instant> = HashMap::new();
    let mut acked: HashSet<u64> = HashSet::new();
    let mut failed: HashSet<u64> = HashSet::new();
    let mut received: HashSet<u64> = HashSet::new();
    let mut reconnects: u64 = 0;
    let mut latency = LatencyHist::new();
    let sent = cfg.messages;

    // 第一阶段：等全部消息"已确认或已放弃"（发送侧收口）
    let deadline = Instant::now() + cfg.deadline;
    while acked.len() + failed.len() < sent {
        if Instant::now() > deadline {
            break; // 进入第二阶段用剩余时间收在途消息
        }
        let ev = next_event(&mut alice_events, &mut bob_events, deadline).await;
        match ev {
            Some((Side::Alice, ClientEvent::MessageQueued { client_msg_id, .. })) => {
                t0.insert(client_msg_id, Instant::now());
            }
            Some((Side::Alice, ClientEvent::Ack { client_msg_id, .. })) => {
                acked.insert(client_msg_id);
            }
            Some((Side::Alice, ClientEvent::SendFailed { client_msg_id })) => {
                failed.insert(client_msg_id);
            }
            Some((Side::Alice, ClientEvent::Disconnected)) => reconnects += 1,
            Some((Side::Bob, ClientEvent::Message(msg))) => {
                // 只统计本场的消息（防御：本地库残留/未来场景的旁路消息不计数）
                if t0.contains_key(&msg.client_msg_id) {
                    record_receive(&t0, &mut received, &mut latency, msg.client_msg_id);
                }
            }
            Some(_) => {} // Connected/Disconnected(Bob)/SyncBatch 等：不参与对账
            None => break, // 客户端事件流关闭：停止等待
        }
    }

    // 第二阶段：在途消息的收尾排干（发送侧收口 ≠ 接收侧到齐；
    // 已 Ack 的消息可能还在下行链路的堆里等出队时刻）
    let grace = Instant::now() + Duration::from_secs(5);
    while received.len() < sent {
        if Instant::now() > grace {
            break;
        }
        let ev = next_event(&mut alice_events, &mut bob_events, grace).await;
        match ev {
            Some((Side::Alice, ClientEvent::MessageQueued { client_msg_id, .. })) => {
                t0.insert(client_msg_id, Instant::now());
            }
            Some((Side::Bob, ClientEvent::Message(msg))) => {
                if t0.contains_key(&msg.client_msg_id) {
                    record_receive(&t0, &mut received, &mut latency, msg.client_msg_id);
                }
            }
            Some(_) => {}
            None => break,
        }
    }

    // ── 收尾：停客户端与服务端（本地库留在 data_base 供检查）──
    drop(alice);
    drop(bob);
    server_shutdown.trigger();

    Ok(ReliabilityOutcome {
        sent,
        acked: acked.len(),
        failed: failed.len(),
        received: received.len(),
        reconnects,
        upstream_attempts: proxy.stats.up.msg_seen.load(Ordering::Relaxed),
        latency,
    })
}

/// 事件来源标记（对账时区分 alice 侧与 bob 侧）。
enum Side {
    Alice,
    Bob,
}

/// 从两条事件流里取一条（谁先到取谁；双闭返回 None）。
async fn next_event(
    alice: &mut mpsc::Receiver<ClientEvent>,
    bob: &mut mpsc::Receiver<ClientEvent>,
    until: Instant,
) -> Option<(Side, ClientEvent)> {
    let wait = until.saturating_duration_since(Instant::now());
    tokio::select! {
        ev = alice.recv() => ev.map(|e| (Side::Alice, e)),
        ev = bob.recv() => ev.map(|e| (Side::Bob, e)),
        _ = tokio::time::sleep(wait) => None,
    }
}

/// 首次实收：记账 + 记延迟（重复投递靠 set 语义天然去重——计数即对账）。
fn record_receive(
    t0: &HashMap<u64, Instant>,
    received: &mut HashSet<u64>,
    latency: &mut LatencyHist,
    client_msg_id: u64,
) {
    if received.insert(client_msg_id) {
        if let Some(&born) = t0.get(&client_msg_id) {
            latency.record(born.elapsed());
        }
    }
}

/// 起一个真实客户端（run_client：完整状态机 + 重发表 + 本地库）。
async fn spawn_client(
    proxy_addr: SocketAddr,
    user_id: u64,
    cfg: &ReliabilityCfg,
) -> Result<(ClientHandle, mpsc::Receiver<ClientEvent>)> {
    let (events_tx, events_rx) = mpsc::channel(1024);
    let (shutdown_tx, shutdown_rx) = im_transport::shutdown_channel();
    // docs/20 §3.1 的坑就在这：ShutdownTx 若随本函数返回被 drop，
    // "sender 全掉 = 视为关停"的语义会当场杀死客户端。压测全程客户端
    // 应活着、进程退出统一回收——forget 一个 watch sender（几十字节）
    // 是测试与压测场景的标准答案。
    std::mem::forget(shutdown_tx);
    let config = ClientConfig {
        data_dir: Some(cfg.data_base.join(if user_id == 1 { "alice" } else { "bob" })),
        heartbeat_interval: Duration::from_secs(5),
        retry_timeout: cfg.retry_timeout,
        retry_max_attempts: cfg.retry_max_attempts,
        ..ClientConfig::new(proxy_addr.to_string(), user_id, "weak")
    };
    let handle = im_client::run_client(config, events_tx, shutdown_rx);
    Ok((handle, events_rx))
}

/// `weak-link` 子命令：跑场景 → 报告（数据归档由 docs/16 承担）。
///
/// # Errors
///
/// 场景装配或对账失败时上抛。
pub async fn weak_link(args: &WeakLinkArgs) -> Result<()> {
    let one_way = Duration::from_millis(args.rtt_ms / 2);
    let jitter = Duration::from_millis(args.jitter_ms);
    let cfg = ReliabilityCfg {
        messages: args.messages,
        up: LinkCfg { delay: one_way, jitter, loss_permille: args.loss_permille, seed: args.seed },
        down: LinkCfg { delay: one_way, jitter, loss_permille: args.loss_permille, seed: args.seed ^ 0x5DEE_CE66 },
        retry_timeout: Duration::from_millis(args.retry_timeout_ms),
        retry_max_attempts: args.retry_max_attempts,
        deadline: Duration::from_secs(args.deadline_secs),
        payload_bytes: args.payload_bytes,
        data_base: std::env::temp_dir().join("im-bench-weaklink"),
    };

    let outcome = run_reliability(cfg).await?;
    report(args, &outcome);
    Ok(())
}

/// 报告输出（到达率是可靠性里程碑的主指标）。
fn report(args: &WeakLinkArgs, o: &ReliabilityOutcome) {
    println!("──── im-bench · weak-link ───────────────────────");
    println!(
        "链路            : RTT {}ms（每方向 {}ms）+ 每方向丢包 {}‰ + 抖动 0..{}ms",
        args.rtt_ms,
        args.rtt_ms / 2,
        args.loss_permille,
        args.jitter_ms
    );
    println!(
        "客户端          : 真实 run_client（重传 RTO {}ms × 最多 {} 次）",
        args.retry_timeout_ms, args.retry_max_attempts
    );
    println!(
        "发送            : {} 条（载荷 {} 字节，种子 {}）",
        o.sent, args.payload_bytes, args.seed
    );
    println!(
        "服务端确认      : {} 条   放弃(重传耗尽): {} 条   发送端断线重连: {} 次",
        o.acked, o.failed, o.reconnects
    );
    let arrival_permille = u128::from(o.received as u64) * 1000 / u128::try_from(o.sent).unwrap_or(1);
    println!(
        "接收端实收      : {} 条（去重后口径）→ 到达率 {}‰（{}.{}%）",
        o.received,
        arrival_permille,
        arrival_permille / 10,
        arrival_permille % 10
    );
    if o.latency.count() > 0 {
        println!(
            "端到端延迟      : P50 {}   P90 {}   P99 {}   max {}",
            fmt_dur(o.latency.percentile(50)),
            fmt_dur(o.latency.percentile(90)),
            fmt_dur(o.latency.percentile(99)),
            fmt_dur(o.latency.max().expect("count>0 时必有 max")),
        );
    }
    // 整数算放大倍数（测量代码不掺浮点，与吞吐口径同款纪律）：
    // attempts×100/sent 得"百分倍"，再拆整部与小数两位
    let amp_pct = u128::from(o.upstream_attempts) * 100 / u128::try_from(o.sent).unwrap_or(1);
    println!(
        "对账            : 上行 Msg 帧 {} 个（含重传，每条消息平均尝试 {}.{} 次）",
        o.upstream_attempts,
        amp_pct / 100,
        amp_pct % 100
    );
    println!("口径说明        : 帧级注入（丢整帧）；接收计数在去重后；数据归档 docs/16");
}

/// 时长人读格式（µs / ms / s，与 connstorm 同款——报告工具的就近复制）。
fn fmt_dur(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1_000 {
        format!("{us}µs")
    } else if us < 1_000_000 {
        format!("{}.{:02}ms", us / 1_000, (us % 1_000) / 10)
    } else {
        format!("{}.{:02}s", us / 1_000_000, (us % 1_000_000) / 10_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use im_protocol::Msg;
    use im_server::AllowAll;
    use std::time::Duration;

    /// xorshift：同种子同序列（可复现是弱网实验的第一属性）。
    #[test]
    fn xorshift_is_deterministic() {
        let mut a = XorShift::new(42);
        let mut b = XorShift::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    /// 零种子被归一（零状态会让移位序列死在 0）。
    #[test]
    fn xorshift_zero_seed_is_normalized() {
        let mut rng = XorShift::new(0);
        assert_ne!(rng.next_u64(), 0);
    }

    /// below(n) 落在 [0, n)：万次采样边界校验。
    #[test]
    fn xorshift_below_stays_in_range() {
        let mut rng = XorShift::new(7);
        for _ in 0..10_000 {
            assert!(rng.below(1000) < 1000);
        }
        assert_eq!(rng.below(0), 0, "n=0 时不 panic");
    }

    /// 丢包序列的分布健全性：10% 丢包率在 10 万次采样中落在 9.5%~10.5%。
    #[test]
    fn loss_draw_matches_configured_rate() {
        let mut rng = XorShift::new(123);
        let dropped = (0..100_000).filter(|_| rng.below(1000) < 100).count();
        let permille = dropped * 10; // 10 万次 → 千分数
        assert!((9_500..=10_500).contains(&permille), "实际丢包 {permille}‰ 偏离 100‰");
    }

    /// 直通链路（0 丢包 0 延迟）：代理是透明的——握手 + 消息往返全通。
    #[tokio::test]
    async fn passthrough_link_is_transparent() {
        let (addr, _sessions, shutdown) = im_server::spawn_server(SessionConfig {
            authenticator: Arc::new(AllowAll),
            ..SessionConfig::default()
        })
        .await
        .expect("服务应能启动");
        let clean = LinkCfg { delay: Duration::ZERO, jitter: Duration::ZERO, loss_permille: 0, seed: 1 };
        let proxy = spawn_proxy(addr, clean, clean).await.expect("代理应能监听");

        let mut conn = Connection::connect(&proxy.addr.to_string()).await.expect("穿过代理应能连上");
        let hs = Handshake { user_id: 1, token: "any".into() };
        conn.write_frame(&hs.encode_frame(1, 0)).await.expect("握手帧应能写出");
        let frame = tokio::time::timeout(Duration::from_secs(2), conn.read_frame())
            .await
            .expect("2s 内应有应答")
            .expect("连接正常")
            .expect("连接未关闭");
        let ack = HandshakeAck::decode_frame(&frame).expect("应答应是 HandshakeAck");
        assert!(ack.session_id > 0, "全放行服务端应接受");

        let msg = Msg { from: 0, to: 2, msg_id: 0, client_msg_id: 1, content: bytes::Bytes::from_static(b"p") };
        conn.write_frame(&msg.encode_frame(2, 0)).await.expect("消息应能写出");
        let frame = tokio::time::timeout(Duration::from_secs(2), conn.read_frame())
            .await
            .expect("2s 内应有回执")
            .expect("连接正常")
            .expect("连接未关闭");
        assert_eq!(frame.cmd, Cmd::MsgAck);
        drop(conn);
        shutdown.trigger();
    }

    /// 全丢链路：帧进得去出不来（注入器不是摆设）。
    #[tokio::test]
    async fn total_loss_link_swallows_frames() {
        let (addr, _sessions, shutdown) = im_server::spawn_server(SessionConfig {
            authenticator: Arc::new(AllowAll),
            ..SessionConfig::default()
        })
        .await
        .expect("服务应能启动");
        let black_hole =
            LinkCfg { delay: Duration::ZERO, jitter: Duration::ZERO, loss_permille: 1000, seed: 9 };
        let proxy = spawn_proxy(addr, black_hole, black_hole).await.expect("代理应能监听");

        let mut conn = Connection::connect(&proxy.addr.to_string()).await.expect("TCP 层应能连上");
        let hs = Handshake { user_id: 1, token: "any".into() };
        conn.write_frame(&hs.encode_frame(1, 0)).await.expect("写出不受丢包影响");
        let none = tokio::time::timeout(Duration::from_millis(500), conn.read_frame()).await;
        assert!(none.is_err(), "500ms 内不应有任何帧穿过黑洞链路");
        drop(conn);
        shutdown.trigger();
    }

    /// 可靠性最小端到端：30% 每向丢包 + 20ms RTT 下 20 条消息全部实收——
    /// 重传 + 去重 + 幂等核销在真实客户端里闭环（可靠性里程碑的缩影）。
    #[tokio::test]
    async fn reliability_holds_under_heavy_loss() {
        let one_way = Duration::from_millis(10);
        let cfg = ReliabilityCfg {
            messages: 20,
            up: LinkCfg { delay: one_way, jitter: Duration::ZERO, loss_permille: 300, seed: 11 },
            down: LinkCfg { delay: one_way, jitter: Duration::ZERO, loss_permille: 300, seed: 13 },
            retry_timeout: Duration::from_millis(80),
            retry_max_attempts: 15,
            deadline: Duration::from_secs(60),
            payload_bytes: 32,
            data_base: std::env::temp_dir().join("im-bench-weaklink-test"),
        };
        let outcome = run_reliability(cfg).await.expect("场景应能完成");
        assert_eq!(outcome.sent, 20);
        assert_eq!(outcome.received, 20, "重传应把丢包吃干净：实收 {}", outcome.received);
        assert_eq!(outcome.failed, 0, "15 次尝试不应有耗尽");
        assert!(outcome.acked <= 20, "确认数不可能超过发送数");
        assert!(
            outcome.upstream_attempts >= 20,
            "30% 丢包下上行必有重传（尝试 {} 次）",
            outcome.upstream_attempts
        );
        assert!(outcome.latency.count() > 0, "有实收就有延迟样本");
    }
}
