//! # Echo server / client：Tokio 异步 TCP 编程的第一课
//!
//! 这是整个项目的第一段可运行网络代码。麻雀虽小，五脏俱全，
//! 它覆盖了后面 IM 服务端要用到的全部骨架概念：
//!
//! 1. **accept 循环 + 每连接一个 task** —— 阶段 3 网关接入层的原型
//! 2. **`TcpStream` 的所有权拆分（`split`）** —— 读端 / 写端分别被不同 task 持有，
//!    这是「所有权」在网络编程里最典型的应用（对照 Java：你从不需要关心谁能持有 socket）
//! 3. **固定大小缓冲区循环读** —— 阶段 1 处理「粘包 / 半包」的载体
//!
//! 阅读建议：先看本文件顶部的函数，再打开 `docs/03-async-tokio.md` 对照理解
//! `async fn` 到底编译成了什么、`Waker` 如何唤醒。
//!
//! 一个 Java 工程师最容易踩的思维差异：
//! Java 里一个 `Socket` 想被两个线程同时读写，靠的是「运行时不报错」；
//! Rust 里 `TcpStream` 想被两个 task 分别读写，必须用类型系统表达——
//! `split()` 把「逻辑上的双工」拆成「类型上的两个独立句柄」，
//! 编译器从根源上禁止你在读端上调用写方法。

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 服务端累计服务的连接数：展示 `Arc<AtomicU64>` 跨 task 共享计数
/// （`AtomicU64` 是 `Send + Sync` 的，所以能被多个 task 同时引用——见 docs/02）
static CONNECTIONS_SERVED: AtomicU64 = AtomicU64::new(0);

/// 每连接读缓冲区大小。
///
/// 为什么是 4 KB？TCP 是**字节流**，一次 `read` 返回的只是「内核缓冲区里当前有多少」，
/// 与对端 `write` 的次数无关（这就是「粘包 / 半包」的根源，阶段 1 用帧协议解决）。
/// 缓冲区大小影响的是单次系统调用的最大搬运量，IM 消息场景 4 KB 足够覆盖绝大多数帧。
const READ_BUF_SIZE: usize = 4096;

/// 启动 echo 服务端：accept 循环 + 每连接 spawn 一个独立 task。
///
/// # 关键点：task 泄漏吗？
/// 连接断开时 `serve_connection` 返回，task 自然结束并被 Tokio 回收。
/// 每个连接一个 task 是 IM 网关的标准模型——连接之间零锁竞争。
///
/// # 关键点：为什么 `listener` 上要无限循环而不用返回值？
/// 与 Java 不同，Rust 的错误处理是显式的（`io::Result`），
/// accept 失败时我们选择记录并继续——单次 accept 失败（如 EMFILE，文件描述符耗尽）
/// 不应杀死整个服务进程，这正是阶段 5 压测时要重点观察的内核参数场景。
pub async fn run_echo_server(listener: TcpListener) -> io::Result<()> {
    loop {
        // accept() 是异步的：没有新连接时当前 task 让出执行权（yield），
        // 不阻塞 worker 线程。对照 Java NIO：selector 事件的回调化封装。
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                // 单次 accept 失败只跳过这一次，不让服务崩溃
                eprintln!("[echo-server] accept 失败：{e}");
                continue;
            }
        };

        let served = CONNECTIONS_SERVED.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!("[echo-server] 接受连接 #{served} 来自 {peer}");

        // spawn 把连接的处理丢给 Tokio 调度器，accept 循环立即回到等待下一个连接。
        // 注意：spawn 要求 future 是 'static + Send —— 这是 Rust 异步最难的两道门槛，
        // docs/02 和 docs/03 会分别拆解为什么。
        tokio::spawn(serve_connection(stream));
    }
}

/// 处理单条连接：循环「读多少、回多少」，直到对端关闭。
///
/// 这是阶段 3 网关「每连接读循环」的原型：
/// 阶段 1 之后，这里的 `stream` 会被包一层 `Framed<_, Codec>`，
/// 循环体从「搬运字节」升级为「解码出一帧帧完整消息」。
pub async fn serve_connection(mut stream: TcpStream) -> io::Result<()> {
    // 固定大小栈上缓冲区，在循环外分配一次、反复复用。
    // 对照 Java：`new byte[4096]` 由 GC 回收；Rust 里它随函数栈帧分配/释放，
    // 零堆分配、零 GC 停顿——这是 IM 高并发场景选择 Rust 的核心理由之一。
    let mut buf = [0u8; READ_BUF_SIZE];

    loop {
        // read 返回 0 == 对端正常关闭（FIN），这是循环的唯一正常出口
        let n = match stream.read(&mut buf).await {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) => {
                eprintln!("[echo-server] 读取错误：{e}");
                return Err(e);
            }
        };

        // write_all 保证把刚读到的 n 个字节全部写回。
        // 注意参数是 &buf[..n]：借用（borrow）而非复制——
        // 我们把缓冲区的一个「切片视图」交给写操作，所有权仍在自己手里，
        // 下一轮循环继续用。Java 里没有对应概念，最接近的是 ByteBuffer 的 position/limit。
        stream.write_all(&buf[..n]).await?;
    }
}

