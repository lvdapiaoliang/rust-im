//! QUIC 装配层（阶段 13）：quinn 双向流接进既有帧协议。
//!
//! 分工（与 `im_crypto::tls` 各管一半，同 [`crate::tls`] 的切法）：
//! - **材料**（im-crypto）：rustls 配置（QUIC 版钉 TLS 1.3 + ALPN）；
//! - **装配**（本模块）：UDP endpoint → QUIC 连接（TLS 1.3 随握手内建）
//!   → 双向流 → [`Connection`] 帧流。
//!
//! # QUIC 与 TCP 的三处本质差异（本模块的 API 形状由此决定）
//!
//! 1. **连接与流分离**：TCP 里「连接」就是字节流本身；QUIC 的连接是
//!    **流的多路复用器**——一条连接上可开任意多条双向流，互不阻塞
//!    （传输层无队头阻塞）。所以 API 里 [`QuicConnection`]（多路复用
//!    句柄，clone 廉价）与 [`QuicStream`]（单条流）是两个类型；
//! 2. **握手内建**：QUIC v1 把 TLS 1.3 拉进自己的握手（RFC 9001），
//!    没有「先 connect 再 handshake」两步——`connect` 返回即加密通道
//!    已建立。ALPN 是强制项（见 [`im_crypto::QUIC_ALPN`]）；
//! 3. **流是收发半部的组合**：quinn 把一条双向流拆成 `SendStream` +
//!    `RecvStream` 两个独立类型，而帧协议的 [`Connection`] 需要一个
//!    同时实现 `AsyncRead + AsyncWrite` 的类型——[`QuicStream`] 把
//!    两个半部粘回来，对帧层伪装成「一条普通的流」。
//!
//! ```text
//!   UDP socket ──quinn──▶ QuicConnection（多路复用器）
//!                              ├──▶ QuicStream #1 ──▶ Connection<QuicStream>（帧流）
//!                              ├──▶ QuicStream #2 ──▶ …（流间互不阻塞）
//!                              └──▶ …
//!   （心跳/超时/优雅关闭在网关层：GatewayStream 对 QUIC 流的实现
//!     让阶段 2 的全部连接生命周期管理零改动复用——阶段 12 泛型化
//!     付的那笔钱，在这里收利息）
//! ```
//!
//! 诚实边界：连接迁移（客户端换网/换端口、按 `ConnectionId` 重连）与
//! 0-RTT 是 quinn 的能力面，本模块的 API 尚未暴露（TLS 1.3 配置钉
//! session ticket 才有 0-RTT；迁移需要 endpoint 级配置）——记入
//! docs/19 的已知取舍，不装作已经做了。

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::connection::Connection;
use crate::error::TransportError;
use crate::gateway::GatewayStream;

/// quinn 错误 → IO 错误的收口。
///
/// QUIC 的错误类别（连接关闭/流重置/握手超时）对帧协议层都是
/// 「这条流的 IO 失败」——与 TLS 握手失败同一条分类原则：
/// 错误按**处置方式**分类，不按发生位置分类。
fn io_err<E>(e: E) -> TransportError
where
    E: std::error::Error + Send + Sync + 'static,
{
    TransportError::Io(io::Error::other(e))
}

/// 地址串解析：装配层统一收口（调用方传 `&str`，错误以 IO 形态上抛）。
fn parse_addr(addr: &str) -> Result<SocketAddr, TransportError> {
    addr.parse().map_err(|e| TransportError::Io(io::Error::new(io::ErrorKind::InvalidInput, e)))
}

/// QUIC 连接句柄：流的多路复用器（阶段 13 的核心新概念）。
///
/// TCP 里连接即流；QUIC 里连接是**流的容器**——本句柄上可以开
/// 任意多条双向流（[`open_stream`] 主动开 / [`accept_stream`] 被动收），
/// 流与流之间在传输层互不阻塞（无队头阻塞，见模块文档）。
/// clone 廉价（内部就是 quinn 的引用计数），可分发给任意 task。
///
/// [`open_stream`]: Self::open_stream
/// [`accept_stream`]: Self::accept_stream
#[derive(Clone, Debug)]
pub struct QuicConnection {
    conn: quinn::Connection,
}

