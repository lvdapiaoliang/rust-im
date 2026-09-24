# 03 - async/await 与 Tokio：百万任务的高效调度

> 本章目标：讲清楚 `.await` 背后发生了什么（状态机 + Waker）、
> Tokio 的多线程调度器如何工作、阻塞任务如何处理。
> 配合 `crates/im-transport/src/echo.rs` 对照阅读——那是本项目第一段真实异步代码。

## 一、Java 对照：三种并发模型的演进

| 模型 | Java 对应 | 每连接成本 | 问题 |
|------|-----------|-----------|------|
| 线程阻塞 | `new Thread` / 线程池 + 阻塞 IO | ~1MB 栈 | 1 万连接就吃掉 10GB 虚拟内存 |
| 回调/NIO | Netty / CompletableFuture | 小 | 「回调地狱」，控制流被打碎 |
| 协程/async | 虚拟线程（JDK 21+）/ Kotlin 协程 | ~KB 级 | 接近最优 |

Rust 的 async 属于第三类，但与虚拟线程有一个本质区别：

> **Java 虚拟线程：运行时调度，代码无感（同步写法，运行时挂起）。**
> **Rust async：编译期状态机 + 库调度器，零运行时魔法，但类型系统全面参与（`Future`、`Pin`、`Send`）。**

Rust 把协程机制做成了**语言语法（async/await）+ 标准库 trait（Future）+ 第三方运行时（Tokio）** 三层，
语言本身不带运行时——这就是「Rust 无 GC、无运行时」口号下 async 的真实形态。

## 二、Future：一个可能还没准备好的值

Future 的定义短得惊人：

```rust
pub trait Future {
    type Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;
}

pub enum Poll<T> {
    Ready(T),     // 完成，这是结果
    Pending,      // 没好，先别问我，好了我会叫你
}
```

两个关键点：

1. **poll 是被动的**：Future 自己不推进，靠外部反复调用 poll
2. **`self: Pin<&mut Self>`**：上一章讲过，async 状态机自引用，必须 Pin 住才能 poll

「好了我会叫你」的「叫」就是 **Waker**——异步编程的灵魂。

## 三、Waker：不要轮询我，我会通知你

### 3.1 没有感知的等待是灾难

想象 100 万连接 × 每秒 poll 一次 = 每秒 100 万次无效系统调用。
正确模型是事件驱动：**IO 就绪时，由事件源主动唤醒对应的任务**。

### 3.2 Waker 工作流程（以 echo server 的 `stream.read()` 为例）

```
task 调用 stream.read(&mut buf).await
        │
        ▼
poll 内部：向 epoll/kqueue/IOCP 注册「这个 fd 可读时，请用这个 Waker 叫我」
        │
        ▼
返回 Pending ──► Tokio 记录此任务「在等 fd」，任务休眠（不占线程！）
        │
        ▼  （几十毫秒后，网卡收到数据）
内核通知 epoll：fd 可读
        │
        ▼
Tokio 的驱动拿到注册的 Waker，调用 waker.wake()
        │
        ▼
任务被重新放回调度队列，某个 worker 线程再次 poll 它
        │
        ▼
这次 poll 返回 Ready(n)，read().await 完成，代码继续往下走
```

对照 Java：这就是 Netty 的 `channelRead` 回调 + `EventLoop`，
但 Rust 用 `.await` 把回调的「碎」缝合回了顺序代码。
**Waker ≈ Netty 里的 pipeline 兴趣集合 + `channel.eventLoop().execute(task)` 的合体**。

### 3.3 手写一个最小 Future（阶段 2 会实现真家伙，先看骨架）

```rust
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

/// 一个到点才完成的定时器：展示 Waker 如何被缓存与触发
pub struct Timer { deadline: Instant }

impl Future for Timer {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.deadline {
            Poll::Ready(())
        } else {
            // 还没到点：注册唤醒。真实实现会往定时器堆里塞 (deadline, waker.clone())
            // 到点后由定时器线程调用 waker.wake()。
            // 【关键】这里若不保存 waker，任务将永远无人唤醒 —— 最经典的 bug
            cx.waker().wake_by_ref(); // 演示用：立刻自唤醒（忙等，勿模仿）
            Poll::Pending
        }
    }
}
```

## 四、async fn 的真面目：编译成状态机

```rust
async fn send_heartbeat(stream: &TcpStream) -> io::Result<()> {
    let payload = build_ping();               // 状态 0 的代码
    stream.write_all(&payload).await;         // 挂起点 1 → 状态 1
    let mut buf = [0u8; 16];
    stream.read_exact(&mut buf).await;        // 挂起点 2 → 状态 2
    Ok(())
}
```

编译器（概念上）生成：

```rust
enum SendHeartbeat<'a> {
    Start(&'a TcpStream),
    WaitingWrite { payload: Vec<u8>, /* + 保存 write future */ },
    WaitingRead { buf: [u8; 16], /* + 保存 read future */ },
    Done,
}

impl Future for SendHeartbeat<'_> {
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        loop {
            match /* 当前状态 */ {
                // 执行到下一个挂起点，返回 Pending；或一路跑完返回 Ready
            }
        }
    }
}
```

