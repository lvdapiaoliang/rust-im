//! conn-storm：连接风暴——三级性能里程碑的测量台（阶段 10）。
//!
//! # 测什么（与 group-fanout 的分工）
//!
//! - `group-fanout`（阶段 7）：**热路径**——扇出引擎的吞吐与延迟；
//! - `conn-storm`（本模块）：**规模**——真实 TCP 三次握手 + 真实握手帧
//!   走完整服务端路径（网关 task + 会话 task + 路由注册），测：
//!   1. 建连速率（风暴口径：能以多快速度接入 N 条连接）；
//!   2. **每连接内存成本**（三级里程碑的核心账本：10 万 → 100 万 →
//!      500 万，纵向天花板由 `内存/连接` 决定）；
//!   3. 稳态保持（hold 阶段：连接不掉、内存不涨）；
//!   4. 拆除（drop 全部连接后路由表清零——回收不泄漏的证明）。
//!
//! # 刻意排除与诚实口径
//!
//! - 客户端与服务端**同进程**（回环 TCP）：每连接成本 = 客户端半连接 +
//!   服务端全套（网关 task + 会话 task + 双向通道），报告按"每连接总成本"
//!   口径陈述——比单侧口径保守，不美化；
//! - 服务端 60s 读空闲断连（[`im_transport::DEFAULT_IDLE_TIMEOUT`]）：
//!   压测连接由 20s 一次的 Ping 扫掠保活（10 万目标的建连相本身就
//!   超过 60s，“客户端不发心跳”的旧假设只在 1 万规模成立），
//!   `--hold-secs` 上限仍 45s（扫掠不豁免拆除验证的纪律）；
//! - 端口口径（本机实测，Windows 与 Linux 不同）：Windows 的动态端口池
//!   是**全局共享**的（不按源地址分区）——10 万连接三次停在 ~55,490
//!   （os error 10055，≈池容量 55,536）；所以压测客户端**显式 bind 源
//!   端口**（20000..=65535，每源 IP 独享 45,536 个，bind 不受动态池约束），
//!   `--source-ips` 在 127/8 回环段轮换源地址（127.x.x.x 整段都是回环）。
//!
//! 学习文档：`docs/16-perf.md`（实测数据与三级里程碑的账本都在那里）。

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use clap::Args;
use im_protocol::{Cmd, Frame, Handshake, HandshakeAck, Payload};
use im_server::{SessionConfig, StaticToken};
use im_transport::Connection;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::TcpStream;
use tokio::task::JoinSet;

use crate::memstats;

/// 保活扫掠间隔：明显小于服务端 60s 读空闲纪律的两留量。
const SWEEP_INTERVAL: Duration = Duration::from_secs(20);

/// 连接风暴参数。
#[derive(Debug, Args)]
pub struct ConnStormArgs {
    /// 目标连接总数（M1 里程碑：100000）
    #[arg(long, default_value_t = 10_000)]
    pub connections: usize,
    /// 每波并行连接数（风暴节奏：一波建完再下一波）
    #[arg(long, default_value_t = 250)]
    pub wave: usize,
    /// 源 IP 轮换数（127.0.0.1..=127.0.0.N；每 IP 显式绑定 45,536 个源端口）
    #[arg(long, default_value_t = 1)]
    pub source_ips: u32,
    /// 稳态保持秒数（服务端 60s 读空闲断连，上限 45）
    #[arg(long, default_value_t = 5)]
    pub hold_secs: u64,
    /// 单条握手的等待上限（毫秒）
    #[arg(long, default_value_t = 5_000)]
    pub handshake_timeout_ms: u64,
}

/// 绑定本地地址的阻塞连接（socket2：std/tokio 都没有"先 bind 后 connect"）。
///
/// `local` 的端口为 0 时由系统自动分配（受动态端口池约束）；压测风暴用
/// [`source_addr`] 显式指定端口绕开 Windows 的全局端口池（见其文档）。
/// 阻塞 connect 只发生在 `spawn_blocking` 池里（回环连接 ~50µs 完成，
/// 不拖累异步运行时）；连接完成后转非阻塞交给 tokio。
fn connect_bound(local: SocketAddr, server: SocketAddr) -> io::Result<TcpStream> {
    let domain = if server.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let sock = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    sock.set_tcp_nodelay(true)?; // 建连/握手延迟是本场景的测量对象
    sock.bind(&SockAddr::from(local))?;
    sock.connect(&SockAddr::from(server))?;
    sock.set_nonblocking(true)?;
    TcpStream::from_std(sock.into())
}

