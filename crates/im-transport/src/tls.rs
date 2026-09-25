//! TLS 装配层（阶段 12）：把 rustls 握手后的流接进既有帧协议。
//!
//! 分工（与 `im_crypto::tls` 各管一半）：
//! - **材料**（im-crypto）：证书、rustls `ServerConfig`/`ClientConfig`；
//! - **装配**（本模块）：`TcpStream` → TLS 握手 → [`Connection`] 帧流。
//!
//! 装饰器模式在这里落地——三层流的层层包装：
//!
//! ```text
//!   TcpStream                ──tokio-rustls──▶  TlsStream<TcpStream>
//!        │                                        │
//!        └──────────── 同一套帧协议代码 ────────────┘
//!                        Connection<S>
//!   （Connection 泛型化后，TCP 与 TLS 共用 read_frame/write_frame，
//!     帧层代码对「下面是不是加密的」完全无感）
//! ```
//!
//! 握手语义：`accept`/`connect` 完整走完 TLS 握手才返回——
//! 返回即「加密通道已建立」，调用方拿到的 [`Connection`] 上的一切
//! 读写都自动在记录层加解密里。握手失败表现为 `io::Error`
//!（对端拒绝、证书校验不过、证书与 ServerName 不匹配等），
//! 归入 [`TransportError::Io`]——TLS 握手失败本质上就是这条连接的
//! IO 失败，不值得单设错误类别。

use std::sync::Arc;

use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream as ClientTlsStreamInner;
use tokio_rustls::server::TlsStream as ServerTlsStreamInner;
use tokio_rustls::{TlsAcceptor as RustlsAcceptor, TlsConnector as RustlsConnector};

use crate::connection::Connection;
use crate::error::TransportError;
use crate::gateway::GatewayStream;

/// 服务端 TLS 流（握手完成后的类型）。
pub type ServerTlsStream = ServerTlsStreamInner<TcpStream>;
/// 客户端 TLS 流（握手完成后的类型）。
pub type ClientTlsStream = ClientTlsStreamInner<TcpStream>;

/// TLS 服务端：accept 一条 TCP 连接并完成服务端侧握手。
///
/// 典型用法：acceptor 提前 `clone` 进 accept 循环（内部 `Arc`，
/// clone 廉价），每条连接握手后直接交给 [`Connection::new`] 包成帧流，
/// 或交给 [`crate::spawn_gateway`] 享受完整的连接生命周期管理。
///
/// # Examples
///
/// ```no_run
/// # use im_crypto::TlsMaterial;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let material = TlsMaterial::generate_demo()?;
/// let acceptor = im_transport::tls::TlsAcceptor::new(material.server_config()?)?;
/// let listener = tokio::net::TcpListener::bind("127.0.0.1:18888").await?;
/// let (tcp, _) = listener.accept().await?;
/// let mut conn = acceptor.accept(tcp).await?; // TLS 握手在此完成
/// # Ok(()) }
/// ```
///
/// # Errors
///
/// TLS 握手失败（对端不是 TLS 客户端、算法协商失败等）时返回
/// [`TransportError::Io`]。
#[derive(Clone)]
pub struct TlsAcceptor {
    inner: RustlsAcceptor,
}

impl TlsAcceptor {
    /// 从 rustls 服务端配置构造。
    ///
    /// # Errors
    ///
    /// 配置内部不完整（证书/私钥不匹配等）时返回 [`TransportError::Io`]。
    pub fn new(config: rustls::ServerConfig) -> Result<Self, TransportError> {
        Ok(Self { inner: RustlsAcceptor::from(Arc::new(config)) })
    }

    /// accept + 服务端握手，返回裸 TLS 流（网关集成的入口：
    /// [`crate::spawn_gateway`] 自己包 [`Connection`]，需要的是流而不是帧连接）。
    ///
    /// # Errors
    ///
    /// 见模块文档：握手失败以 IO 错误形式上抛。
    pub async fn accept_stream(&self, tcp: TcpStream) -> Result<ServerTlsStream, TransportError> {
        self.inner.accept(tcp).await.map_err(TransportError::Io)
    }

    /// accept + 服务端握手：返回的 [`Connection`] 已是加密帧流。
    ///
    /// # Errors
    ///
    /// 见模块文档：握手失败以 IO 错误形式上抛。
    pub async fn accept(
        &self,
        tcp: TcpStream,
    ) -> Result<Connection<ServerTlsStream>, TransportError> {
        let stream = self.accept_stream(tcp).await?;
        Ok(Connection::new(stream))
    }
}

