//! im-bench：rust-im 压测工具。
//!
//! 阶段 7 的第一个真实场景 `group-fanout`：**群扇出热路径**。
//! 一条群消息 → 2 万成员，瓶颈全在「扇出引擎」本身——
//! [`im_server::web::fanout::GroupHub`] 的每群 actor +
//! [`im_server::session::Sessions::fanout_one`] 的 `try_send` 同步快路径。
//!
//! # 刻意排除的东西（测什么，先说清不测什么）
//!
//! - **网络**：不连 TCP/WS——每个成员是内存 channel 的 [`FrameSink`]
//!   （与真实路径的「有界 `mpsc` + `try_send`」同构，成本同量级）；
//! - **数据库**：成员表来自内存 [`MemberSource`]——扇出热路径本来
//!   就不查库（成员快照只装载一次），预热后 DB 完全不在路径上；
//! - **握手/鉴权/去重**：不走 `handle_msg`，直接 `hub.route`——
//!   上述环节由集成测试守住，本工具只量扇出引擎。
//!
//! # 测量口径
//!
//! - **单条扇出延迟**：逐条发、逐条等扇出完成（忙等轮询统计计数，
//!   微秒级粒度），采样 N 次给分位数；
//! - **持续扇出吞吐**：M 条消息背靠背灌入 actor 收件箱，全部扇出
//!   完成后算总账（含 actor 串行化本身——这是设计语义，不是开销）。
//!
//! 学习文档：`docs/13-group-fanout.md`（原始数据与结论都落在那里）。

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Bytes;
use clap::{Args, Parser, Subcommand};
use im_protocol::{Frame, Msg};
use im_server::web::fanout::{GroupHub, GroupStats, MemberList, MemberSource};
use im_server::web::groups::GroupError;
// route 是 GroupRouter 的 trait 方法：不在作用域里就调不到（trait 方法
// 的可见性规则——也是阶段 7 依赖注入的配套纪律：调用方必须“知道”契约）
use im_server::{FrameSink, GroupRouter, SendFuture, SessionConfig, Sessions, TrySendError};
use im_transport::TransportError;
use tokio::sync::mpsc;

mod connstorm;
mod histogram;
mod memstats;
mod weaklink;

use connstorm::ConnStormArgs;
use weaklink::WeakLinkArgs;

/// 压测群 ID：任意定值（避开成员 ID 段 `1..=members` 即可）。
const GROUP_ID: u64 = 900_000_000_000;

/// 发送者 ID：刻意**不是成员**——扇出对发送者的「跳过」（不回显）
/// 不会混进 delivered 口径，`fanned × 成员数` 恰好等于三路计数之和。
const SENDER_ID: u64 = 900_000_000_001;

/// 单次等待扇出完成的上限（防计数对不上时无限忙转）。
const BENCH_DEADLINE: Duration = Duration::from_secs(60);

// ────────────────────────────────────────────────────────────────
// 压测零件：成员 sink 与成员源
// ────────────────────────────────────────────────────────────────

/// 压测 sink：出站通道直出（真实 TCP 路径的 `ConnectionHandle` 同样
/// 是「有界 `mpsc` + `try_send`」，形状一致、成本同量级）。
#[derive(Debug)]
struct BenchSink(mpsc::Sender<Frame>);

impl FrameSink for BenchSink {
    fn send(&self, frame: Frame) -> SendFuture<'_> {
        Box::pin(async move { self.0.send(frame).await.map_err(|_| TransportError::Closed) })
    }

    fn try_send(&self, frame: Frame) -> Result<(), TrySendError> {
        self.0.try_send(frame).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => TrySendError::Full,
            mpsc::error::TrySendError::Closed(_) => TrySendError::Closed,
        })
    }
}

/// 内存成员源：固定成员表（首条消息装载后，热路径不再碰它——
/// 这正是 [`MemberSource`] 抽象的存在理由，见 fanout.rs 的论证）。
struct BenchSource {
    /// 全量成员 ID。
    members: Vec<u64>,
}

