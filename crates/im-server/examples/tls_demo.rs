//! 阶段 12 演示：**会话核心整体跑在 TLS 之上**。
//!
//! 运行：`cargo run -p im-server --example tls_demo`
//!
//! 这条 demo 的意义不是「TLS 能 echo」（im-transport 的单测已证明），
//! 而是证明**升级到 TLS 没有动会话核心一行代码**：
//!
//! ```text
//!   裸 TCP 形态（serve）            TLS 形态（本 demo）
//!   ─────────────────              ─────────────────
//!   accept → serve_connection      accept → accept_stream（多一步握手）
//!                                    → serve_connection（同一个函数！）
//! ```
//!
//! 认证、路由、离线暂存、seq 去重、优雅关闭全部照常工作——
//! 泛型化的 [`serve_connection`] 对「下面是不是加密的」无感。
//! 两条客户端连接（Alice/Bob）在加密链路上互发中文消息，
//! 从输出能看到雪花 `session_id、Ack` 与消息逐字无损。

use bytes::Bytes;
use im_crypto::TlsMaterial;
use im_protocol::{Handshake, HandshakeAck, Msg, MsgAck, Payload};
use im_server::session::{SessionConfig, Sessions, serve_connection};
use im_transport::shutdown::shutdown_channel;
use im_transport::tls::{TlsAcceptor, TlsConnector};
use im_transport::{Connection, GatewayConfig};
use tokio::net::TcpListener;

/// 演示端口固定（与 sdk 的 `demo_server` 同风格，方便抓包观察）。
const DEMO_ADDR: &str = "127.0.0.1:18889";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── 1. 材料 + 双端配置：进程内生成，无任何外部证书文件 ──
    let material = TlsMaterial::generate_demo()?;
    let acceptor = TlsAcceptor::new(material.server_config()?)?;
    let connector = TlsConnector::new(material.client_config()?)?;

    // ── 2. 服务端：accept 循环 + TLS 握手 + 会话核心（与 serve 唯一
    //       的区别是多一步 accept_stream）──
    let listener = TcpListener::bind(DEMO_ADDR).await?;
    let sessions = Sessions::new(SessionConfig::default());
    let (shutdown_tx, shutdown_rx) = shutdown_channel();

    let acceptor_handle = tokio::spawn(async move {
        let mut accept_shutdown = shutdown_rx.clone();
        loop {
            tokio::select! {
                () = accept_shutdown.wait() => break,
                accepted = listener.accept() => {
                    let Ok((tcp, _peer)) = accepted else { continue };
                    // TLS 握手失败（对端不是 TLS 客户端）→ 丢弃这条连接，
                    // 不影响 accept 循环
                    let Ok(tls) = acceptor.accept_stream(tcp).await else { continue };
                    let sessions = sessions.clone();
                    let shutdown = shutdown_rx.clone();
                    let conn_id = sessions.next_conn_id();
                    tokio::spawn(async move {
                        let _ = serve_connection(
                            &sessions, conn_id, tls, GatewayConfig::default(), shutdown,
                        )
                        .await;
                    });
                }
            }
        }
    });
    println!("[server] TLS 监听已启动：{DEMO_ADDR}");

    // ── 3. 客户端：Alice 与 Bob 双双走 TLS 接入 ──
    let mut alice = TlsDemoClient::connect(&connector, DEMO_ADDR).await?;
    let mut bob = TlsDemoClient::connect(&connector, DEMO_ADDR).await?;

    let alice_ack = alice.handshake(1001).await?;
    let bob_ack = bob.handshake(1002).await?;
    println!("[alice] 握手通过，session_id = {}", alice_ack.session_id);
    println!("[bob]   握手通过，session_id = {}", bob_ack.session_id);

    // ── 4. 业务：Alice → Bob 中文消息，全链路（含 TLS 记录层）无损 ──
    let text = "你好 Bob，这条消息全程走 TLS 记录层加解密";
    alice.send_msg(1002, text).await?;

    let ack: MsgAck = alice.recv().await?;
    println!("[alice] 收到 MsgAck：msg_id = {}", ack.msg_id);

    let incoming: Msg = bob.recv().await?;
    // Msg 的 content 是原始字节：按 UTF-8 还原成文本比对
    let received = String::from_utf8(incoming.content.to_vec())?;
    println!("[bob]   收到消息：{received}");
    assert_eq!(received, text, "TLS 链路上的中文必须逐字无损");

    // 反向一条，证明双向加密
    bob.send_msg(1001, "收到！Alice").await?;
    let back: Msg = alice.recv().await?;
    let back_text = String::from_utf8(back.content.to_vec())?;
    assert_eq!(back_text, "收到！Alice");
    println!("[alice] 收到回信：{back_text}");

    // ── 5. 优雅收尾 ──
    shutdown_tx.trigger();
    acceptor_handle.await?;
    println!("[server] 已优雅关停，TLS demo 全链路验证通过 ✅");
    Ok(())
}

/// TLS 客户端小脚手架：帧连接 + 递增 seq（与协议约定一致）。
struct TlsDemoClient {
    conn: Connection<im_transport::ClientTlsStream>,
    seq: u64,
    client_msg_id: u64,
}

impl TlsDemoClient {
    async fn connect(connector: &TlsConnector, addr: &str) -> anyhow::Result<Self> {
        // ServerName 固定 "localhost"（demo 证书的 SAN 之一）
        let conn = connector.connect(addr, "localhost").await?;
        Ok(Self { conn, seq: 0, client_msg_id: 0 })
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    async fn handshake(&mut self, user_id: u64) -> anyhow::Result<HandshakeAck> {
        let hs = Handshake { user_id, token: "demo".to_string() };
        let frame = hs.encode_frame(self.next_seq(), 0);
        self.conn.write_frame(&frame).await?;
        self.recv().await
    }

    async fn send_msg(&mut self, to: u64, text: &str) -> anyhow::Result<()> {
        self.client_msg_id += 1;
        let msg = Msg {
            from: 0,
            to,
            msg_id: 0,
            client_msg_id: self.client_msg_id,
            content: Bytes::copy_from_slice(text.as_bytes()),
        };
        let frame = msg.encode_frame(self.next_seq(), 0);
        self.conn.write_frame(&frame).await?;
        Ok(())
    }

    async fn recv<T: Payload>(&mut self) -> anyhow::Result<T> {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(3), self.conn.read_frame())
            .await?? // 超时 → anyhow
            .ok_or_else(|| anyhow::anyhow!("连接已关闭"))?;
        Ok(T::decode_frame(&frame)?)
    }
}
