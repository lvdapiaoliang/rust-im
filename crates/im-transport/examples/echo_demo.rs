//! # echo_demo：阶段 0 可运行示例
//!
//! 运行：`cargo run -p im-transport --example echo_demo`
//!
//! 演示内容（配合 docs/03-async-tokio.md 第七节练习 4）：
//! 1. 启动 echo 服务端（随机端口）
//! 2. 并发启动 3 个客户端 task，各发一条不同消息
//! 3. 等待全部返回，验证互不串话
//!
//! 观察点：每个 `.await` 就是一个挂起点（状态机状态切换）；
//! 3 个客户端并发执行但共用同一个运行时的几个 worker 线程。

use std::time::Duration;

use im_transport::{run_echo_client, spawn_echo_server_on_random_port};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 启动服务端：listener 的所有权 move 进后台 task（docs/01 第五节知识点）
    let addr = spawn_echo_server_on_random_port().await?;
    println!("[demo] echo 服务端已启动：{addr}");

    // 3 个客户端并发：每个 client task 拥有自己的 payload（move 语义）
    let clients = ["alice: 你好", "bob: hello", "carol: こんにちは"];

    let mut handles = Vec::new();
    for msg in clients {
        let addr = addr.to_string();
        handles.push(tokio::spawn(async move {
            // 模拟错峰连接，让输出更易读
            tokio::time::sleep(Duration::from_millis(100)).await;
            let payload = msg.as_bytes().to_vec();
            let echoed = run_echo_client(&addr, &payload).await?;
            let text = String::from_utf8(echoed).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("[demo] 收到回显：{text}");
            Ok::<(), anyhow::Error>(())
        }));
    }

    // join 所有 task：`await` 返回的 `JoinHandle` 本身也是一个 Future
    for h in handles {
        h.await??;
    }

    println!("[demo] 3 个客户端全部完成，无串话");
    Ok(())
}