/// 在一条连接上完成协议握手：发 `Handshake`，等 `HandshakeAck`。
///
/// # Errors
///
/// 写失败、超时、连接关闭或服务端拒绝（`session_id` == 0）都报错，
/// 错误串进入失败分类统计。
async fn handshake(
    conn: &mut Connection,
    user_id: u64,
    token: &str,
    wait: Duration,
) -> Result<(), String> {
    let hs = Handshake { user_id, token: token.to_string() };
    conn.write_frame(&hs.encode_frame(1, 0)).await.map_err(|e| format!("写握手帧: {e}"))?;
    let frame = tokio::time::timeout(wait, conn.read_frame())
        .await
        .map_err(|_| "等待握手应答超时".to_string())?
        .map_err(|e| format!("读握手应答: {e}"))?
        .ok_or_else(|| "连接在应答前关闭".to_string())?;
    let ack = HandshakeAck::decode_frame(&frame).map_err(|e| format!("应答解码: {e}"))?;
    if ack.session_id == 0 {
        return Err(format!("服务端拒绝: {}", ack.reason));
    }
    Ok(())
}

/// 场景主体：装配 → 波次风暴（建连/握手分相）→ 稳态 → 拆除 → 报告。
///
/// # Errors
///
/// 服务起不来、或浪潮结束后的守恒校验（在线数 == 成功数）不过时报错退出
/// ——压测结果建立在计数对得上才有意义（docs/20 §4.3）。
pub async fn conn_storm(args: &ConnStormArgs) -> Result<()> {
    anyhow::ensure!(args.connections >= 1, "--connections 至少为 1");
    anyhow::ensure!(args.wave >= 1, "--wave 至少为 1");
    anyhow::ensure!(args.source_ips >= 1, "--source-ips 至少为 1");
    anyhow::ensure!(args.source_ips <= 254, "--source-ips 最多 254（127.0.0.x 的 x 段上限）");
    let hold_secs = args.hold_secs.min(45); // 服务端 60s 读空闲纪律
    let need_ips = u32::try_from(args.connections / 45_536 + 1).expect("连接数装得下 u32");
    if args.source_ips < need_ips {
        println!(
            "提示: {need_ips} 个源 IP 才够显式端口容量（每 IP 45,536 个；当前 {}）——不足时连接失败会计入报告",
            args.source_ips
        );
    }

    // ── 装配：真实服务端（随机端口）+ 内存基线 ──
    let config = SessionConfig {
        authenticator: Arc::new(StaticToken { token: "storm".to_string() }),
        ..SessionConfig::default()
    };
    let (addr, sessions, shutdown_tx) =
        im_server::spawn_server(config).await.context("启动被测服务端")?;
    let baseline = memstats::snapshot();
    println!("被测服务端    : {addr}（同进程回环）");
    if let Some(base) = baseline {
        println!("内存基线      : {}", memstats::fmt_mib(base.working_set));
    } else {
        println!("内存基线      : 本平台未实现采样（报告无内存列）");
    }

    // ── 风暴：波次进行，建连相与握手相分开计时 ──
    let t_storm = Instant::now();
    let (mut held, connect_ns, handshake_ns, failures) = run_storm(args, addr).await;

    // ── 守恒校验：在线数必须等于保活袋大小 ──
    let online = sessions.online_count();
    anyhow::ensure!(online == held.len(), "计数对不上：路由表 {online} vs 保活连接 {}", held.len());

    // ── 稳态：hold 秒内每秒采样内存与在线数（扫掠保活，理由见 run_storm）──
    let mut hold_samples = Vec::new();
    let mut last_sweep = Instant::now();
    for s in 0..hold_secs {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if last_sweep.elapsed() >= SWEEP_INTERVAL {
            keepalive_sweep(&mut held).await;
            last_sweep = Instant::now();
        }
        if let Some(snap) = memstats::snapshot() {
            hold_samples.push((s + 1, snap.working_set));
        }
    }

    // ── 拆除：drop 全部连接，路由表必须清零（回收不泄漏）──
    let t0 = Instant::now();
    let total_before = held.len();
    drop(held);
    let mut teardown = Duration::ZERO;
    let cleared = loop {
        if sessions.online_count() == 0 {
            teardown = t0.elapsed();
            break true;
        }
        if t0.elapsed() > Duration::from_secs(30) {
            break false;
        }
        tokio::task::yield_now().await;
    };

    report(
        args,
        total_before,
        connect_ns,
        handshake_ns,
        &failures,
        baseline,
        &hold_samples,
        t_storm.elapsed(),
        teardown,
        cleared,
    )?;
    shutdown_tx.trigger();
    Ok(())
}