impl QuicConnection {
    /// 对端地址。
    ///
    /// 每次调用动态取（不是握手时的快照）：QUIC 的**连接迁移**允许
    /// 客户端换网/换端口后凭 `ConnectionId` 续命——地址是「当前」的。
    #[must_use]
    pub fn remote_addr(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// 主动开一条双向流（客户端视角：「我要跟对端说话」）。
    ///
    /// QUIC 流号低 2 位是方向与发起方标记：客户端主动开的流从 0 起，
    /// 服务端主动开的流从 1 起——同一连接里双方可同时开流互不撞号。
    ///
    /// # Errors
    ///
    /// 连接已关闭/对端拒绝流（`RESET_STREAM`）时以 IO 错误返回。
    pub async fn open_stream(&self) -> Result<QuicStream, TransportError> {
        let (send, recv) = self.conn.open_bi().await.map_err(io_err)?;
        Ok(QuicStream { send, recv, conn: self.clone() })
    }

    /// 被动收一条双向流（服务端视角：「对端开了一条流」）。
    ///
    /// 每连接一条会话流的 IM 形态下，accept 循环长这样：
    ///
    /// ```no_run
    /// # use im_transport::quic::{QuicAcceptor, QuicConnection};
    /// # async fn example(mut conn: QuicConnection) {
    /// loop {
    ///     let Ok(stream) = conn.accept_stream().await else { break };
    ///     // 每条流 spawn 一个会话 task（对照 TCP：每连接一个 task）
    /// }
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// 连接关闭（对端走完/自己关停）时以 IO 错误返回——accept 循环
    /// 拿到错误就退出，连接的生命周期到此为止。
    pub async fn accept_stream(&self) -> Result<QuicStream, TransportError> {
        let (send, recv) = self.conn.accept_bi().await.map_err(io_err)?;
        Ok(QuicStream { send, recv, conn: self.clone() })
    }

    /// 把一条流包成帧连接（`open_stream` + `Connection::new` 一步到位）。
    ///
    /// # Errors
    ///
    /// 同 [`Self::open_stream`]。
    pub async fn open_frame(&self) -> Result<Connection<QuicStream>, TransportError> {
        Ok(Connection::new(self.open_stream().await?))
    }
}

/// 一条 QUIC 双向流：quinn 收发半部的组合，对帧层伪装成「一条普通的流」。
///
/// 帧协议的 [`Connection`] 要求 `AsyncRead + AsyncWrite` 齐备，而 quinn
/// 把双向流拆成 `SendStream`（只写）与 `RecvStream`（只读）两个类型——
/// 本结构把两个半部粘回来：读委托 `recv`、写委托 `send`，`poll_shutdown`
/// 映射到 QUIC 的 FIN（流正常半关闭，对端 `read_frame` 会看到 EOF——
/// 与 TCP 的 FIN 语义对齐，帧层的连接关闭检测原样可用）。
pub struct QuicStream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    /// 所属连接（开 sibling 流的入口；地址也从这里动态取）
    conn: QuicConnection,
}

impl QuicStream {
    /// 所属的连接句柄（多路复用：在同一条连接上再开流）。
    #[must_use]
    pub fn connection(&self) -> &QuicConnection {
        &self.conn
    }
}

impl AsyncRead for QuicStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // 两个半部都是 Unpin：Pin 只是被 trait 签名要求的仪式，无自引用。
        // 固有方法遮蔽陷阱：`RecvStream` 有自己的 `poll_read` 固有方法
        // （返回 quinn 错误类型），直接 `.poll_read(...)` 会解析到固有方法
        // 而 trait impl 不可见——必须全限定走 tokio 的 impl（返回 io::Error，
        // 帧协议层只认 IO 错误）。这是「固有方法与 trait 方法同名」的又一例
        // （同 docs/20 §7.2 #9 的 Hmac 歧义：同名的两件事，语义不同）。
        <quinn::RecvStream as AsyncRead>::poll_read(Pin::new(&mut self.get_mut().recv), cx, buf)
    }
}

impl AsyncWrite for QuicStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        <quinn::SendStream as AsyncWrite>::poll_write(Pin::new(&mut self.get_mut().send), cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        <quinn::SendStream as AsyncWrite>::poll_flush(Pin::new(&mut self.get_mut().send), cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // QUIC 的流关闭（FIN）——对端读到流尾，与 TCP 半关闭语义对齐
        <quinn::SendStream as AsyncWrite>::poll_shutdown(Pin::new(&mut self.get_mut().send), cx)
    }
}

// ── GatewayStream：QUIC 流直接进网关（心跳/超时/优雅关闭全复用）──

impl GatewayStream for QuicStream {
    fn peer_addr(&self) -> Option<SocketAddr> {
        // 动态取连接当前地址（见 QuicConnection::remote_addr 的迁移说明）
        Some(self.conn.remote_addr())
    }
}