**三大推论**（全部来自 docs/01、02 的知识汇合）：

1. 跨 `.await` 存活的局部变量成为状态机**字段**——所以 `&mut buf` 这种借用变成自引用，需要 Pin（02 章）
2. Future 只是「待执行的计划」，**没人 poll 它就永远不执行**——`async fn` 调用后必须 `.await` 或 spawn
3. 借用局部变量的 Future 不是 `'static`，所以 `tokio::spawn(async { ... &local ... })` 编译报错——spawn 要求是自己拥有全部数据的任务

## 五、Tokio 运行时：调度器全景

### 5.1 组件图

```
┌───────────────────────────────────────────────┐
│                Tokio Runtime                   │
│                                                │
│  ┌─────────────┐   任务队列（每 worker 一个     │
│  │ Worker × N  │   local 队列 + 全局队列）      │
│  │  ├─ run queue (local)                      │
│  │  ├─ 工作窃取 (work stealing) ◄──┐           │
│  ├─────────────┤                   │           │
│  │ Worker × 2  │ ── 窃取空闲──►─────┘           │
│  └─────────────┘                                │
│  ┌─────────────────────────────────────┐       │
│  │ Blocking Pool（独立线程池，默认 512）│       │
│  └─────────────────────────────────────┘       │
│  ┌─────────────────────────────────────┐       │
│  │ IO Driver（epoll / kqueue / IOCP）   │◄──── Waker 唤醒链路
│  └─────────────────────────────────────┘       │
│  ┌─────────────────────────────────────┐       │
│  │ Time Driver（定时器堆）              │       │
│  └─────────────────────────────────────┘       │
└───────────────────────────────────────────────┘
```

### 5.2 多线程调度器的核心机制（面试重点）

1. **N 个 worker 线程 = CPU 核数**（`#[tokio::main]` 默认）
2. 每个任务是一个被 `Box::pin` 的 future，被唤醒（wake）后进入**某个队列**：
   - `tokio::spawn` → 提交到当前 worker 的 local 队列（免全局锁）
   - **工作窃取**：空闲 worker 从别人 local 队列尾部偷任务，保负载均衡
3. LIFO 槽优化：刚被唤醒的任务优先插队执行，提高缓存亲和性
4. **一个任务从被 poll 到返回 Pending 之间不会被抢占**——协作式调度，单次 poll 时间片内独占线程

> 为什么百万连接可行：任务 ≠ 线程。一个被 Pending 的任务只是堆上几百字节的状态机，
> 线程只在「有任务可跑」时才工作。100 万连接 ≈ 100 万个堆上状态机 + 几十个线程。
> 对照 Java：这正是虚拟线程的思路，Rust 2018 年就有。

### 5.3 单线程 vs 多线程运行时

```rust
// 单线程（current_thread）：所有任务在一个线程上轮转
#[tokio::main(flavor = "current_thread")]

// 多线程（默认）：N 个 worker + 工作窃取
#[tokio::main]
```

| 场景 | 选择 | 原因 |
|------|------|------|
| 纯 IO 转发网关 | 多线程 | 用满多核 |
| 需要 `!Send` 的任务（如操作线程本地库） | 单线程 | spawn 不要求 Send |
| 嵌入 SDK 内的运行时（阶段 6） | 单线程/独立实例 | 避免和宿主 App 抢线程 |

### 5.4 阻塞任务处理（岗位 JD 原题）

**在 async 上下文里执行阻塞调用（同步 IO、重 CPU、锁等待）会卡死整个 worker**——
因为协作式调度不会抢占你，队列里所有任务跟着陪葬。

三种正确姿势：

```rust
// ① 专用 API：Tokio 自带的异步文件操作（内部就是丢给 blocking pool）
tokio::fs::read_to_string("data.txt").await?;

// ② 手动丢给阻塞池：同步重活（如 SQLite、加密压缩）
let content = tokio::task::spawn_blocking(move || {
    std::fs::read_to_string("data.txt")     // 同步阻塞，但在专用线程池里无害
}).await??;

// ③ CPU 密集且要并行：分片后 spawn 到多线程池
```

> 面试金句：**Tokio 的阻塞池是给「不可异步化的同步代码」的隔离区；
> 它保护的是调度器，不是性能。** 阶段 3 的 SQLite 持久化全部走 spawn_blocking。

### 5.5 io_uring（Linux 专属，阶段 5 M2 里程碑主角）

Tokio 主线基于 epoll/IOCP 的「就绪通知」模型仍有两次系统调用开销（注册 + 收取）。
`io_uring` 提供共享提交/完成队列，批量提交 IO、零系统调用收割结果，
是单机 100 万+ 连接的关键武器。Windows 上对应 IOCP 的深度利用与注册 IO（RIO）。
阶段 5 用 `tokio-uring` 做对照实验，数据进 docs/08。

