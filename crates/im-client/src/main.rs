//! # im-client：IM 客户端（阶段 4 TUI 版）
//!
//! 用法：`im-client [server_addr] [user_id] [token]`
//!
//! ratatui 三栏界面：左会话列表、右上消息区、右下输入框。
//! 按键见 `tui` 模块文档（Tab 切会话 / `/to <id>` 新会话 /
//! Enter 发送 / `/quit` 或 Esc 退出）。
//!
//! CLI 打印版（阶段 3 的过渡形态）已由 TUI 取代——事件流接口
//! （`ClientEvent`）不变，换的只是「渲染层」。

use im_client::{tui, ClientConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:8888".to_string());
    let user_id: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let token = args.next().unwrap_or_else(|| "demo".to_string());

    let config = ClientConfig {
        // 持久化到工作目录：聊天历史与重发表重启不丢
        // （临时目录会随系统清理，不适合真实使用；
        //   同一用户复用同一目录也保证了 client_msg_id 单调不复用）
        data_dir: Some(std::path::PathBuf::from("im-client-data")),
        ..ClientConfig::new(&addr, user_id, &token)
    };
    tui::run(config).await
}
