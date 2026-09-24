//! 帧发送端抽象（[`FrameSink`]）：会话核心与传输实现的解耦点。
//!
//! 阶段 3 的会话层直接持有 `im_transport::ConnectionHandle`（TCP 网关的
//! 写 actor 通道）——那时「会话」与「TCP」是一一对应的。阶段 5 引入
//! Web 接入后，同一段会话逻辑（握手/路由/离线/同步）要同时服务
//! TCP（二进制协议）与 WS（JSON 信封）两种传输：**会话核心只关心
//! 「把一帧交给对端」，不关心帧最终怎么编码上线**。
//!
//! ```text
//!   会话核心（session.rs：SessionState / handle_frame / deliver）
//!        │  只依赖本 trait（依赖倒置：细节依赖抽象）
//!        ▼
//!   FrameSink ◀── ConnectionHandle 适配（TCP 写 actor 通道）
//!        ▲
//!        └── WS 出站通道适配（阶段 5 web 模块：Frame → JSON 信封写出）
//! ```
//!
//! # 落地的设计模式
//!
//! - **依赖倒置（DIP）**：高层（会话核心）定义接口，低层（各传输）实现
//!   接口——与 [`crate::session::Authenticator`] 同一手法，只是这次
//!   抽象的是「出站 IO」而非「认证策略」；
//! - **适配器模式**：`ConnectionHandle` 的固有异步方法通过装箱 future
//!   摆进 trait——`impl FrameSink for ConnectionHandle` 就是适配层；
//! - **trait 对象 + Arc**：`async fn` 直写 trait 无法作 `dyn`（编译器
//!   尚不支持动态派发的 async 方法），返回 `Pin<Box<dyn Future>>` 是
//!   标准换法——代价是每帧一次堆分配，换来路由表里统一存
//!   `Arc<dyn FrameSink>`（对 64KB 级的帧处理成本而言可忽略）。

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;

use im_protocol::Frame;
use im_transport::{ConnectionHandle, TransportError};

/// [`FrameSink::send`] 的返回形态：装箱 future，让 trait 可作 `dyn` 对象。
pub type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<(), TransportError>> + Send + 'a>>;

/// 帧发送端：向「这条连接的对端」送出一帧的唯一抽象。
///
/// 实现方必须保证：多个 task 并发 `send` 不会交错帧字节（TCP 侧由写
/// actor 独占写半部保证，WS 侧由出站单 task 保证）。
pub trait FrameSink: Send + Sync + Debug {
    /// 异步发送一帧。
    ///
    /// # Errors
    ///
    /// 连接已死（对端关闭/写通道退役）时返回 [`TransportError::Closed`]，
    /// 调用方（如 [`crate::session::Sessions::deliver`]）据此降级为离线入队。
    fn send(&self, frame: Frame) -> SendFuture<'_>;

    /// 直通一段传输层原生文本（阶段 6 事件推送专用）。
    ///
    /// 需求背景：好友请求/被接受等**服务端主动事件**不是协议帧
    /// （`im-protocol` 没有 `Event` 命令字——业务事件不该膨胀二进制协议），
    /// 它们只在 Web 接入路径存在，形态是 JSON 信封文本。
    /// TCP 路径（TUI）没有事件语义，显式拒绝而非静默吞掉——
    /// 调用方（REST 处理器）据此知道「这个用户收不到事件」。
    ///
    /// 同一 trait 承载「帧」与「原生文本」两条通道，是适配器模式的
    /// 一次扩展：会话核心与 REST 层都只认 `FrameSink`，不必关心
    /// 对面是浏览器还是 TUI。
    ///
    /// # Errors
    ///
    /// 传输不支持文本直通（TCP 路径）或连接已死时返回
    /// [`TransportError::Closed`]；事件是 best-effort，调用方不重试。
    fn send_text(&self, _text: String) -> SendFuture<'_> {
        // 默认实现 = 本传输不支持：装箱一个立即失败的 future
        Box::pin(async { Err(TransportError::Closed) })
    }
}

