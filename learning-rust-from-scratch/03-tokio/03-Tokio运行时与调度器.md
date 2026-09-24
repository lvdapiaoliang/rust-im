# 03 - Tokio 运行时与调度器

> 面试核心篇。岗位 JD 原文：「Tokio 多线程调度器、spawn、阻塞任务处理、io_uring」。

## 本章目标

掌握 Tokio 运行时的组件构成、多线程调度器的工作窃取机制、
运行时的配置选择，能回答「Tokio 调度是怎么工作的」这类面试题。

## 一、运行时组件总览

```
┌─────────────────────────────────────────────────┐
│                 Tokio Runtime                    │
│                                                  │
│   ┌─────────────────────────────────────────┐   │
│   │  Scheduler（多线程版）                   │   │
│   │  Worker-0  Worker-1  ... Worker-(N-1)    │   │
│   │  [local 队列] [local 队列]  [local 队列]  │   │
│   │      ↕ 工作窃取（work stealing）          │   │
│   │  [全局队列（inject queue，overflow 用）]   │   │
│   │  [LIFO 槽（刚唤醒任务优先插队）]           │   │
│   └─────────────────────────────────────────┘   │
│                                                  │
│   IO Driver：epoll(Linux) / IOCP(Win) / kqueue   │
│   Time Driver：时间轮定时器堆                    │
│   Blocking Pool：独立线程池（默认 512 线程）      │
└─────────────────────────────────────────────────┘
```

## 二、多线程调度器机制（逐条拆解）

### 2.1 任务的一生

```rust
tokio::spawn(my_task());     // ① 创建：状态机 Box::pin 进堆，几百字节
                              // ② 入队：优先放当前 worker 的 local 队列
                              // ③ 某个 worker pop 出它 → poll
                              // ④ Pending？挂起，线程去取下一个任务
                              // ⑤ 事件源 wake → 重新入队（LIFO 槽）
                              // ⑥ Ready？任务完成，内存释放
```

### 2.2 为什么 local 队列优先（减少锁竞争）

任务入全局队列要抢全局锁；放自己的 local 队列是无锁的（单生产者）。
**窃取是 bulk 操作**：空闲 worker 一次偷一半任务，均摊了竞争成本。

### 2.3 LIFO 槽（缓存亲和性优化）

```
Worker 正在跑任务 A → A wake 了任务 B（比如处理同一连接的下一个包）
→ B 进入 LIFO 槽 → A 主动让出（yield）后，B 是下一个被执行的
→ A、B 大概率操作同一批数据，CPU 缓存还热着
```

> 【Java】Netty 的 EventLoop 是「连接绑线程」模型（一个连接永远同一个线程处理）；
> Tokio 是「任务自由迁移」+ 用 LIFO 槽**模拟**局部性。两种取舍：
> Netty 避免了同步但负载可能不均；Tokio 灵活但需要任务内部自己保证数据安全（Send）。

### 2.4 公平性：防饿死

- 每个 worker 每处理 61 个任务，强制从全局队列取一个（防止全局队列任务饿死）
- 任务不能无限霸占线程：一次 poll 里不做重活是**社区契约**
  （违反不报错，但拖垮同 worker 所有任务——见第 7 篇）

## 三、运行时构建与配置

```rust
use tokio::runtime;

// ① 最常用：默认多线程
let rt = runtime::Runtime::new().unwrap();
rt.block_on(async_main());

// ② 手动构建（生产服务标准姿势，参数显式化）
let rt = runtime::Builder::new_multi_thread()
    .worker_threads(8)                // worker 数（默认 = CPU 核数）
    .thread_name("im-worker")         // 线程名（崩溃日志可读性）
    .enable_all()                     // 开 IO + Time driver（必开！）
    .build()?;

// ③ 单线程运行时（current_thread）
let rt = runtime::Builder::new_current_thread()
    .enable_all()
    .build()?;

// ④ 临时进入运行时上下文（在已有 runtime 里跑 block_on 会 panic 的场景）
rt.handle().block_on(...);            // Handle 可 clone，可跨线程传递
```

### 单线程 vs 多线程选择

| | 多线程（默认） | current_thread |
|---|---|---|
| spawn 要求 | `Send + 'static` | 只要 `'static`（可以 `!Send`！） |
| CPU 利用 | 多核并行 | 单核 |
| 适用 | 服务端、并行计算 | 测试、嵌入 SDK、配合 block_in_place |