/// 风暴主体：波次进行，建连相与握手相分开计时。
///
/// 建连相在 `spawn_blocking` 池里并行 connect（源 IP 轮换），握手相
/// 每连接一个短命 task——两相分开，报告里才能各自给出速率。
///
/// 返回（保活袋，建连相纳秒，握手相纳秒，失败分类）。
async fn run_storm(
    args: &ConnStormArgs,
    addr: SocketAddr,
) -> (Vec<Connection>, u128, u128, HashMap<String, usize>) {
    let mut held: Vec<Connection> = Vec::with_capacity(args.connections);
    let mut next_user_id: u64 = 1; // 一条连接一个身份，路由表里的行才有意义
    let mut connect_ns: u128 = 0;
    let mut handshake_ns: u128 = 0;
    let mut failures: HashMap<String, usize> = HashMap::new();
    let mut last_sweep = Instant::now();

    for chunk in (0..args.connections).collect::<Vec<_>>().chunks(args.wave) {
        // 建连相：阻塞池并行 connect（源 IP 轮换）
        let t0 = Instant::now();
        let mut connects = JoinSet::new();
        for &idx in chunk {
            let local = source_addr(idx, args.source_ips);
            let server = addr;
            connects.spawn_blocking(move || connect_bound(local, server));
        }
        let mut streams = Vec::with_capacity(chunk.len());
        while let Some(joined) = connects.join_next().await {
            match joined.expect("连接 task 不应 panic") {
                Ok(stream) => streams.push(stream),
                Err(e) => *failures.entry(format!("connect: {e}")).or_insert(0) += 1,
            }
        }
        connect_ns += t0.elapsed().as_nanos();

        // 握手相：每连接一个短命 task（发帧、等应答、退 task 时连接入保活袋）
        let t0 = Instant::now();
        let mut shakes = JoinSet::new();
        for stream in streams {
            let user_id = next_user_id;
            next_user_id += 1;
            let wait = Duration::from_millis(args.handshake_timeout_ms);
            shakes.spawn(async move {
                let mut conn = Connection::new(stream);
                let result = handshake(&mut conn, user_id, "storm", wait).await;
                (conn, result)
            });
        }
        while let Some(joined) = shakes.join_next().await {
            let (conn, result) = joined.expect("握手 task 不应 panic");
            match result {
                Ok(()) => held.push(conn),
                Err(e) => *failures.entry(format!("handshake: {e}")).or_insert(0) += 1,
            }
        }
        handshake_ns += t0.elapsed().as_nanos();

        // 保活扫掠：目标 10 万时建连相本身超过服务端 60s 读空闲纪律，
        // 每隔 SWEEP_INTERVAL 给全部保活连接写一帧 Ping 重置服务端计时器
        // （真实客户端的心跳在压测里的形状）。写失败静默忽略——
        // 已死的连接逃不过风暴后的守恒校验。
        if last_sweep.elapsed() >= SWEEP_INTERVAL {
            keepalive_sweep(&mut held).await;
            last_sweep = Instant::now();
        }
    }
    (held, connect_ns, handshake_ns, failures)
}

/// 保活扫掠：给全部保活连接写一帧 `Ping`。
///
/// 服务端“吞 Ping 回 Pong”，且心跳不参与 seq 去重（seq 0 即可）；
/// 回出的 Pong 落在无人读的接收缓冲里（每次扫掠几十字节，可忽略）。
async fn keepalive_sweep(held: &mut [Connection]) {
    let ping = Frame::new(Cmd::Ping, 0, 0, Bytes::new());
    for conn in held {
        let _ = conn.write_frame(&ping).await;
    }
}

