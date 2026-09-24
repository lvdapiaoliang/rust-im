# 04 - 任务管理与 Channel

## 本章目标

掌握 spawn 的完整语义（'static + Send 的原因）、JoinHandle、任务取消模式、
tokio 四种 channel 的选型——这些是写异步服务的日常肌肉记忆。

## 一、spawn 深入：为什么要求 'static + Send

```rust
tokio::spawn(async {
    // 这个闭包/async 块必须：
    // 1. 'static：不借用任何局部变量（任务可能活得比创建者久）
    // 2. Send：内容可跨线程移动（调度器可能把任务偷到别的 worker）
    do_work().await
});
```

**'static 的三种满足方式**：

```rust
let data = vec![1, 2, 3];

// ❌ 借用局部变量
// tokio::spawn(async { println!("{:?}", data); });   // E0597

// ✅ 方式一：move 所有权
tokio::spawn(async move { println!("{data:?}"); });

// ✅ 方式二：借用的数据本身就活得够久（如 &'static）
tokio::spawn(async { println!("{}", STATIC_CONFIG); });

// ✅ 方式三：Arc 共享
let shared = std::sync::Arc::new(data);
let s2 = shared.clone();
tokio::spawn(async move { println!("{s2:?}"); });
```

> 【Java】`new Thread(() -> use(localVar))` 里 JVM 自动捕获引用；
> Rust 要求你显式决定：拿走（move）、共享（Arc）、还是不跨任务（别 spawn）。
> **编译错误把「隐式共享可变状态」这个 Java 并发 bug 之源在编译期掐死**。

## 二、JoinHandle：等待与结果

```rust
let handle = tokio::spawn(async { 41 + 1 });

let result: i32 = handle.await.unwrap();   // 注意：await 的是 JoinHandle！
// join 失败（unwrap 出 Err）当且仅当任务 panic 了
// 【Java】Future.get() 会把异常重抛，JoinHandle.await 是显式的 Result

// 分离任务（detached）：不保存 handle，任务照跑（fire-and-forget）
tokio::spawn(async { write_log().await });
```

## 三、任务取消：Rust 没有内置 cancel，但有更好的东西

**取消的唯一途径：让任务自己结束**。三种模式：

### 3.1 Drop future（对未 spawn 的任务）

```rust
let fut = long_task();
fut.await;
// 如果不 await 直接 drop——状态机被销毁，一切随 Drop 释放
// （RAII 取消：连接断开 = 读任务自动结束，rust-im 的 echo 就是这个模式）
```

### 3.2 CancellationToken（tokio_util）

```rust
use tokio_util::sync::CancellationToken;

let token = CancellationToken::new();
let child = token.clone();

tokio::spawn(async move {
    tokio::select! {
        _ = child.cancelled() => println!("被取消，退出"),
        _ = do_work() => println!("正常完成"),
    }
});

token.cancel();   // 广播取消
```

### 3.3 channel 关闭（drop 所有发送端 → recv 返回 None）

```rust
let (tx, mut rx) = tokio::sync::mpsc::channel(16);
tokio::spawn(async move {
    while let Some(msg) = rx.recv().await {   // 所有 tx drop 后 recv 返回 None
        process(msg).await;
    }
    // 循环自然结束 = 任务取消
});
drop(tx);   // 关闭信道
```

> 【Java】Java 的 Thread.interrupt 是「戳一下，听不听随你」；
> Rust 的取消是协作式的、由数据流驱动（channel 关闭/token 广播），
> 任务有完全确定的取消点，不会在任意字节码处被打断。

## 四、tokio 的四种 Channel（选型是高频面试题）

| Channel | 多生产者 | 多消费者 | 适用 |
|---------|-----------|-----------|------|
| `mpsc` | ✅ | ❌（单 receiver） | **默认选择**：任务间流水线 |
| `oneshot` | ❌（1 次） | ❌（1 次） | 请求-响应、一次性通知 |
| `broadcast` | ✅ | ✅（每个消费者独立游标，**慢消费者丢消息**） | 事件广播（lacuna 处理） |
| `watch` | ✅（覆盖写） | ✅（只看最新值） | 配置/状态分发（历史无关） |