/// QUIC 服务端：bind UDP 端口 + accept 连接。
///
/// 收 [`rustls::ServerConfig`]（QUIC 版，见 `TlsMaterial::quic_server_config`）
/// 而不是 quinn 配置——quinn 类型不越出装配层，上游只需要懂 rustls
/// （与 [`crate::tls::TlsAcceptor`] 同一条纪律）。
///
/// 转换在装配层内完成：quinn 不直接收 `rustls::ServerConfig`，
/// 要包一层 [`quinn::crypto::rustls::QuicServerConfig`]——它额外携带
/// QUIC 首包加密需要的初始套件（握手前的包也要加密，TLS 套件是
/// 握手后才有的事，这个「鸡生蛋」由固定 AES-128-GCM-SHA256 解决）。
/// 转换会校验配置启用了 TLS 1.3（QUIC 硬性要求），不满足直接报错。
///
/// # Examples
///
/// ```no_run
/// # use im_crypto::TlsMaterial;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let material = TlsMaterial::generate_demo()?;
/// let acceptor = im_transport::quic::QuicAcceptor::bind(
///     "127.0.0.1:18890", material.quic_server_config()?)?;
/// let conn = acceptor.accept_conn().await?; // QUIC 连接（TLS 1.3 已内建完成）
/// let stream = conn.accept_stream().await?; // 第一条双向流
/// # Ok(()) }
/// ```
///
/// # Errors
///
/// 地址非法（`InvalidInput`）或 UDP 端口绑定失败时以 IO 错误返回。
pub struct QuicAcceptor {
    endpoint: quinn::Endpoint,
}

impl QuicAcceptor {
    /// 绑定 UDP 端口并装配 QUIC 服务端配置。
    ///
    /// # Errors
    ///
    /// 见类型文档：地址解析/端口绑定失败。
    pub fn bind(addr: &str, config: rustls::ServerConfig) -> Result<Self, TransportError> {
        let quic_config =
            quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(config)).map_err(io_err)?;
        let server = quinn::ServerConfig::with_crypto(Arc::new(quic_config));
        let endpoint = quinn::Endpoint::server(server, parse_addr(addr)?)?;
        Ok(Self { endpoint })
    }

    /// 本地 UDP 地址（demo 打印/日志用）。
    ///
    /// # Errors
    ///
    /// socket 已失效时以 IO 错误返回（正常运行期间不会）。
    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        self.endpoint.local_addr().map_err(TransportError::Io)
    }

    /// accept 一条 QUIC 连接——TLS 1.3 握手随连接建立**内建完成**，
    /// 返回时加密通道已就绪（对照 TCP+TLS 的两步：connect 再 handshake）。
    ///
    /// # Errors
    ///
    /// endpoint 已关闭（`Closed`）或握手失败（对端不是 QUIC/证书校验
    /// 不过等，以 IO 错误返回）。
    pub async fn accept_conn(&self) -> Result<QuicConnection, TransportError> {
        let incoming = self.endpoint.accept().await.ok_or(TransportError::Closed)?;
        let conn = incoming.await.map_err(io_err)?;
        Ok(QuicConnection { conn })
    }

    /// accept 连接 + 等第一条双向流 + 包成帧连接。
    ///
    /// 「一条流一条会话」的 IM 形态下，这是 accept 循环最顺手的一步：
    /// 返回的句柄还能在同一连接上继续收流（多路复用）。
    ///
    /// # Errors
    ///
    /// 同 [`Self::accept_conn`] 与 [`QuicConnection::accept_stream`]。
    pub async fn accept(&self) -> Result<(QuicConnection, Connection<QuicStream>), TransportError> {
        let conn = self.accept_conn().await?;
        let stream = conn.accept_stream().await?;
        Ok((conn, Connection::new(stream)))
    }
}

/// QUIC 客户端：UDP endpoint + 默认客户端配置。
///
/// `server_name` 与拨号地址独立（与 [`crate::tls::TlsConnector`] 同一条
/// 设计）：证书按 `server_name` 校验（SNI），路由按地址。
pub struct QuicConnector {
    endpoint: quinn::Endpoint,
}