/// TLS 客户端：连接 + 客户端侧握手。
///
/// `server_name` 是 TLS SNI/证书校验用的名字（如 `"localhost"`），
/// 与实际拨号的 `addr` 独立——证书按它校验，路由按 `addr`。
/// 这正是阶段 12 证书 SAN 写 `localhost` + `127.0.0.1` 的消费方。
#[derive(Clone)]
pub struct TlsConnector {
    inner: RustlsConnector,
}

impl TlsConnector {
    /// 从 rustls 客户端配置构造。
    ///
    /// # Errors
    ///
    /// 配置内部不完整时返回 [`TransportError::Io`]。
    pub fn new(config: rustls::ClientConfig) -> Result<Self, TransportError> {
        Ok(Self { inner: RustlsConnector::from(Arc::new(config)) })
    }

    /// TCP 连接 + TLS 握手，返回裸 TLS 流（网关集成的入口）。
    ///
    /// # Errors
    ///
    /// 同 [`Self::connect`]。
    pub async fn connect_stream(
        &self,
        addr: &str,
        server_name: &str,
    ) -> Result<ClientTlsStream, TransportError> {
        let tcp = TcpStream::connect(addr).await?;
        let name = rustls::pki_types::ServerName::try_from(server_name.to_string()).map_err(
            |e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()),
        )?;
        self.inner.connect(name, tcp).await.map_err(TransportError::Io)
    }

    /// TCP 连接 + TLS 握手一步完成：返回的 [`Connection`] 已是加密帧流。
    ///
    /// # Errors
    ///
    /// TCP 拨号失败、TLS 握手失败（证书不受信/SAN 不匹配等）时返回
    /// [`TransportError::Io`]；`server_name` 不是合法 DNS/IP 时同样
    /// 以 IO 错误（`InvalidInput`）返回。
    pub async fn connect(
        &self,
        addr: &str,
        server_name: &str,
    ) -> Result<Connection<ClientTlsStream>, TransportError> {
        let stream = self.connect_stream(addr, server_name).await?;
        Ok(Connection::new(stream))
    }
}

// ── GatewayStream：让 TLS 流直接进网关（心跳/超时/优雅关闭全复用）──

impl GatewayStream for ServerTlsStream {
    fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        // tokio-rustls 的 get_ref 返回 (&TcpStream, &ServerConnection) 元组：
        // 地址属于 TCP，只取 .0
        self.get_ref().0.peer_addr().ok()
    }
}