impl MemberSource for BenchSource {
    fn list_members(&self, _group_id: u64) -> MemberList<'_> {
        let members = self.members.clone();
        Box::pin(async move { Ok::<Vec<u64>, GroupError>(members) })
    }
}

// ────────────────────────────────────────────────────────────────
// 测量工具：进度快照 / 忙等 / 分位数 / 格式化
// ────────────────────────────────────────────────────────────────

/// 扇出进度快照（三路投递结果之和 + 接管数，口径见 [`GroupStats`]）。
#[derive(Clone, Copy)]
struct Progress {
    /// 进入扇出的消息数。
    fanned: u64,
    /// delivered + skipped + offline（每条消息对全成员恰好合计一次）。
    done: u64,
}

/// 全零基准（预热前的全新 hub 从零计数，绝对目标因此可用增量口径表达）。
const ZERO: Progress = Progress { fanned: 0, done: 0 };

/// 读一次进度快照。
fn progress(stats: &GroupStats) -> Progress {
    let done = stats.delivered.load(Ordering::Relaxed)
        + stats.skipped.load(Ordering::Relaxed)
        + stats.offline.load(Ordering::Relaxed);
    Progress { fanned: stats.fanned.load(Ordering::Relaxed), done }
}

/// 等 actor 消化完 `base` 之后追加的 `msgs` 条消息（每条 `per_msg` 个结果）。
///
/// 忙等（`yield_now` 轮询而非 `sleep`）：单条扇出是毫秒级测量对象，
/// `sleep(1ms)` 的粒度会污染它；`yield_now` 让出调度权，多 worker
/// 运行时里轮询 task 与扇出 actor 互不抢核。
///
/// # Errors
///
/// 超过 [`BENCH_DEADLINE`] 仍未达标（扇出卡死或计数对不上）时报错退出。
async fn wait_fanout(stats: &GroupStats, base: Progress, msgs: u64, per_msg: u64) -> Result<()> {
    let want_fanned = base.fanned + msgs;
    let want_done = base.done + msgs * per_msg;
    let deadline = Instant::now() + BENCH_DEADLINE;
    loop {
        let p = progress(stats);
        if p.fanned >= want_fanned && p.done >= want_done {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "扇出未在 60s 内完成（卡死或计数对不上）");
        tokio::task::yield_now().await;
    }
}

/// 最近邻分位数：升序样本第 `pct`% 处（p50 of 1..=100 → 50）。
fn percentile(sorted: &[u128], pct: u64) -> u128 {
    // 全程 u128 算术 + try_from 收口：测量代码不留裸 cast
    let n = u128::try_from(sorted.len()).expect("样本数装得下 u128");
    let rank = (n * u128::from(pct)).div_ceil(100).saturating_sub(1).min(n - 1);
    sorted[usize::try_from(rank).expect("分位索引装得下 usize")]
}

/// 微秒 → 人读时长：小于 1ms 用 µs，否则 ms 两位小数
/// （整数算术完成格式化——测量代码里不掺浮点）。
fn fmt_us(us: u128) -> String {
    if us < 1_000 {
        format!("{us}µs")
    } else {
        let whole = us / 1_000;
        let frac = (us % 1_000) / 10; // 两位小数（10µs 粒度对毫秒级对象足够）
        format!("{whole}.{frac:02}ms")
    }
}

/// 千分位格式化（报告可读性：7600000 → 7,600,000）。
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

// ────────────────────────────────────────────────────────────────
// 命令行
// ────────────────────────────────────────────────────────────────

/// 命令行入口（clap derive：子命令留给后续场景扩展）。
#[derive(Parser)]
#[command(name = "im-bench", about = "rust-im 压测工具（阶段 7 群扇出 / 阶段 10 三级里程碑）", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// 压测场景。
#[derive(Subcommand)]
enum Command {
    /// 群扇出：真实扇出 actor + `try_send` 慢消费者隔离（无 `DB`/网络开销）
    GroupFanout(GroupFanoutArgs),
    /// 连接风暴：真实 TCP 建连/握手速率 + 每连接内存账本（M1 里程碑）
    ConnStorm(ConnStormArgs),
    /// 弱网模拟：帧级丢包/延迟/乱序注入，真实客户端重传的到达率实测
    WeakLink(WeakLinkArgs),
}