impl QuicConnector {
    /// 构造客户端 endpoint：本地 UDP 绑随机端口 + 设默认配置。
    ///
    /// # Errors
    ///
    /// 本地 UDP 绑定失败时以 IO 错误返回（几乎不可达，随机端口总能绑上）。
    pub fn new(config: rustls::ClientConfig) -> Result<Self, TransportError> {
        let bind_any: SocketAddr = "0.0.0.0:0".parse().map_err(|e: std::net::AddrParseError| {
            TransportError::Io(io::Error::new(io::ErrorKind::InvalidInput, e))
        })?;
        let mut endpoint = quinn::Endpoint::client(bind_any)?;
        let quic_config =
            quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(config)).map_err(io_err)?;
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_config)));
        Ok(Self { endpoint })
    }

    /// 连接服务端——QUIC 握手（含 TLS 1.3 + ALPN 协商）在此完成。
    ///
    /// # Errors
    ///
    /// 地址非法、UDP 拨号失败、握手失败（证书不受信/SAN 不匹配/ALPN
    /// 不一致——QUIC 强制协商，任何一项不过都连不上）时以 IO 错误返回。
    pub async fn connect(
        &self,
        addr: &str,
        server_name: &str,
    ) -> Result<QuicConnection, TransportError> {
        // connect 返回的 Connecting 是握手 future；两步都可能失败
        // （拨号参数错 / 握手不过），都收口成 IO 错误
        let connecting = self.endpoint.connect(parse_addr(addr)?, server_name).map_err(io_err)?;
        let conn = connecting.await.map_err(io_err)?;
        Ok(QuicConnection { conn })
    }

    /// 连接 + 开第一条双向流 + 包成帧连接（客户端最顺手的一步）。
    ///
    /// # Errors
    ///
    /// 同 [`Self::connect`] 与 [`QuicConnection::open_stream`]。
    pub async fn connect_frame(
        &self,
        addr: &str,
        server_name: &str,
    ) -> Result<Connection<QuicStream>, TransportError> {
        let conn = self.connect(addr, server_name).await?;
        let stream = conn.open_stream().await?;
        Ok(Connection::new(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use im_crypto::TlsMaterial;
    use im_protocol::{Cmd, Frame};
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    use crate::gateway::{GatewayConfig, run_gateway_connection};
    use crate::shutdown::shutdown_channel;

    /// 测试脚手架：QUIC echo 服务端（每连接循环收流、每流 echo 一帧），
    /// 返回监听地址。真实 quinn + 真实 UDP loopback，零 mock。
    /// （不 async：await 都在 spawn 的块里，函数体自身没有要等的东西；
    /// 也不返 Result：绑定随机端口从不失败，失败即测试崩——unwrap 直给）
    fn spawn_quic_echo(material: &TlsMaterial) -> SocketAddr {
        let acceptor =
            QuicAcceptor::bind("127.0.0.1:0", material.quic_server_config().unwrap()).unwrap();
        let addr = acceptor.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok(quic_conn) = acceptor.accept_conn().await {
                tokio::spawn(async move {
                    while let Ok(stream) = quic_conn.accept_stream().await {
                        tokio::spawn(async move {
                            let mut conn = Connection::new(stream);
                            while let Ok(Some(frame)) = conn.read_frame().await {
                                if conn.write_frame(&frame).await.is_err() {
                                    break;
                                }
                            }
                        });
                    }
                });
            }
        });
        addr
    }

    /// 主线用例：QUIC 之上跑既有帧协议——`GatewayStream` 的意义
    /// （同一套 `read_frame`/`write_frame`，底层从 TCP 换成 QUIC 流，零改动）。
    #[tokio::test]
    async fn frame_roundtrip_over_quic() {
        let material = TlsMaterial::generate_demo().unwrap();
        let addr = spawn_quic_echo(&material);
        let connector = QuicConnector::new(material.quic_client_config().unwrap()).unwrap();

        let mut client = connector.connect_frame(&addr.to_string(), "localhost").await.unwrap();
        client
            .write_frame(&Frame::new(Cmd::Msg, 1, 0, Bytes::from_static(b"hello over quic")))
            .await
            .unwrap();

        let reply = timeout(Duration::from_secs(2), client.read_frame())
            .await
            .expect("2s 内应收到回帧")
            .expect("连接正常")
            .expect("连接未关闭");
        assert_eq!(reply.cmd, Cmd::Msg);
        assert_eq!(reply.payload, Bytes::from_static(b"hello over quic"));
    }

    /// QUIC 流直接进网关：Ping 自动回 Pong——阶段 2 的连接生命周期管理
    /// （心跳应答/超时/优雅关闭）对 QUIC 流零改动复用。
    #[tokio::test]
    async fn quic_stream_goes_through_gateway() {
        let material = TlsMaterial::generate_demo().unwrap();
        let server_config = material.quic_server_config().unwrap();
        let acceptor = QuicAcceptor::bind("127.0.0.1:0", server_config).unwrap();
        let addr = acceptor.local_addr().unwrap();

        let (inbound_tx, _inbound_rx) = mpsc::channel(16);
        let (_shutdown_tx, shutdown_rx) = shutdown_channel();
        tokio::spawn(async move {
            let Ok(quic_conn) = acceptor.accept_conn().await else { return };
            let Ok(stream) = quic_conn.accept_stream().await else { return };
            // 网关拿到的就是 QuicStream——与 TcpStream/TlsStream 同一个 trait 入口
            let _ =
                run_gateway_connection(stream, GatewayConfig::default(), inbound_tx, shutdown_rx)
                    .await;
        });

        let connector = QuicConnector::new(material.quic_client_config().unwrap()).unwrap();
        let mut client = connector.connect_frame(&addr.to_string(), "localhost").await.unwrap();
        client.write_frame(&Frame::new(Cmd::Ping, 7, 0, Bytes::new())).await.unwrap();

        let pong = timeout(Duration::from_secs(2), client.read_frame())
            .await
            .expect("2s 内应收到 Pong")
            .expect("连接正常")
            .expect("连接未关闭");
        assert_eq!(pong.cmd, Cmd::Pong);
        assert_eq!(pong.ack, 8, "累计确认：ack = 收到的 seq + 1");
    }

    /// QUIC 的招牌：同一连接上流与流互不阻塞（传输层无队头阻塞）。
    ///
    /// 服务端在流 A 上读到一帧后故意挂住 10 秒；同一连接的流 B 的
    /// echo 往返必须畅通——TCP 单连接上这是不可能的：字节流交织，
    /// 前面的字节不解完，后面的帧到不了解码器（应用层多路复用也逃
    /// 不掉这条队头阻塞，QUIC 的流级独立是传输层给的）。
    #[tokio::test]
    async fn streams_multiplex_without_head_of_line_blocking() {
        let material = TlsMaterial::generate_demo().unwrap();
        let server_config = material.quic_server_config().unwrap();
        let acceptor = QuicAcceptor::bind("127.0.0.1:0", server_config).unwrap();
        let addr = acceptor.local_addr().unwrap();

        tokio::spawn(async move {
            let quic_conn = acceptor.accept_conn().await.unwrap();
            // 流 A：慢消费者（读一帧后挂住不回）
            let slow_stream = quic_conn.accept_stream().await.unwrap();
            tokio::spawn(async move {
                let mut slow = Connection::new(slow_stream);
                let _ = slow.read_frame().await;
                tokio::time::sleep(Duration::from_secs(10)).await;
            });
            // 流 B：正常 echo——它不该替流 A 还债
            let echo_stream = quic_conn.accept_stream().await.unwrap();
            tokio::spawn(async move {
                let mut echo = Connection::new(echo_stream);
                if let Ok(Some(frame)) = echo.read_frame().await {
                    let _ = echo.write_frame(&frame).await;
                }
            });
        });

        let connector = QuicConnector::new(material.quic_client_config().unwrap()).unwrap();
        let quic_conn = connector.connect(&addr.to_string(), "localhost").await.unwrap();

        // 先开流 A 发一帧（服务端会挂住它），再开流 B 验证往返畅通
        let mut slow = quic_conn.open_frame().await.unwrap();
        slow.write_frame(&Frame::new(Cmd::Msg, 1, 0, Bytes::from_static(b"slow"))).await.unwrap();

        let mut fast = quic_conn.open_frame().await.unwrap();
        fast.write_frame(&Frame::new(Cmd::Msg, 2, 0, Bytes::from_static(b"fast"))).await.unwrap();
        let reply = timeout(Duration::from_secs(2), fast.read_frame())
            .await
            .expect("流 B 不应被挂住的流 A 阻塞——这就是 QUIC 无队头阻塞")
            .expect("连接正常")
            .expect("连接未关闭");
        assert_eq!(reply.payload, Bytes::from_static(b"fast"));
    }

    /// 不可信 CA 被拒：QUIC 内建 TLS 1.3 的信任锚真实生效
    /// （不是「UDP 通了就算连上」——证书校验在握手里真的在跑）。
    #[tokio::test]
    async fn untrusted_ca_is_rejected_over_quic() {
        let server_material = TlsMaterial::generate_demo().unwrap();
        let addr = spawn_quic_echo(&server_material);

        // 客户端信任的是另一套 CA——同源 CA 都没签过服务端证书
        let stranger = TlsMaterial::generate_demo().unwrap();
        let connector = QuicConnector::new(stranger.quic_client_config().unwrap()).unwrap();

        let result = connector.connect(&addr.to_string(), "localhost").await;
        assert!(result.is_err(), "不可信 CA 的 QUIC 握手必须失败，实际 {result:?}");
    }
}
