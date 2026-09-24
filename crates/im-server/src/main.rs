//! # im-server：IM 服务端
//!
//! 阶段 3 的完整形态：
//! - 网关接入（复用 `im-transport`：读循环/写 actor/心跳/空闲超时/优雅关闭）
//! - 会话路由表（手写分片并发哈希表 `router`）：`user_id` → 连接
//! - 会话层（`session`）：握手认证、消息路由、离线暂存、断点同步
//! - 雪花 ID（`snowflake`）：全局消息 ID / 会话 ID 生成
//! - 离线消息（内存版，阶段 4 持久化进 `im-storage`）
//! - 分布式预留：一致性哈希环路由（阶段 8）
//!
//! 学习文档：`docs/06-server-arch.md`

use anyhow::Result;
use im_server::{serve, SessionConfig, Sessions};
use im_transport::shutdown_channel;

/// 默认监听地址（可用 `IM_SERVER_ADDR` 覆盖）。
const DEFAULT_ADDR: &str = "0.0.0.0:8888";

#[tokio::main]
async fn main() -> Result<()> {
    let addr = std::env::var("IM_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let sessions = Sessions::new(SessionConfig::default());
    let (shutdown_tx, shutdown_rx) = shutdown_channel();

    println!("im-server listening on {addr} (Ctrl-C to stop)");

    // Ctrl-C → 优雅关停：accept 停止，各连接排干出站队列后退出
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        println!("shutdown signal received");
        shutdown_tx.trigger();
    });

    serve(listener, sessions, shutdown_rx).await?;
    println!("im-server stopped");
    Ok(())
}