/// `group-fanout` 场景参数。
#[derive(Debug, Args)]
struct GroupFanoutArgs {
    /// 群成员总数
    #[arg(long, default_value_t = 20_000)]
    members: usize,
    /// 持续扇出阶段的消息条数
    #[arg(long, default_value_t = 200)]
    messages: u64,
    /// 单条延迟的采样次数
    #[arg(long, default_value_t = 30)]
    samples: usize,
    /// 慢消费者个数（容量 1 且永不排空：验证隔离不拖慢全群）
    #[arg(long, default_value_t = 0)]
    slow: usize,
    /// 正常成员的出站通道容量
    #[arg(long, default_value_t = 64)]
    capacity: usize,
    /// 消息内容字节数
    #[arg(long, default_value_t = 64)]
    payload: usize,
}

// ────────────────────────────────────────────────────────────────
// 场景主体
// ────────────────────────────────────────────────────────────────

/// 装配压测世界：会话中心 + 全员注册 + 扇出中枢（内存成员源，不连库）。
///
/// 返回（中枢，慢消费者接收端保活袋，装配耗时）——保活袋里的接收端
/// 一旦 drop，sink 就会报 `Closed` 走「离线降级」，而慢消费者的语义
/// 是「活着但永不读」：保活袋必须等到测量结束才能 drop。
fn setup(args: &GroupFanoutArgs, members: u64) -> (GroupHub, Vec<mpsc::Receiver<Frame>>, Duration) {
    let t0 = Instant::now();
    let sessions = Sessions::new(SessionConfig::default());
    let member_ids: Vec<u64> = (1..=members).collect();

    let mut slow_rxs = Vec::with_capacity(args.slow);
    for (idx, &uid) in member_ids.iter().enumerate() {
        if idx < args.slow {
            // 慢消费者：容量 1、永不排空——首条送达后全部 Skipped
            let (tx, rx) = mpsc::channel(1);
            slow_rxs.push(rx);
            sessions.register(uid, idx as u64 + 1, Arc::new(BenchSink(tx))).expect("注册压测成员");
        } else {
            // 正常成员 = 持续排空的 task：真人「一直在读」的等价物
            let (tx, mut rx) = mpsc::channel(args.capacity);
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            sessions.register(uid, idx as u64 + 1, Arc::new(BenchSink(tx))).expect("注册压测成员");
        }
    }
    let hub =
        GroupHub::with_source(Arc::new(BenchSource { members: member_ids }), sessions.clone());
    (hub, slow_rxs, t0.elapsed())
}