## 六、本项目代码走读：echo.rs 的四个知识点现场

| 位置 | 现象 | 本章对应 |
|------|------|----------|
| `listener.accept().await` | 无连接时整个 accept 循环让出线程 | Waker 注册-唤醒链路 |
| `tokio::spawn(serve_connection(stream))` | spawn 要求 `'static + Send` | 01 章 move + 02 章 Send 的汇合点 |
| 每连接一个 task | 100 万连接 = 100 万小状态机，几十个线程 | 5.2 任务≠线程 |
| 测试里的 `#[tokio::test]` | 单线程运行时跑测试 | 5.3 运行时形态选择 |

## 七、动手练习

1. **观察 Future 是惰性的**：

   ```rust
   async fn side_effect() { println!("执行了！"); }
   #[tokio::main]
   async fn main() {
       let f = side_effect();      // 只创建 future，不执行
       println!("future 已创建");
       f.await;                    // 这里才打印「执行了！」
   }
   ```

2. **亲手调度一个 Future**：用 `std::pin::pin!` + `futures::poll!`（或手写循环）poll 一个 `Timer`，理解「poll 返回 Pending 时你的循环在忙等」。然后解释：Tokio 为什么不忙等？（Waker 注册到了 IO/Time driver）

3. **制造一次「阻塞毒丸」**：在 echo server 的 `serve_connection` 里插入 `std::thread::sleep(std::time::Duration::from_secs(5))`，并发 10 个客户端，观察其他连接全部卡死；换成 `tokio::time::sleep(...).await` 恢复。这就是 5.4 的第一手体验。

4. **阅读并运行示例**：`cargo run -p im-transport --example echo_demo`（本阶段附带的可运行示例：起 server + 3 个并发客户端），对照源码找出每个 `.await` 挂起点对应的状态机状态。

## 八、面试题与标准回答

**Q1：async/await 的原理？**

> async fn 被编译为实现了 Future 的状态机，每个 .await 是一个挂起点/状态。Future 本身惰性，靠外部 poll 推进；Pending 时通过 Waker 向调度器注册「就绪后唤醒我」。跨 await 存活的局部变量成为状态机字段，其中的借用构成自引用，因此 poll 的签名是 self: Pin<&mut Self>。Rust 语言只提供这套机制，调度器由运行时（Tokio）实现。

**Q2：讲讲 Tokio 的多线程调度器。**

> 默认 worker 数等于 CPU 核数，任务被 Box::pin 后进队列。spawn 优先入当前 worker 的 local 队列，避免全局锁；空闲 worker 通过工作窃取从其他 local 队列偷任务保证负载均衡；刚唤醒的任务走 LIFO 槽提升缓存亲和性。调度是协作式的：任务 poll 返回前不被抢占，所以在 async 上下文里执行阻塞调用会饿死同 worker 的所有任务——必须用 spawn_blocking 隔离。我在 IM 网关设计里，连接 IO 任务在多线程运行时上，SQLite 持久化走 spawn_blocking 阻塞池，压测时用 tokio-runtime 的 metrics 观测窃取率。

**Q3：阻塞任务在 Tokio 里怎么处理？**

> 三个层次：优先用异步版本 API（tokio::fs 等，内部已走阻塞池）；同步重活用 spawn_blocking 丢进独立阻塞池（默认 512 线程，可配），它的意义是隔离而非提速；CPU 密集型并行用 Rayon 或分片 spawn。判断标准只有一个：这段代码会不会让 poll 长时间不返回。我的项目里所有 SQLite 调用都在 spawn_blocking 里，实测网关 P99 从毫秒级抖动恢复稳定。

**Q4：为什么 tokio::spawn 要求 'static + Send？**

> spawn 出的任务生命周期由调度器掌控，可能远超创建它的函数，因此不能借用任何局部变量——必须 move 拥有全部数据（'static）；多线程调度器可能把任务迁移到任意 worker 线程执行，因此数据必须可安全跨线程转移（Send）。我踩过的实际坑：想 spawn 一个持有 &TcpStream 的任务，编译器报借用生命周期不足，解法是把 stream 拆分（split）后 move 所有权，或用 Arc 共享。

**Q5：Tokio 的 io-uring 支持是怎么回事？**

> Tokio 主线基于 epoll/IOCP 的就绪通知模型，每次 IO 至少两次系统调用（注册兴趣+收取事件）。io_uring 用内核/用户态共享的提交队列（SQ）与完成队列（CQ），支持批量提交、零拷贝收割，还提供异步文件、超时等统一接口。tokio-uring 是基于 io_uring 的独立运行时（单线程模型）。我在阶段 5 的计划是用它对网关做 10 万/100 万连接对照压测，量化每连接内存与 syscall 次数的差距——这是 M2 里程碑的核心实验。

## 下一章

概念弹药已备齐。接下来进入阶段 1：`im-protocol`——亲手设计二进制帧格式，
用 docs/01 的借用/切片知识解决 TCP 粘包拆包（配套文档 04）。
