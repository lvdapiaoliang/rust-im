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

use im_server::{SessionConfig, StaticToken};
use im_transport::shutdown_channel;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let config = SessionConfig {
        // 压测/演示共用同一鉴权器形态（docs/16 的 conn-storm 同款）
        authenticator: Arc::new(StaticToken { token: "demo".to_owned() }),
        ..SessionConfig::default()
    };
    let (addr, _sessions, _shutdown) = im_server::spawn_server(config).await?;
    println!("im-sdk demo server listening on {addr}");
    println!("token = \"demo\"，任意 user_id 可登录；Ctrl-C 退出");

    // 阻塞到 Ctrl-C（_shutdown 交给 ctrl-c 触发，与主程序同一套优雅关停）
    let (shutdown_tx, shutdown_rx) = shutdown_channel();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown_tx.trigger();
    });
    let _ = shutdown_rx.is_triggered().await;
    println!("demo server stopped");
    Ok(())
}