impl GatewayStream for ClientTlsStream {
    fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.get_ref().0.peer_addr().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use im_crypto::TlsMaterial;
    use im_protocol::{Cmd, Frame};
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    /// 测试脚手架：一个 TLS echo 服务端（accept → 收一帧 → 原样回），
    /// 返回监听地址。真实 rustls + 真实 TCP loopback，零 mock。
    async fn spawn_tls_echo(material: &TlsMaterial) -> std::io::Result<String> {
        let acceptor = TlsAcceptor::new(material.server_config().unwrap()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?.to_string();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut conn = acceptor.accept(tcp).await.unwrap();
            let frame = conn.read_frame().await.unwrap().expect("应读到一帧");
            conn.write_frame(&frame).await.unwrap();
        });
        Ok(addr)
    }

    /// 主线用例：TLS 之上跑既有帧协议——`Connection` 泛型化的意义
    /// （同一套 read_frame/write_frame，底层从 TCP 换成 TLS，零改动）。
    #[tokio::test]
    async fn frame_roundtrip_over_tls() {
        let material = TlsMaterial::generate_demo().unwrap();
        let addr = spawn_tls_echo(&material).await.unwrap();
        let connector = TlsConnector::new(material.client_config().unwrap()).unwrap();

        let mut client = connector.connect(&addr, "localhost").await.unwrap();
        let frame = Frame::new(Cmd::Msg, 7, 0, Bytes::from_static(b"hello over tls"));
        client.write_frame(&frame).await.unwrap();

        let echo = client.read_frame().await.unwrap().expect("应回一帧");
        assert_eq!(echo, frame);
    }

    /// 证书信任锚不匹配：客户端不认服务端 CA，握手必须失败
    ///（TLS 存在感测试——证明加密与校验真实发生，不是“能通就行”）
    #[tokio::test]
    async fn untrusted_ca_rejected_at_handshake() {
        let server_material = TlsMaterial::generate_demo().unwrap();
        let addr = spawn_tls_echo(&server_material).await.unwrap();
        // 客户端信的是**另一套** CA（信任锚 ≠ 服务端 CA）
        let stranger = TlsMaterial::generate_demo().unwrap();
        let connector = TlsConnector::new(stranger.client_config().unwrap()).unwrap();

        let err = connector.connect(&addr, "localhost").await;
        assert!(err.is_err(), "信任锚不匹配的握手必须失败");
    }

    /// SAN 不匹配：ServerName 写一个证书里没有的名字，握手必须失败
    #[tokio::test]
    async fn wrong_server_name_rejected() {
        let material = TlsMaterial::generate_demo().unwrap();
        let addr = spawn_tls_echo(&material).await.unwrap();
        let connector = TlsConnector::new(material.client_config().unwrap()).unwrap();

        // 证书 SAN 只有 localhost / 127.0.0.1，没有 evil.example
        let err = connector.connect(&addr, "evil.example").await;
        assert!(err.is_err(), "ServerName 与 SAN 不匹配必须失败");
    }

    /// 线上确实有 TLS：一个**裸 TCP 服务端**（不做 TLS upgrade）
    /// 直接读客户端发来的前 5 字节——若链路是明文，第一字节就该是
    /// 帧协议的已知明文头；实测必为 0x16（TLS ClientHello 记录类型）。
    /// 这是确定性的：不并发竞争 accept，裸服务端读到的就是唯一连接。
    #[tokio::test]
    async fn wire_is_not_plaintext() {
        let material = TlsMaterial::generate_demo().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // 裸服务端：只 accept + 读前 5 字节，永不应答 TLS 握手
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            tcp.read_exact(&mut buf).await.unwrap();
            buf
        });

        // TLS 客户端拨号并开始握手（对方不应答，握手最终超时/失败——
        // 我们关心的是它**写上线路的第一批字节**）
        let connector = TlsConnector::new(material.client_config().unwrap()).unwrap();
        let client =
            tokio::spawn(async move { connector.connect(&addr.to_string(), "localhost").await });

        let head = server.await.unwrap();
        // 0x16 = TLS handshake 记录类型（ClientHello），
        // 后随 0x03 0x01~0x03 的版本号——帧协议的明文头不可能长这样
        assert_eq!(head[0], 0x16, "链路第一字节应是 TLS ClientHello（0x16）而非明文帧头");
        assert_eq!(head[1], 0x03, "TLS 记录版本号主字节应为 0x03");
        client.abort(); // 裸服务端不应答，握手注定挂起，收尾时中止
    }

    /// TLS 流直接进网关：心跳/写 actor/优雅关闭在加密链路上照常工作
    #[tokio::test]
    async fn gateway_over_tls_keeps_heartbeat_alive() {
        use crate::gateway::run_gateway_connection;
        use crate::shutdown::shutdown_channel;

        let material = TlsMaterial::generate_demo().unwrap();
        let acceptor = TlsAcceptor::new(material.server_config().unwrap()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // 服务端：TLS accept → 网关（服务端心跳策略：自动回 Pong）。
        // 服务端业务层只需**持有并排空**入站通道——服务端模式下 Ping
        // 被网关就地应答，业务层本来也看不到什么，但 Receiver 决不能
        // 立刻 drop（通道关闭会把网关误杀）。
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let tls = acceptor.accept_stream(tcp).await.unwrap();
            let (server_tx, mut server_rx) = tokio::sync::mpsc::channel(16);
            // ShutdownTx 必须活着：过早 drop 会静默触发关停（docs/20 §4.2 老坑）
            let (_server_shutdown_tx, server_shutdown_rx) = shutdown_channel();
            let gateway = tokio::spawn(run_gateway_connection(
                tls,
                crate::gateway::GatewayConfig::default(),
                server_tx,
                server_shutdown_rx,
            ));
            while server_rx.recv().await.is_some() {} // 排空入站（否则反压会阻塞读循环）
            gateway.await.unwrap().unwrap();
        });

        // 客户端：TLS connect → 网关（客户端策略：50ms 心跳发 Ping）
        let connector = TlsConnector::new(material.client_config().unwrap()).unwrap();
        let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(16);
        let config = crate::gateway::GatewayConfig {
            heartbeat: crate::gateway::HeartbeatPolicy::Client {
                interval: std::time::Duration::from_millis(50),
            },
            ..crate::gateway::GatewayConfig::default()
        };
        let tls = connector.connect_stream(&addr.to_string(), "localhost").await.unwrap();
        // 同上：客户端的 ShutdownTx 也要活过整个测试
        let (_client_shutdown_tx, client_shutdown_rx) = shutdown_channel();
        let client = tokio::spawn(run_gateway_connection(tls, config, inbound_tx, client_shutdown_rx));

        // 客户端业务层应看到服务端回的 Pong（ack = seq+1）——
        // 心跳、网关、帧协议全部工作在 TLS 之上
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), inbound_rx.recv())
            .await
            .expect("2s 内应收到心跳 Pong")
            .expect("客户端网关存活");
        assert_eq!(event.frame.cmd, Cmd::Pong);
        assert_eq!(event.frame.ack, 2);

        client.abort();
        server.abort();
    }
}
