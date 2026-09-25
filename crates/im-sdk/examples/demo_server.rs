//! 演示服务端：SDK 冒烟演示（`java/im/sdk/Demo.java`）的配套对端。
//!
//! 与 im-server 主程序不同，这里**不依赖 PostgreSQL**——直接用
//! `SessionConfig` + `StaticToken` 起一个纯 TCP 会话核心，一条命令可跑：
//!
//! ```text
//! cargo run -p im-sdk --release --example demo_server
//! ```
//!
//! 认证口径：任意 user_id + token `"demo"`（写死、简单、只用于演示——
//! 真实部署请用 im-server 主程序 + 数据库鉴权）。

use std::sync::Arc;
use std::time::Duration;

use im_server::{SessionConfig, StaticToken};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let config = SessionConfig {
        // 压测/演示共用同一鉴权器形态（docs/16 的 conn-storm 同款）
        authenticator: Arc::new(StaticToken { token: "demo".to_owned() }),
        ..SessionConfig::default()
    };
    let (addr, _sessions, shutdown_tx) = im_server::spawn_server(config).await?;
    println!("im-sdk demo server listening on {addr}");
    println!("token = \"demo\"，任意 user_id 可登录；Ctrl-C 退出");

    // Ctrl-C → 触发 spawn_server 返回的关停信号（accept/连接循环都订阅它）。
    // 演示服务端不追求完整排干语义：给在途连接 300ms 收尾后退出。
    let _ = tokio::signal::ctrl_c().await;
    println!("shutdown signal received");
    shutdown_tx.trigger();
    tokio::time::sleep(Duration::from_millis(300)).await;
    println!("demo server stopped");
    Ok(())
}
