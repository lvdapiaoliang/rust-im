//!
//! 阶段 5 的完整形态：**双接入服务进程**。
//! - TCP 网关（8888）：TUI 客户端的二进制协议路径（保持不变）
//! - Web 服务（8080）：REST `/api` + WS `/ws`（阶段 5 新增，与 TCP 共用会话核心）
//! - PostgreSQL：账号/好友/群组/文件元数据（启动时自动迁移）
//!
//! 学习文档：`docs/06-server-arch.md`、`docs/12-web-protocol.md`
//!

use anyhow::{Context, Result};
use im_server::SessionConfig;
use im_server::session::Sessions;
use im_server::web::api::{self, AppState};
use im_server::web::db;
use im_transport::shutdown_channel;
use tokio::net::TcpListener;

/// TCP 网关默认监听地址（可用 `IM_SERVER_ADDR` 覆盖）。
const DEFAULT_TCP_ADDR: &str = "0.0.0.0:8888";
/// Web 服务默认监听地址（可用 `IM_WEB_ADDR` 覆盖）。
const DEFAULT_WEB_ADDR: &str = "0.0.0.0:8080";
/// 上传文件的落盘根目录（可用 `IM_FILES_DIR` 覆盖）。
const DEFAULT_FILES_DIR: &str = "data/files";

#[tokio::main]
async fn main() -> Result<()> {
    let tcp_addr = std::env::var("IM_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_TCP_ADDR.to_string());
    let web_addr = std::env::var("IM_WEB_ADDR").unwrap_or_else(|_| DEFAULT_WEB_ADDR.to_string());

    // Web 依赖持久化：先连库并迁移（连不上就快速失败——REST/WS 都没法服务）
    let pool = db::connect_and_migrate()
        .await
        .context("连接 PostgreSQL 失败（检查 DATABASE_URL 或本机库是否启动）")?;

    let sessions = Sessions::new(SessionConfig::default());
    let files_dir = std::env::var("IM_FILES_DIR").unwrap_or_else(|_| DEFAULT_FILES_DIR.to_string());
    let state = AppState::new(pool, sessions.clone(), &files_dir)
        .await
        .context("初始化文件存储目录失败")?;

    // ── Web 服务：REST（WS 网关阶段 5 后续接入同一端口）──
    let web_listener =
        TcpListener::bind(&web_addr).await.with_context(|| format!("绑定 {web_addr} 失败"))?;
    println!("im-server web  listening on http://{web_addr} (REST /api)");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(web_listener, api::router(state)).await {
            eprintln!("web 服务异常退出: {e}");
        }
    });

    // ── TCP 网关：与阶段 3/4 完全一致 ──
    let listener =
        TcpListener::bind(&tcp_addr).await.with_context(|| format!("绑定 {tcp_addr} 失败"))?;
    let (shutdown_tx, shutdown_rx) = shutdown_channel();
    println!("im-server tcp   listening on {tcp_addr} (Ctrl-C to stop)");

    // Ctrl-C → 优雅关停：accept 停止，各连接排干出站队列后退出
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        println!("shutdown signal received");
        shutdown_tx.trigger();
    });

    im_server::serve(listener, sessions, shutdown_rx).await?;
    println!("im-server stopped");
    Ok(())
}