```rust
// mpsc：有界 = 内建背压！send 满了会 await（背压传导给生产者）
let (tx, mut rx) = tokio::sync::mpsc::channel::<Msg>(100);
// 【实战】rust-im 网关每连接一个 mpsc 发送端，容量 100：
// 下游处理不过来时，send().await 挂起 → 读循环停读 → TCP 缓冲区满 →
// 对端感知到背压——整条链路自动反压，没有一行额外代码

tx.send(msg).await?;        // 满了就等（优雅背压）
let msg = rx.recv().await;   // None = 所有发送端已关闭

// oneshot：异步世界的「回调转 await」
let (tx, rx) = tokio::sync::oneshot::channel();
tokio::spawn(async move { tx.send(compute()).ok(); });
let result = rx.await?;      // 单值等待

// watch：只关心最新值
let (tx, mut rx) = tokio::sync::watch::channel(Config::default());
tx.send_modify(|c| c.timeout_ms = 100);     // 覆盖
while rx.changed().await.is_ok() {
    println!("新配置：{:?}", rx.borrow());
}

// broadcast：注意 Lag 错误（消费者太慢，旧消息被回收）
let (tx, mut rx) = tokio::sync::broadcast::channel(16);
let _ = tx.send(event);                      // 忽略无订阅者的错误
match rx.recv().await {
    Ok(e) => ...,
    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
        // 慢了 n 条，自行决定：重放、忽略还是断线重连
    }
    Err(Closed) => break,
}
```

> 【Java】对标：mpsc ≈ SynchronousQueue/ArrayBlockingQueue（但有界 + 异步等待），
> broadcast ≈ Kafka 的消费组语义（各自游标、慢消费者丢数据），
> watch ≈ AtomicReference + 版本号。
> 【实战】rust-im 计划：每连接 mpsc（消息下发）、配置中心 watch、
> 事件总线 broadcast、SDK 请求响应用 oneshot——四种全用上。

### std::sync::mpsc 和 crossbeam 呢？

- `std::sync::mpsc`：同步阻塞 send/recv，**不能在 async 上下文用**（会阻塞 worker）
- `crossbeam-channel`：高性能同步 channel，同样不适合 async 路径
- 异步代码里发送少量数据给同步世界（如日志线程）可用；
  正经异步通信一律 `tokio::sync::*`

## 五、并发原语组合器

```rust
// join!：并发跑多个 future，全部完成（数量编译期已知）
let (a, b, c) = tokio::join!(fa(), fb(), fc());

// try_join!：同上，但任一 Err 立即返回（要求都返回 Result）

// FuturesUnordered：动态数量并发，完成一个收一个（【Java】CompletionService）
use futures::stream::{FuturesUnordered, StreamExt};
let mut futs = FuturesUnordered::new();
for id in 0..100 {
    futs.push(handle_request(id));
}
while let Some(result) = futs.next().await {
    println!("完成：{result:?}");
}

// BufferedStream：并发度上限的流式处理（限流并发！）
let results: Vec<_> = stream.iter(ids)
    .map(|id| handle(id))
    .buffer_unordered(32)      // 最多 32 个在飞
    .collect()
    .await;
```

> 【Java】join! ≈ CompletableFuture.allOf；buffer_unordered ≈ 信号量限流的并行流。
> 区别：Rust 这些是**零成本组合**（编译成一个状态机，无堆分配的 CompletableFuture 链）。

## 练习

1. 生产者-消费者：mpsc(16)，两个生产者 task 每秒各发 5 条，消费者 sleep 500ms 处理一条，
   观察背压（生产者的 send 被 await 卡住）。
2. 用 CancellationToken 实现优雅停机：主任务收到 ctrl+c 后 cancel，子任务打印「清理完成」再退出
   （提示：tokio::signal::ctrl_c()）。
3. 用 watch 实现动态配置：watch channel 发送日志级别，3 个消费者 task 只在变化时打印。

## 自测

1. spawn 的两个约束各自为什么存在？各列一种满足方式。
2. Rust 取消任务的三种模式？
3. 四种 channel 的语义差异？mpsc 有界 channel 为什么天然是背压？
4. join!/FuturesUnordered/buffer_unordered 分别对应 Java 的什么？

下一篇：[05-异步IO超时select.md](05-异步IO超时select.md)——网络编程三件套。
