//! # im-client：IM 客户端（阶段 3 最小 CLI 版）
//!
//! 用法：`im-client [server_addr] [user_id] [token]`
//!
//! - 输入 `to 内容` 发送消息（如 `2 你好`）；
//! - 其余行被忽略；Ctrl-C 退出。
//!
//! TUI 界面是阶段 4 的话题——本入口只验证「协议全链路跑通」。

use bytes::Bytes;
use im_client::{run_client, ClientConfig, ClientEvent};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:8888".to_string());
    let user_id: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let token = args.next().unwrap_or_else(|| "demo".to_string());

    let config = ClientConfig::new(&addr, user_id, &token);
    println!("im-client: user {user_id} -> {addr}（输入 `to 内容` 发送，Ctrl-C 退出）");

    let (events_tx, mut events_rx) = mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = im_transport::shutdown_channel();
    let handle = run_client(config, events_tx, shutdown_rx).await;

    // 事件打印
    let printer = tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            match event {
                ClientEvent::Connected { session_id } => {
                    println!("[已连接 session={session_id}]");
                }
                ClientEvent::Disconnected => println!("[连接断开，重连中...]"),
                ClientEvent::Message(msg) => {
                    println!("[来自 {}] {}", msg.from, String::from_utf8_lossy(&msg.content));
                }
                ClientEvent::Ack { msg_id } => println!("[已送达 msg_id={msg_id}]"),
                ClientEvent::SyncBatch(messages) => {
                    for msg in messages {
                        println!(
                            "[离线补投 来自 {}] {}",
                            msg.from,
                            String::from_utf8_lossy(&msg.content)
                        );
                    }
                }
                ClientEvent::Rejected { reason } => {
                    println!("[登录被拒：{reason}]");
                    break;
                }
            }
        }
    });

    // stdin → send_msg；Ctrl-C → 退出
    let stdin = std::io::stdin();
    let input = tokio::spawn(async move {
        use std::io::BufRead;
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Some((to, content)) = line.split_once(' ') else {
                continue; // 格式：`to 内容`
            };
            let Ok(to) = to.trim().parse::<u64>() else {
                continue;
            };
            if content.is_empty() {
                continue;
            }
            let _ = handle.send_msg(to, Bytes::copy_from_slice(content.as_bytes())).await;
        }
        shutdown_tx.trigger();
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => println!("\nbye"),
        _ = printer => {}
        _ = input => {}
    }
    Ok(())
}