/// 一个最小 echo 客户端：发送一段字节，读回相同内容，返回收到的数据。
///
/// 单独做成库函数而非只在测试里写，是因为阶段 5 的 im-bench
/// 会把它扩展成「连接风暴发生器」：几万个并发客户端 task 同时跑。
pub async fn run_echo_client(addr: &str, payload: &[u8]) -> io::Result<Vec<u8>> {
    // connect 返回的 TcpStream 拥有（own）这条连接的全部资源，
    // stream 离开作用域时（无论正常返回还是 Err 提前返回），Drop 自动关闭 socket。
    // 对照 Java：没有 try-with-resources，也不需要——析构即关闭，且编译器保证。
    let mut stream = TcpStream::connect(addr).await?;

    stream.write_all(payload).await?;

    let mut echoed = vec![0u8; payload.len()];
    stream.read_exact(&mut echoed).await?;

    Ok(echoed)
}

/// 启动一个绑定随机端口（127.0.0.1:0）的 echo 服务端，返回实际地址。
///
/// 专为测试设计：端口 0 由操作系统分配空闲端口，避免测试间端口冲突。
///
/// 小知识点：与 std 的 `TcpListener::try_clone` 不同，tokio 的 `TcpListener`
/// **没有**实现 `Clone`——它代表对内核 listening socket 的独占抽象。
/// 所以这里直接把 listener 的所有权 move 进后台 task（进程活着服务就一直跑），
/// 调用方只拿地址。这是 Rust 所有权的典型场景：资源归属清晰，无歧义。
pub async fn spawn_echo_server_on_random_port() -> io::Result<std::net::SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    // listener 被 move 进 task：spawn 要求 'static，move 语义正好满足
    tokio::spawn(run_echo_server(listener));
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端到端冒烟测试：起服务端 → 客户端发消息 → 收到相同回显。
    ///
    /// 这个测试本身就是「异步测试怎么写」的范本：
    /// #[tokio::test] 会创建一个单线程 tokio 运行时来跑这个 async 函数。
    #[tokio::test]
    async fn echo_roundtrip() {
        let addr = spawn_echo_server_on_random_port().await.unwrap();

        let payload = b"hello, rust-im!";
        let echoed = run_echo_client(&addr.to_string(), payload).await.unwrap();

        assert_eq!(&echoed, payload);
    }

    /// 边界测试：空 payload 也要正常工作（write_all(空) 是 no-op，read_exact(空) 立即返回）
    #[tokio::test]
    async fn echo_empty_payload() {
        let addr = spawn_echo_server_on_random_port().await.unwrap();
        let echoed = run_echo_client(&addr.to_string(), b"").await.unwrap();
        assert!(echoed.is_empty());
    }

    /// 大 payload 测试：超过单次 4 KB 读缓冲区（16 KB），
    /// 服务端必须经过多次「读-写」循环才能搬完，验证循环逻辑正确。
    #[tokio::test]
    async fn echo_large_payload_spans_multiple_reads() {
        let addr = spawn_echo_server_on_random_port().await.unwrap();

        let payload = vec![0xABu8; READ_BUF_SIZE * 4];
        let echoed = run_echo_client(&addr.to_string(), &payload).await.unwrap();

        assert_eq!(echoed.len(), payload.len());
        assert_eq!(echoed, payload);
    }

    /// 并发测试：50 个客户端 task 同时连接、同时收发，
    /// 验证「每连接一个 task」模型下连接之间互不干扰。
    /// 这就是阶段 5 连接风暴压测的雏形。
    #[tokio::test]
    async fn echo_many_concurrent_clients() {
        let addr = spawn_echo_server_on_random_port().await.unwrap();
        let addr = addr.to_string();

        let mut handles = Vec::new();
        for i in 0..50u32 {
            let addr = addr.clone();
            // 每个客户端发不同的内容，防止「碰巧都一样」掩盖串话（cross-talk）bug
            let payload = format!("client-{i}-message").into_bytes();
            handles.push(tokio::spawn(async move {
                let echoed = run_echo_client(&addr, &payload).await.unwrap();
                assert_eq!(echoed, payload);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }
}