impl FrameSink for ConnectionHandle {
    fn send(&self, frame: Frame) -> SendFuture<'_> {
        // 适配器：固有异步方法 → 装箱 future（借用 self，生命周期自然对齐）
        Box::pin(ConnectionHandle::send(self, frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use im_protocol::Cmd;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    /// 测试用最小 sink：出站通道直出（WS 路径的形状预演）。
    #[derive(Debug)]
    struct ChannelSink(mpsc::Sender<Frame>);

    impl FrameSink for ChannelSink {
        fn send(&self, frame: Frame) -> SendFuture<'_> {
            Box::pin(async move { self.0.send(frame).await.map_err(|_| TransportError::Closed) })
        }
    }

    /// 自定义 sink 经 trait 送出的帧应原样到达出站通道
    #[tokio::test]
    async fn custom_sink_delivers_frame_through_trait() {
        let (tx, mut rx) = mpsc::channel(4);
        let sink = ChannelSink(tx);

        let frame = Frame::new(Cmd::Msg, 1, 0, Bytes::from_static(b"via-trait"));
        sink.send(frame.clone()).await.expect("通道存活，发送应成功");

        let got = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("2s 内应收到帧")
            .expect("sink 存活");
        assert_eq!(got, frame);
    }

    /// 接收端全部 drop 后：send 报 `Closed`（deliver 降级离线的依据）
    #[tokio::test]
    async fn sink_reports_closed_when_receiver_gone() {
        let (tx, rx) = mpsc::channel(4);
        let sink = ChannelSink(tx);
        drop(rx);

        let result = sink.send(Frame::new(Cmd::Msg, 1, 0, Bytes::new())).await;
        assert!(matches!(result, Err(TransportError::Closed)));
    }

    /// 默认 `send_text` = 不支持：TCP 适配器（未覆写）应拒绝文本直通，
    /// 调用方据此知道该用户收不到 Web 事件（阶段 6 语义）
    #[tokio::test]
    async fn default_send_text_rejects_with_closed() {
        let (tx, _rx) = mpsc::channel(4);
        let sink = ChannelSink(tx); // 未覆写 send_text：走默认实现

        let result = sink.send_text("{}".to_string()).await;
        assert!(matches!(result, Err(TransportError::Closed)));
    }

    /// TCP 路径的适配器：`ConnectionHandle` 实现的 trait 与固有方法行为一致
    #[tokio::test]
    async fn connection_handle_adapter_matches_inherent_send() {
        use im_transport::GatewayConfig;

        // 起一个真实网关：入站帧直接丢弃，只为拿到发送句柄
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (inbound_tx, mut inbound_rx) = mpsc::channel(4);
        let (shutdown_tx, shutdown_rx) = im_transport::shutdown_channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_handle, task) = im_transport::spawn_gateway(
                stream,
                GatewayConfig::default(),
                inbound_tx,
                shutdown_rx,
            );
            let _ = task.await;
        });

        // 客户端裸连接：发一帧 Msg 换取入站事件附带的 handle
        // （不能用 Ping——Server 心跳策略会就地应答，帧进不了入站通道）
        let mut client = im_transport::Connection::connect(&addr.to_string()).await.unwrap();
        client.write_frame(&Frame::new(Cmd::Msg, 1, 0, Bytes::new())).await.unwrap();

        let event = timeout(Duration::from_secs(2), inbound_rx.recv())
            .await
            .expect("2s 内应收到入站帧")
            .expect("网关存活");

        // 经 trait 发送（而非固有方法）：客户端应原样收到
        let frame = Frame::new(Cmd::MsgAck, 9, 2, Bytes::from_static(b"adapted"));
        FrameSink::send(&event.handle, frame.clone()).await.expect("网关存活，trait 发送应成功");

        let got = timeout(Duration::from_secs(2), client.read_frame())
            .await
            .expect("2s 内应收到帧")
            .expect("连接正常")
            .expect("连接未关闭");
        assert_eq!(got, frame);

        shutdown_tx.trigger();
    }
}
