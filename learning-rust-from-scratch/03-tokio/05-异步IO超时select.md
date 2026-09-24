# 05 - 异步 IO、超时与 select

## 本章目标

掌握 TcpListener/TcpStream 的异步用法、读写拆分、超时包装、
select! 多路等待——网络服务的核心工具箱。

## 一、异步 TCP 编程

```rust
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("0.0.0.0:8080").await?;

    loop {
        let (stream, peer) = listener.accept().await?;
        println!("新连接：{peer}");
        tokio::spawn(handle(stream));       // 每连接一个 task
    }
}

async fn handle(mut stream: TcpStream) -> std::io::Result<()> {
    let mut buf = vec![0u8; 4096];
    loop {
        let n = stream.read(&mut buf).await?;    // 未就绪时挂起，不占线程
        if n == 0 { return Ok(()); }              // 对端关闭
        stream.write_all(&buf[..n]).await?;
    }
}
```

> 【Java】accept/read/write 的形态与 Netty 差异巨大：
> Netty 是「事件回调」（channelRead），Tokio 是「阻塞式写法的异步版」——
> 代码看起来像 BIO，实际永不阻塞线程。这是 async/await 最大的工程价值。

### 读写拆分与合并

```rust
// split：读端写端分给两个 task（所有权拆分！）
let (mut rd, mut wr) = stream.into_split();
let writer_task = tokio::spawn(async move {
    while let Some(msg) = rx.recv().await {
        wr.write_all(&msg).await?;
    }
});
// rd 留在当前 task 继续读
// 【Java】不需要像 Netty 那样关心 handler 的线程模型——所有权即线程模型

// 也可以手动 split（borrow 版，同一 task 里交替读写用）
let (mut rd, mut wr) = (&stream, &stream);
// tokio 的 TcpStream 本身支持 &mut 并发（内部 Arc），into_split 是 owned 版
```

### 其他网络类型

```rust
tokio::net::UdpSocket      // UDP
tokio::net::UnixStream     // Unix 域套接字（本机 IPC，比 TCP 快）
tokio::net::lookup_host    // 异步 DNS
```

## 二、超时：一切等待都要有期限

```rust
use tokio::time::{timeout, Duration};

// timeout 包装任意 future
match timeout(Duration::from_secs(3), stream.read(&mut buf)).await {
    Ok(Ok(n)) => { /* 读到了 n 字节 */ }
    Ok(Err(e)) => { /* IO 错误 */ }
    Err(_elapsed) => { /* 超时：内层 future 被 drop（取消清理！） */ }
}

// 【实战】rust-im 心跳检测：
// 超时时间内没收到任何包 → 判定半开连接 → 主动断开重连
```

> 注意：超时后内层 future 被 **drop**，其状态机里的资源随 Drop 释放——
> 这就是 RAII 式取消，不需要 try-with-resources。

## 三、select!：多路复用的等待

```rust
use tokio::select;

tokio::select! {
    msg = rx.recv() => {
        // channel 来消息了
        match msg { Some(m) => process(m), None => break }
    }
    _ = tokio::time::sleep(Duration::from_secs(30)) => {
        // 30 秒没消息 → 发心跳
    }
}
```

语义要点：

1. **随机公平**：多个分支同时就绪时**随机**选（避免饥饿）——和 Java 的 `CompletableFuture.anyOf` 不同
2. **未选中的分支被取消（drop）**——这是 select 最容易踩坑的地方：

```rust
// ❌ 坑：每次循环 select，buf 的读被取消丢掉，永远读不完
loop {
    let mut buf = [0u8; 1024];
    tokio::select! {
        n = stream.read(&mut buf) => { /* 半包数据随取消丢弃！ */ }
        _ = timeout_fut => break,
    }
}

// ✅ 修法一：读操作提到循环外，用 &mut future 复用进度
let read_fut = stream.read(&mut buf);
tokio::pin!(read_fut);
loop {
    tokio::select! {
        n = &mut read_fut => { ... }     // 复用！被取消不丢进度
        _ = ... => ...,
    }
}

// ✅ 修法二：分支里再写循环 / 用 cancellation-safe 的 API
```

**Cancellation safety**（取消安全性）：select 分支里的 future 被取消后，
已完成的副作用是否丢失？判断标准：

| API | 取消安全 | 说明 |
|-----|----------|------|
| `rx.recv()` | ✅ | 没收到就没收到 |
| `stream.read()` | ❌ | 读了一半会丢 |
| `stream.write_all()` | ❌ | 写一半取消，已写部分不可回滚 |
| `sleep` | ✅ | 无副作用 |
| `rx.recv_many / read_buf` | ⚠️ 视情况 | 看数据留在哪 |

> 这是 Tokio 面试的「段位题」。答出「select 会 drop 未选中分支 + 取消安全性」
> 就超过 80% 的候选人了。
> 【实战】rust-im 阶段 2 的读循环 + 心跳 select 必须用 `pin!` + `&mut` 处理半包问题。

### select 其他形态

```rust
// 带完整分支（处理 + 兜底）
tokio::select! {
    biased;                     // 改为顺序轮询（放弃随机公平，用于优先级）
    _ = ctrl_c() => { /* 优先检查退出信号 */ }
    msg = rx.recv() => { ... }
}

// default：无就绪分支时立即走（非阻塞轮询）
tokio::select! {
    msg = rx.try_recv_result() => { ... }
    default => { /* 这轮没消息 */ }
}
```

## 四、经典模式：读循环 + 心跳 + 优雅退出

```rust
/// rust-im 传输层连接循环的骨架（阶段 2 完整版的前身）
async fn connection_loop(
    mut stream: TcpStream,
    mut shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut read_fut = stream.read_buf(&mut buf);   // BytesMut 版本更好
    tokio::pin!(read_fut);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                stream.shutdown().await.ok();        // 优雅关闭
                return Ok(());
            }
            _ = tokio::time::sleep(HEARTBEAT_INTERVAL) => {
                send_ping(&mut stream).await?;       // 心跳
            }
            res = &mut read_fut => {
                let n = res?;
                if n == 0 { return Ok(()); }         // 对端关闭
                handle_frame(&mut buf)?;             // 解析帧
                read_fut.set(stream.read_buf(&mut buf)); // 重建 future
            }
        }
    }
}
```

五个知识点在一页代码里：select、pin!、取消令牌、半包处理、优雅关闭。

## 练习

1. 写一个带 3 秒读超时的 echo server：客户端连上不发包 → 3 秒后被踢。
2. 用 select! 实现聊天客户端：stdin 输入发送 + socket 接收打印
   （提示：stdin 异步用 `tokio::io::stdin()`）。
3. 复现「半包丢失坑」：在 select 分支里直接 `stream.read(&mut buf)`，
   配合另一个频繁就绪的分支，观察数据丢失；再用 pin! + &mut 修复。

## 自测

1. timeout 超时后内层 future 发生了什么？资源如何清理？
2. select! 的公平策略？未选中分支的命运？
3. 什么是取消安全性？read 为什么不安全，怎么修？

下一篇：[06-同步原语与共享状态.md](06-同步原语与共享状态.md)