> 【实战】rust-im 的 `#[tokio::test]` 默认就是 current_thread——
> 测试里可以用线程不安全的类型；SDK（im-sdk）内部也可能用独立单线程运行时
> 隔离宿主 App 的线程。

### #[tokio::main] 的参数形态

```rust
#[tokio::main(flavor = "current_thread")]
async fn main() { }

#[tokio::main(worker_threads = 4)]
async fn main() { }
```

## 四、阻塞任务的隔离（面试必考：三档处理）

```rust
// ① 标准库的异步版本（内部自动走阻塞池）——优先选
let content = tokio::fs::read_to_string("big.txt").await?;

// ② 同步重活手动进阻塞池
let hashed = tokio::task::spawn_blocking(move || {
    expensive_hash(&data)             // 同步 CPU 密集代码
}).await?;

// ③ 多线程运行时里「就地变阻塞」：当前 worker 临时把自己变阻塞线程，
//    调度器再补一个 worker（高级用法，慎用）
tokio::task::block_in_place(|| {
    heavy_sync_operation()
})?;
```

> **核心认知：spawn_blocking 不是性能优化，是隔离**。
> 协作式调度下，一次 50ms 的同步调用 = 同 worker 上所有任务延迟 50ms。
> 阻塞池（默认 512 线程，可配 max_blocking_threads）就是隔离区。
> 【实战】rust-im 的 SQLite 落库全部走 spawn_blocking。

## 五、IO driver 与 io_uring（岗位 JD 点名）

### 当前主线：就绪通知模型

```
epoll（Linux）/ IOCP（Windows）/ kqueue（macOS）
注册 fd + Waker → 内核通知「可读/可写」→ driver 调 Waker → 任务重新入队
每次 IO 往返 = 注册（1 syscall）+ 事件收取（1 syscall）+ 真正 read/write
```

### io_uring：异步提交模型

```
用户态与内核共享：提交队列 SQ + 完成队列 CQ
批量提交多个 IO 请求（一次 syscall 提交一批）→ 完成时结果出现在 CQ（零 syscall 收割）
还能统一异步化：文件、超时、连接建立……
Linux 5.1+ 可用；tokio-uring 是基于它的独立运行时（单线程设计）
```

| | epoll + tokio | io_uring + tokio-uring |
|---|---|---|
| syscall 次数 | 每 IO 2+ 次 | 批量摊薄，可到接近 0 |
| 文件 IO | 不支持（必须走线程池） | 原生异步 |
| 成熟度 | 极成熟 | 演进中（内核版本要求） |
| Windows | 换 IOCP | 不可用 |

> 【实战】rust-im 阶段 5 的 M2 里程碑（100 万连接）计划用 tokio-uring 做对照压测，
> 量化 syscall 次数与每连接内存差异 → docs/08。

## 六、观测运行时

```rust
// 运行时指标（启用 unstable feature 后）
let rt = runtime::Builder::new_multi_thread()
    .enable_metrics()          // 需要 RUSTFLAGS="--cfg tokio_unstable"
    .build()?;

// 关注：worker 数、各 local 队列长度、steal 次数、blocking pool 活跃数
// 窃取率高 = 任务分布不均（考虑批量 spawn 或调整 worker 数）
```

## 练习

1. 写一个程序：多线程运行时 spawn 1000 个任务，每个 sleep 随机时长后打印，
   观察完成顺序（乱序！不是提交顺序）。
2. 制造阻塞毒丸：spawn 一个 `std::thread::sleep(3s)` 的任务，
   同时 spawn 100 个即时任务，观察它们被卡住；换 spawn_blocking 恢复。
3. 用 `Builder::new_current_thread()` 跑同样程序，观察行为差异（需要主动 yield 才能并发）。

## 自测

1. Tokio 多线程调度器的任务从 spawn 到完成经历什么？队列结构是什么样的？
2. LIFO 槽解决什么问题？和 Netty 的连接绑线程模型的取舍？
3. spawn_blocking 为什么存在？阻塞池的默认大小？
4. epoll 模型和 io_uring 模型的 syscall 成本差异？

下一篇：[04-任务管理与Channel.md](04-任务管理与Channel.md)——spawn/JoinHandle/取消/channel 选型。
