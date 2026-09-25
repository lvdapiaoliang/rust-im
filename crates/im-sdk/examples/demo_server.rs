//! 演示服务端：SDK 冒烟演示（`java/im/sdk/Demo.java`）的配套对端。
//!
//! 与 im-server 主程序不同，这里**不依赖 `PostgreSQL`**——直接用
//! `SessionConfig` + `StaticToken` 起一个纯 TCP 会话核心，一条命令可跑：
//!
//! ```text
//! cargo run -p im-sdk --release --example demo_server
//! ```
//!
//! 认证口径：任意 `user_id` + token `"demo"`（写死、简单、只用于演示——
//! 真实部署请用 im-server 主程序 + 数据库鉴权）。
//!
//! 固定端口 18888：`Demo.java` 的默认地址就是它——冒烟脚本和手动体验都
//! 不用解析输出里的随机端口（端口被占则启动失败，错误明确不掩盖）。

use std::sync::Arc;
use std::time::Duration;

use im_server::{SessionConfig, Sessions, StaticToken, serve};
use im_transport::shutdown_channel;

/// 演示服务端固定监听地址（与 `java/im/sdk/Demo.java` 的默认参数一致）。
const DEMO_ADDR: &str = "127.0.0.1:18888";

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let config = SessionConfig {
        // 压测/演示共用同一鉴权器形态（docs/16 的 conn-storm 同款）
        authenticator: Arc::new(StaticToken { token: "demo".to_owned() }),
        ..SessionConfig::default()
    };
    // 与 spawn_server 的区别只有一处：listener 自己 bind 固定地址
    // （spawn_server 内部绑 127.0.0.1:0 随机端口，服务于测试的隔离性）
    let listener = tokio::net::TcpListener::bind(DEMO_ADDR).await?;
    let sessions = Sessions::new(config);
    let (shutdown_tx, shutdown_rx) = shutdown_channel();
    tokio::spawn(serve(listener, sessions, shutdown_rx));
    println!("im-sdk demo server listening on {DEMO_ADDR}");
    println!("token = \"demo\"，任意 user_id 可登录；Ctrl-C 退出");

    // Ctrl-C → 触发关停信号（accept/连接循环都订阅它）。
    // 演示服务端不追求完整排干语义：给在途连接 300ms 收尾后退出。
    let _ = tokio::signal::ctrl_c().await;
    println!("shutdown signal received");
    shutdown_tx.trigger();
    tokio::time::sleep(Duration::from_millis(300)).await;
    println!("demo server stopped");
    Ok(())
}