/// 报告输出（含守恒与内存账本）。
///
/// # Errors
///
/// 拆除后路由表未清零时报错（连接回收泄漏是必须暴露的缺陷）。
#[allow(clippy::too_many_arguments)]
fn report(
    args: &ConnStormArgs,
    established: usize,
    connect_ns: u128,
    handshake_ns: u128,
    failures: &HashMap<String, usize>,
    baseline: Option<memstats::MemSnapshot>,
    hold_samples: &[(u64, u64)],
    storm_wall: Duration,
    teardown: Duration,
    cleared: bool,
) -> Result<()> {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    println!("──── im-bench · conn-storm ──────────────────────");
    println!(
        "目标            : {} 连接（波 {}，源 IP {}，逻辑核 {cores}）",
        args.connections, args.wave, args.source_ips
    );
    println!("建立            : {established} 连接，风暴墙钟 {}", fmt_secs(storm_wall));
    if established > 0 {
        let cps = u128::try_from(established).expect("连接数装得下 u128") * 1_000_000_000
            / connect_ns.max(1);
        let hps = u128::try_from(established).expect("连接数装得下 u128") * 1_000_000_000
            / handshake_ns.max(1);
        println!("建连相          : {}（{} 连接/秒）", fmt_ns(connect_ns), thousands(cps));
        println!("握手相          : {}（{} 连接/秒）", fmt_ns(handshake_ns), thousands(hps));
    }
    let fail_total: usize = failures.values().sum();
    if fail_total > 0 {
        println!("失败            : {fail_total} 次（按原因分类）：");
        for (reason, count) in failures {
            println!("  [{count:4}] {reason}");
        }
    } else {
        println!("失败            : 0");
    }

    // 内存账本：基线 → 峰值样本，每连接成本 = 差值 / 建立数
    if let (Some(base), Some(&(t, peak_ws))) = (baseline, hold_samples.last()) {
        let delta = peak_ws.saturating_sub(base.working_set);
        let per_conn = delta / established.max(1) as u64;
        println!(
            "内存账本        : 基线 {} → 稳态 {t}s {}（Δ {}）",
            memstats::fmt_mib(base.working_set),
            memstats::fmt_mib(peak_ws),
            memstats::fmt_mib(delta),
        );
        println!("每连接成本      : {}（含客户端半连接 + 服务端全套）", fmt_kib(per_conn));
    }

    println!("拆除            : {established} 连接，{}，路由表清零: {cleared}", fmt_secs(teardown));
    anyhow::ensure!(cleared, "拆除后路由表未清零——连接回收存在泄漏");
    println!("口径说明        : 同进程回环（每连接含双端成本）；数据归档 docs/16");
    Ok(())
}

/// 第 `idx` 条连接的源 IP：127.0.0.(1 + idx 轮换)。
///
/// 入参 `ips` 已在场景入口校验 ≤ 254——末段必然装得下 u8。
fn source_ip(idx: usize, ips: u32) -> IpAddr {
    let last = 1 + (u32::try_from(idx).expect("连接数装得下 u32") % ips);
    IpAddr::V4(Ipv4Addr::new(127, 0, 0, u8::try_from(last).expect("末段 ≤ 254（入口已校验）")))
}

/// 第 `idx` 条连接的本地地址：源 IP 轮换 + **显式源端口**。
///
/// Windows 的动态端口池是**全局共享**的（不按源地址分区，与 Linux
/// 相反）——源 IP 轮换换不来新端口，10 万目标三次停在 ~55,490
/// （≈池容量 55,536，os error 10055；判别实验与口径见 docs/16）。
/// 显式 `bind` 源端口不受动态池约束（池只管自动分配）：每个源 IP
/// 独享 20000..=65535 共 45,536 个。
fn source_addr(idx: usize, ips: u32) -> SocketAddr {
    let ip = source_ip(idx, ips);
    // 组内序号（同 IP 的第几条）决定端口；总容量 ips × 45,536，
    // 组内回绕前必然已撞入口的容量提示
    let per_ip = usize::try_from(ips).expect("入口已校验 ≥ 1");
    let nth = u32::try_from(idx / per_ip).expect("连接数装得下 u32");
    let port = 20_000 + nth % 45_536;
    SocketAddr::new(ip, u16::try_from(port).expect("20,000 + 余数 ≤ 65,535"))
}

/// 纳秒 → 人读时长（µs/ms/s 三段）。
fn fmt_ns(ns: u128) -> String {
    if ns < 1_000 {
        format!("{ns}ns")
    } else {
        fmt_secs(Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX)))
    }
}

/// 时长 → 人读格式。
fn fmt_secs(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1_000 {
        format!("{us}µs")
    } else if us < 1_000_000 {
        format!("{}.{:02}ms", us / 1_000, (us % 1_000) / 10)
    } else {
        format!("{}.{:02}s", us / 1_000_000, (us % 1_000_000) / 10_000)
    }
}