/// `group-fanout` 场景：装配 → 预热 → 延迟采样 → 持续吞吐 → 报告。
async fn group_fanout(args: &GroupFanoutArgs) -> Result<()> {
    anyhow::ensure!(args.slow <= args.members, "--slow 不能超过 --members");
    anyhow::ensure!(args.capacity >= 1, "--capacity 至少为 1");
    anyhow::ensure!(args.payload >= 1, "--payload 至少为 1 字节");
    let members = args.members as u64;

    let (hub, slow_rxs, setup_elapsed) = setup(args, members);

    // 消息内容只生成一次：`Bytes` 克隆是引用计数，采样里不含内容拷贝
    let content = Bytes::from(vec![b'x'; args.payload]);
    let make_msg = |msg_id: u64| Msg {
        from: SENDER_ID,
        to: GROUP_ID,
        msg_id,
        client_msg_id: msg_id,
        content: content.clone(),
    };
    let mut next_msg_id = 0u64;

    // ── 预热：首条走慢路径（孵化 actor + 装载快照），不计入任何统计 ──
    anyhow::ensure!(
        hub.route(GROUP_ID, &make_msg(next_msg_id)).await,
        "扇出中枢未接管（成员源为空？）"
    );
    next_msg_id += 1;
    let stats = hub.stats(GROUP_ID).expect("预热后 actor 必然已孵化");
    wait_fanout(&stats, ZERO, 1, members).await?;

    // ── 单条扇出延迟：逐条发、逐条等完成（N 次采样给分位数）──
    let mut latencies_us: Vec<u128> = Vec::with_capacity(args.samples);
    for _ in 0..args.samples {
        let base = progress(&stats);
        let t0 = Instant::now();
        anyhow::ensure!(hub.route(GROUP_ID, &make_msg(next_msg_id)).await, "热路径 route 不应失败");
        next_msg_id += 1;
        wait_fanout(&stats, base, 1, members).await?;
        latencies_us.push(t0.elapsed().as_micros());
    }
    latencies_us.sort_unstable();

    // ── 持续扇出：背靠背灌入（route 只入收件箱即返回；收件箱满才反压——
    //    这正是设计语义：actor 卡在快照重载时才让发送者等）──
    let mut burst = None;
    if args.messages > 0 {
        let base = progress(&stats);
        let t0 = Instant::now();
        for _ in 0..args.messages {
            anyhow::ensure!(
                hub.route(GROUP_ID, &make_msg(next_msg_id)).await,
                "热路径 route 不应失败"
            );
            next_msg_id += 1;
        }
        wait_fanout(&stats, base, args.messages, members).await?;
        burst = Some((args.messages, t0.elapsed()));
    }

    report(args, members, setup_elapsed, &latencies_us, burst, &stats)?;

    // 慢消费者通道到此才允许关闭（之后 sink 报 Closed 也无所谓——测量已结束）
    drop(slow_rxs);
    Ok(())
}

