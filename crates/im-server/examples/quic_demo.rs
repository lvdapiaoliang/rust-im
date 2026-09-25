//! 阶段 13 演示：**会话核心整体跑在 QUIC 之上 + 两条会话流共用一条连接**。
//!
//! 运行：`cargo run -p im-server --example quic_demo`
//!
//! 这条 demo 证明两件事：
//!
//! 1. **升级到 QUIC 没有动会话核心一行代码**——`serve_connection`
//!    吃的是 `GatewayStream`，QUIC 流实现同一个 trait 就能进网关
//!    （阶段 12 泛型化付的钱，这里收利息，docs/18 §九预告的收款点）；
//!
//! 2. **QUIC 的招牌：多路复用**——Alice 和 Bob 不是两条 TCP 连接，
//!    而是**同一条 QUIC 连接上的两条双向流**（一个 UDP 五元组）。
//!    换成 TCP 这是两条独立连接；应用层在单连接上复用（如 HTTP/2）
//!    又会引入字节流交织的队头阻塞——QUIC 的流级独立是传输层给的。
//!
//! ```text
//!   TLS 形态（tls_demo）              QUIC 形态（本 demo）
//!   ─────────────────                ─────────────────
//!   TCP accept（两条连接）           UDP accept_conn（一条连接）
//!   ├ Alice 连接 → serve_connection  ├ Alice 流 ──┐
//!   └ Bob 连接   → serve_connection  └ Bob 流   ──┴→ serve_connection
//!                                     （每流一个会话，同一个函数）
//! ```

use bytes::Bytes;
use im_crypto::TlsMaterial;
use im_protocol::{Handshake, HandshakeAck, Msg, MsgAck, Payload};
use im_server::session::{SessionConfig, Sessions, serve_connection};
use im_transport::shutdown::shutdown_channel;
use im_transport::{
    Connection, GatewayConfig, QuicAcceptor, QuicConnection, QuicConnector, QuicStream,
};

/// 演示端口固定（紧挨 tls_demo 的 18889，方便对比抓包：
/// 一个抓 TCP，一个抓 UDP）。
const DEMO_ADDR: &str = "127.0.0.1:18890";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── 1. 材料 + QUIC 双端配置：rustls 配置钉 TLS 1.3 + ALPN，
    //       证书材料与 TLS 形态完全复用（同一套 CA 管两种传输）──
    let material = TlsMaterial::generate_demo()?;
    let acceptor = QuicAcceptor::bind(DEMO_ADDR, material.quic_server_config()?)?;
    let connector = QuicConnector::new(material.quic_client_config()?)?;
    println!("[server] QUIC 监听已启动（UDP）：{DEMO_ADDR}");

    let sessions = Sessions::new(SessionConfig::default());
    let (shutdown_tx, shutdown_rx) = shutdown_channel();

    // ── 2. 服务端：accept 连接，每连接循环收流、每流一个会话核心
    //       （与 TLS 形态唯一的区别：连接里还有第二层循环）──
    let acceptor_handle = tokio::spawn(async move {
        let mut accept_shutdown = shutdown_rx.clone();
        loop {
            tokio::select! {
                () = accept_shutdown.wait() => break,
                accepted = acceptor.accept_conn() => {
                    // 握手失败（对端不是 QUIC）→ 丢弃，不影响 accept 循环
                    let Ok(quic_conn) = accepted else { continue };
                    let sessions = sessions.clone();
                    let shutdown = shutdown_rx.clone();
                    tokio::spawn(async move {
                        // 每连接的收流循环：一条流 = 一个会话（多路复用）
                        while let Ok(stream) = quic_conn.accept_stream().await {
                            let sessions = sessions.clone();
                            let shutdown = shutdown.clone();
                            let conn_id = sessions.next_conn_id();
                            tokio::spawn(async move {
                                let _ = serve_connection(
                                    &sessions, conn_id, stream,
                                    GatewayConfig::default(), shutdown,
                                )
                                .await;
                            });
                        }
                    });
                }
            }
        }
    });

    // ── 3. 客户端：一条 QUIC 连接，Alice/Bob 各开一条流 ──
    let quic_conn = connector.connect(DEMO_ADDR, "localhost").await?;
    println!("[client] QUIC 连接已建立（TLS 1.3 随握手内建完成）：{}", quic_conn.remote_addr());

    let mut alice = QuicDemoClient::open_stream(&quic_conn).await?;
    let mut bob = QuicDemoClient::open_stream(&quic_conn).await?;
    println!("[client] 同一连接上开了两条流：Alice 与 Bob（多路复用）");

    let alice_ack = alice.handshake(1001).await?;
    let bob_ack = bob.handshake(1002).await?;
    println!("[alice] 握手通过，session_id = {}", alice_ack.session_id);
    println!("[bob]   握手通过，session_id = {}", bob_ack.session_id);

    // ── 4. 业务：Alice → Bob 中文消息，全链路（QUIC 包 + TLS 1.3）无损 ──
    let text = "你好 Bob，这条消息走 QUIC 流（同连接多路复用）";
    alice.send_msg(1002, text).await?;

    let ack: MsgAck = alice.recv().await?;
    println!("[alice] 收到 MsgAck：msg_id = {}", ack.msg_id);

    let incoming: Msg = bob.recv().await?;
    // Msg 的 content 是原始字节：按 UTF-8 还原成文本比对
    let received = String::from_utf8(incoming.content.to_vec())?;
    println!("[bob]   收到消息：{received}");
    assert_eq!(received, text, "QUIC 链路上的中文必须逐字无损");

    // 反向一条：Bob 的流独立收发，双向加密照常
    bob.send_msg(1001, "收到！Alice（流 #2 回信）").await?;
    let back: Msg = alice.recv().await?;
    let back_text = String::from_utf8(back.content.to_vec())?;
    assert_eq!(back_text, "收到！Alice（流 #2 回信）");
    println!("[alice] 收到回信：{back_text}");

    // ── 5. 优雅收尾 ──
    shutdown_tx.trigger();
    acceptor_handle.await?;
    println!("[server] 已优雅关停，QUIC demo 全链路验证通过 ✅");
    Ok(())
}

/// QUIC 客户端小脚手架：**一条流**上的帧连接 + 递增 seq（与协议约定一致）。
/// 对照 tls_demo 的 TlsDemoClient：只有 connect 换成了「在共享连接上开流」，
/// 其余逐行相同——上层根本感知不到传输换了。
struct QuicDemoClient {
    conn: Connection<QuicStream>,
    seq: u64,
    client_msg_id: u64,
}

impl QuicDemoClient {
    /// 在既有连接上开一条新流（QUIC 多路复用的客户端侧形态）。
    async fn open_stream(quic_conn: &QuicConnection) -> anyhow::Result<Self> {
        let conn = quic_conn.open_frame().await?;
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