/// 千分位（与 main.rs 同款——报告工具的就近复制，见 histogram.rs 论证）。
fn thousands(n: u128) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// 字节数 → KiB（每连接成本的主单位，整数口径）。
fn fmt_kib(bytes: u64) -> String {
    format!("{}.{:02} KiB", bytes / 1024, (bytes % 1024) * 100 / 1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use im_protocol::{Cmd, Msg, MsgAck};
    use im_server::AllowAll;
    use tokio::time::timeout;

    /// 源 IP 轮换：idx 均匀散布在 127.0.0.1..=127.0.0.N。
    #[test]
    fn source_ip_round_robin() {
        assert_eq!(source_ip(0, 1), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(source_ip(0, 3), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(source_ip(4, 3), IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)));
        assert_eq!(source_ip(5, 3), IpAddr::V4(Ipv4Addr::new(127, 0, 0, 3)));
        assert_eq!(source_ip(6, 3), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    /// 显式源端口：同 IP 内端口互不重复、从 20000 起推进；跨 IP 可复用
    /// （四元组不同）；组内序号回绕不 panic。
    #[test]
    fn source_addr_assigns_explicit_ports() {
        let v4 = |a, b, c, d| IpAddr::V4(Ipv4Addr::new(a, b, c, d));
        assert_eq!(source_addr(0, 3), SocketAddr::new(v4(127, 0, 0, 1), 20_000));
        assert_eq!(source_addr(1, 3), SocketAddr::new(v4(127, 0, 0, 2), 20_000));
        assert_eq!(source_addr(2, 3), SocketAddr::new(v4(127, 0, 0, 3), 20_000));
        // 同 IP（idx 0 与 3 都是 .1）的端口必须推进：20,000 → 20,001
        assert_eq!(source_addr(3, 3), SocketAddr::new(v4(127, 0, 0, 1), 20_001));
        // 组内序号回绕（3 × 45,536 条之后）——仅验证不 panic
        let _ = source_addr(136_608, 3);
    }

    /// 绑定源 IP 的连接：走完整服务端握手 + 一条消息往返。
    /// （127.0.0.2 是回环——Windows/Linux 对 127/8 全段默认如此。）
    #[tokio::test]
    async fn bound_connect_handshakes_through_real_server() {
        let (addr, _sessions, shutdown) = im_server::spawn_server(SessionConfig {
            authenticator: Arc::new(AllowAll),
            ..SessionConfig::default()
        })
        .await
        .expect("服务应能启动");
        let stream = connect_bound(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0), addr)
            .expect("绑定回环连接");
        let mut conn = Connection::new(stream);
        handshake(&mut conn, 7, "any", Duration::from_secs(2))
            .await
            .expect("全放行服务端应接受握手");

        // 一条上行消息 → 服务端裁决并回执（完整会话路径的最小验证）
        let msg =
            Msg { from: 0, to: 99, msg_id: 0, client_msg_id: 1, content: Bytes::from_static(b"x") };
        conn.write_frame(&msg.encode_frame(2, 0)).await.expect("消息应能写出");
        let frame = timeout(Duration::from_secs(2), conn.read_frame())
            .await
            .expect("2s 内应收到应答")
            .expect("连接正常")
            .expect("连接未关闭");
        let ack = MsgAck::decode_frame(&frame).expect("应答应是 MsgAck");
        assert_eq!(ack.client_msg_id, 1);
        assert!(ack.msg_id > 0, "msg_id 由服务端雪花分配");
        assert_eq!(frame.cmd, Cmd::MsgAck);
        shutdown.trigger();
    }

    /// 握手被拒（错误 token + StaticToken）：错误串进入失败分类。
    #[tokio::test]
    async fn rejected_handshake_is_reported_as_failure() {
        let (addr, _sessions, shutdown) = im_server::spawn_server(SessionConfig {
            authenticator: Arc::new(StaticToken { token: "right".to_string() }),
            ..SessionConfig::default()
        })
        .await
        .expect("服务应能启动");
        let stream = connect_bound(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0), addr)
            .expect("连接应成功");
        let mut conn = Connection::new(stream);
        let err = handshake(&mut conn, 7, "wrong", Duration::from_secs(2))
            .await
            .expect_err("错误口令应被拒绝");
        assert!(err.contains("拒绝"), "错误串应说明原因: {err}");
        shutdown.trigger();
    }
}