/// 输出报告（含计数对账——对不上即扇出路径有 bug，压测结果作废）。
///
/// # Errors
///
/// `fanned × 成员数 ≠ delivered+skipped+offline` 时报错
/// （发送者非成员的干净口径下两者必须相等）。
fn report(
    args: &GroupFanoutArgs,
    members: u64,
    setup_elapsed: Duration,
    latencies_us: &[u128],
    burst: Option<(u64, Duration)>,
    stats: &GroupStats,
) -> Result<()> {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    println!("──── im-bench · group-fanout ────────────────────");
    println!(
        "成员            : {}（慢消费者 {} 个：容量 1、永不排空）",
        thousands(u128::from(members)),
        thousands(args.slow as u128)
    );
    println!("正常成员通道    : 容量 {} + 持续排空 task", args.capacity);
    println!("消息载荷        : {} 字节", args.payload);
    println!("环境            : 逻辑核 {cores}");
    println!(
        "装配            : {} 会话 + 排空 task，耗时 {}",
        thousands(u128::from(members)),
        fmt_us(setup_elapsed.as_micros())
    );

    if !latencies_us.is_empty() {
        println!("单条扇出延迟（{} 次逐条采样，含 route 入队 + actor 全程）：", latencies_us.len());
        println!(
            "  min {}   P50 {}   P90 {}   P99 {}   max {}",
            fmt_us(latencies_us[0]),
            fmt_us(percentile(latencies_us, 50)),
            fmt_us(percentile(latencies_us, 90)),
            fmt_us(percentile(latencies_us, 99)),
            fmt_us(latencies_us[latencies_us.len() - 1]),
        );
    }
    if let Some((messages, elapsed)) = burst {
        // 整数算速率（测量代码不掺浮点）：先化成 µs 再换算成秒
        let elapsed_us = elapsed.as_micros().max(1);
        let msgs_per_sec = u128::from(messages) * 1_000_000 / elapsed_us;
        let deliveries_per_sec = msgs_per_sec * u128::from(members);
        println!("持续扇出（{messages} 条背靠背灌入）：");
        println!("  消息吞吐      : {} 条/秒", thousands(msgs_per_sec));
        println!("  成员投递      : {} 人次/秒", thousands(deliveries_per_sec));
    }

    let p = progress(stats);
    let expected = u128::from(p.fanned) * u128::from(members);
    anyhow::ensure!(
        expected == u128::from(p.done),
        "计数对不上：fanned×成员数={expected}，delivered+skipped+offline={}",
        p.done
    );
    println!(
        "计数对账        : fanned={} delivered={} skipped={} offline={}",
        thousands(u128::from(p.fanned)),
        thousands(u128::from(stats.delivered.load(Ordering::Relaxed))),
        thousands(u128::from(stats.skipped.load(Ordering::Relaxed))),
        thousands(u128::from(stats.offline.load(Ordering::Relaxed))),
    );
    if args.slow > 0 {
        println!(
            "  慢消费者隔离  : skipped={}（首条送达后全部跳过，其余成员不受影响）",
            thousands(u128::from(stats.skipped.load(Ordering::Relaxed)))
        );
    }
    println!("口径说明        : 不含网络/DB/握手（内存 channel sink + 内存成员源）");
    println!("                  原始数据归档于 docs/13-group-fanout.md");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::GroupFanout(args) => group_fanout(&args).await,
        Command::ConnStorm(args) => connstorm::conn_storm(&args).await,
        Command::WeakLink(args) => weaklink::weak_link(&args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use im_protocol::Payload;
    use std::time::Duration;

    /// 千分位格式化：每三位一组，恰好在首位后不出现多余逗号。
    #[test]
    fn thousands_groups_every_three_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(6_840_000), "6,840,000");
    }

    /// 分位数（最近邻法）：升序样本第 pct% 处，小样本自动夹紧到边界。
    #[test]
    fn percentile_picks_nearest_rank() {
        let samples: Vec<u128> = (1..=100).collect();
        assert_eq!(percentile(&samples, 1), 1);
        assert_eq!(percentile(&samples, 50), 50);
        assert_eq!(percentile(&samples, 99), 99);
        assert_eq!(percentile(&samples, 100), 100);
    }

    #[test]
    fn percentile_small_sample_clamps() {
        let samples: Vec<u128> = vec![5, 10];
        assert_eq!(percentile(&samples, 50), 5);
        assert_eq!(percentile(&samples, 99), 10);
    }

    /// 压测装配的最小端到端：内存成员源 + 真实扇出中枢（无 DB、无网络）。
    /// fanout.rs 有同款单元测试，这里守住的是 **im-bench 自己的装配**
    /// （`BenchSink` 的 `try_send` 通路 + `BenchSource` 的装箱 future）。
    #[tokio::test]
    async fn bench_source_routes_through_real_hub() {
        let sessions = Sessions::new(SessionConfig::default());
        let hub =
            GroupHub::with_source(Arc::new(BenchSource { members: vec![1, 2] }), sessions.clone());

        // 两个在线成员（接收端保活：drop 会变成 Closed → 离线路径）
        let (tx1, _rx1) = mpsc::channel(4);
        let (tx2, mut rx2) = mpsc::channel(4);
        sessions.register(1, 1, Arc::new(BenchSink(tx1))).expect("注册不应冲突");
        sessions.register(2, 2, Arc::new(BenchSink(tx2))).expect("注册不应冲突");

        let msg = Msg {
            from: SENDER_ID,
            to: GROUP_ID,
            msg_id: 1,
            client_msg_id: 1,
            content: Bytes::from_static(b"bench"),
        };
        assert!(hub.route(GROUP_ID, &msg).await, "内存源非空，应被扇出接管");

        let stats = hub.stats(GROUP_ID).expect("actor 应已孵化");
        wait_fanout(&stats, ZERO, 1, 2).await.expect("扇出应完成");
        assert_eq!(stats.delivered.load(Ordering::Relaxed), 2, "两名成员都应在线送达");

        let frame = tokio::time::timeout(Duration::from_secs(2), rx2.recv())
            .await
            .expect("2s 内应收到扇出帧")
            .expect("sink 存活");
        let decoded = Msg::decode_frame(&frame).expect("载荷应与命令字匹配");
        assert_eq!(decoded.to, GROUP_ID, "载荷 to 保持群 ID");
        assert_eq!(decoded.from, SENDER_ID);
    }
}
