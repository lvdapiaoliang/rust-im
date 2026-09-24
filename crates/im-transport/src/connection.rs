//! 连接层：把裸 `TcpStream` 升级为「帧流」。
//!
//! [`Connection`] 只做一件事：让调用者用**帧**而不是字节思考。
//!
//! - [`Connection::read_frame`]：读出一个完整帧（粘包/半包在内部消化），
//!   `Ok(None)` 表示对端正常关闭（TCP FIN）；
//! - [`Connection::write_frame`]：编码并写出一个帧（复用内部缓冲，零额外分配）。
//!
//! # 与 echo（阶段 0）的对照
//!
//! echo 循环搬运的是「字节切片」；这里交换的是「帧」——
//! 阶段 1 的 `FrameDecoder` / `Frame::encode_into` 终于接上了真正的 TCP。
//!
//! # 半部拆分
//!
//! 读与写往往属于不同的 task（网关：读循环 + 写 actor，见 [`crate::gateway`]），
//! [`Connection::into_split`] 按**所有权**把连接一分为二——
//! 这是「split」模式在类型系统里的表达（更多背景见 `echo.rs` 顶部文档）。

use std::collections::VecDeque;
use std::net::SocketAddr;

use bytes::BytesMut;
use im_protocol::{DEFAULT_MAX_FRAME_LEN, Frame, FrameDecoder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use crate::error::TransportError;

/// 单次 read 的最大搬运量。
///
/// 帧协议决定了「读到多少字节」与「解出多少帧」可以不同步：
/// 一次 read 可能是半帧（攒在解码器里等下次），也可能压着好几帧（粘包）。
/// 8 KB 足够单次系统调用搬完绝大多数 IM 帧。
const READ_BUF_SIZE: usize = 8 * 1024;

/// 一条 TCP 连接上的帧流。
///
/// 内部三件套：
/// - `decoder`：阶段 1 的增量解码器（跨 read 保留半帧进度）；
/// - `pending`：已解码、待交付的帧队列（一次 read 解出多帧时先攒着）；
/// - `write_buf`：编码缓冲区，跨 `write_frame` 调用复用（零分配热路径）。
///
/// # Examples
///
/// ```
/// use bytes::Bytes;
/// use im_protocol::{Cmd, Frame};
/// # use im_transport::Connection;
/// # async fn demo(mut conn: Connection) -> Result<(), Box<dyn std::error::Error>> {
/// // 写一帧
/// conn.write_frame(&Frame::new(Cmd::Msg, 1, 0, Bytes::from_static(b"hi"))).await?;
/// // 读一帧；Ok(None) = 对端关闭
/// if let Some(frame) = conn.read_frame().await? {
///     assert_eq!(frame.cmd, Cmd::Msg);
/// }
/// # Ok(()) }
/// ```
pub struct Connection {
    stream: TcpStream,
    decoder: FrameDecoder,
    read_buf: Box<[u8; READ_BUF_SIZE]>,
    write_buf: BytesMut,
    /// 已解码、尚未交付的帧（粘包：一次 read 可能解出多帧）。
    pending: VecDeque<Frame>,
}

impl Connection {
    /// 包装一条已建立的连接（默认单帧上限，见 `DEFAULT_MAX_FRAME_LEN`）。
    #[must_use]
    pub fn new(stream: TcpStream) -> Self {
        Self::with_max_frame_len(stream, DEFAULT_MAX_FRAME_LEN)
    }

    /// 包装一条已建立的连接，并指定单帧上限。
    #[must_use]
    pub fn with_max_frame_len(stream: TcpStream, max_frame_len: usize) -> Self {
        Self {
            stream,
            decoder: FrameDecoder::with_max_frame_len(max_frame_len),
            read_buf: Box::new([0u8; READ_BUF_SIZE]),
            write_buf: BytesMut::new(),
            pending: VecDeque::new(),
        }
    }

    /// 主动连接到 `addr`（如 `"127.0.0.1:8080"`）。
    ///
    /// # Errors
    ///
    /// 连接失败（服务不可达、拒绝连接等）时返回 [`TransportError::Io`]。
    pub async fn connect(addr: &str) -> Result<Self, TransportError> {
        let stream = TcpStream::connect(addr).await?;
        Ok(Self::new(stream))
    }

    /// 对端地址（日志与连接管理用）。
    ///
    /// # Errors
    ///
    /// 仅在 socket 已进入异常状态时失败。
    pub fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        self.stream.peer_addr()
    }

    /// 读出一个完整帧；`Ok(None)` 表示对端正常关闭（读到 FIN）。
    ///
    /// 粘包与半包对调用方完全透明：
    /// 半帧时内部继续等待字节（本调用保持 pending），
    /// 粘包时多出的帧留在 `pending` 队列，下次调用立即返回。
    ///
    /// # Errors
    ///
    /// IO 故障（[`TransportError::Io`]）或字节流违反协议
    /// （[`TransportError::Protocol`]，此时连接应立即丢弃）。
    pub async fn read_frame(&mut self) -> Result<Option<Frame>, TransportError> {
        Self::read_one(&mut self.stream, &mut self.decoder, &mut self.read_buf, &mut self.pending)
            .await
    }

    /// 编码并写出一个帧。
    ///
    /// 编码进入复用的 `write_buf` 后一次 `write_all` 写出，并显式 flush——
    /// `TcpStream` 没有用户态缓冲，flush 实际是 no-op，
    /// 但保持「任何 `AsyncWrite` 实现都正确」的通用语义。
    ///
    /// # Errors
    ///
    /// 写入失败（连接已断等）时返回 [`TransportError::Io`]。
    pub async fn write_frame(&mut self, frame: &Frame) -> Result<(), TransportError> {
        Self::write_one(&mut self.stream, &mut self.write_buf, frame).await
    }

    /// 把连接拆成读、写两个**独立所有权**的半部。
    ///
    /// 拆分后两者无共享状态，可安全地交给不同 task——
    /// 网关的标准布局：读循环 task + 写 actor task（见 [`crate::gateway`]）。
    #[must_use]
    pub fn into_split(self) -> (ReadHalf, WriteHalf) {
        let (read, write) = self.stream.into_split();
        (
            ReadHalf {
                stream: read,
                decoder: self.decoder,
                read_buf: self.read_buf,
                pending: self.pending,
            },
            WriteHalf { stream: write, write_buf: self.write_buf },
        )
    }

    /// `read_frame` 的共享实现（`Connection` 与 `ReadHalf` 逻辑完全一致）。
    async fn read_one(
        stream: &mut TcpStream,
        decoder: &mut FrameDecoder,
        read_buf: &mut [u8; READ_BUF_SIZE],
        pending: &mut VecDeque<Frame>,
    ) -> Result<Option<Frame>, TransportError> {
        loop {
            // 1. 先交付上次多解出来的帧（粘包队列）
            if let Some(frame) = pending.pop_front() {
                return Ok(Some(frame));
            }
            // 2. 队列空 → 需要更多字节；read 返回 0 = 对端 FIN
            let n = stream.read(read_buf).await?;
            if n == 0 {
                return Ok(None);
            }
            // 3. 喂给增量解码器：可能 0 帧（半包）、1 帧、或多帧（粘包）
            let frames = decoder.decode(&read_buf[..n])?;
            pending.extend(frames);
        }
    }

    /// `write_frame` 的共享实现。
    async fn write_one(
        stream: &mut TcpStream,
        write_buf: &mut BytesMut,
        frame: &Frame,
    ) -> Result<(), TransportError> {
        // 先清空：若上次写失败残留了数据，追加会造成重复帧
        write_buf.clear();
        frame.encode_into(write_buf);
        stream.write_all(write_buf).await?;
        write_buf.clear();
        stream.flush().await?;
        Ok(())
    }
}

/// 连接的读半部：只能读帧（由 [`Connection::into_split`] 产生）。
pub struct ReadHalf {
    stream: OwnedReadHalf,
    decoder: FrameDecoder,
    read_buf: Box<[u8; READ_BUF_SIZE]>,
    pending: VecDeque<Frame>,
}

impl ReadHalf {
    /// 读出一个完整帧；`Ok(None)` 表示对端正常关闭。
    ///
    /// 语义与 [`Connection::read_frame`] 完全一致——半部只是所有权的拆分，
    /// 不是行为的拆分。
    ///
    /// # Errors
    ///
    /// 同 [`Connection::read_frame`]。
    pub async fn read_frame(&mut self) -> Result<Option<Frame>, TransportError> {
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Ok(Some(frame));
            }
            let n = self.stream.read(&mut self.read_buf[..]).await?;
            if n == 0 {
                return Ok(None);
            }
            let frames = self.decoder.decode(&self.read_buf[..n])?;
            self.pending.extend(frames);
        }
    }
}

/// 连接的写半部：只能写帧（由 [`Connection::into_split`] 产生）。
pub struct WriteHalf {
    stream: OwnedWriteHalf,
    write_buf: BytesMut,
}

impl WriteHalf {
    /// 编码并写出一个帧。
    ///
    /// 语义与 [`Connection::write_frame`] 完全一致。
    ///
    /// # Errors
    ///
    /// 同 [`Connection::write_frame`]。
    pub async fn write_frame(&mut self, frame: &Frame) -> Result<(), TransportError> {
        self.write_buf.clear();
        frame.encode_into(&mut self.write_buf);
        self.stream.write_all(&self.write_buf).await?;
        self.write_buf.clear();
        self.stream.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use im_protocol::Cmd;
    use tokio::net::TcpListener;

    /// 真实 TCP 上的帧往返：`write_frame` → `read_frame`
    #[tokio::test]
    async fn frame_roundtrip_over_tcp() {
        let listener = TcpStreamEchoListener::spawn().await;
        let mut client = Connection::connect(&listener.addr).await.unwrap();

        let frame = Frame::new(Cmd::Msg, 42, 0, Bytes::from_static(b"hello transport"));
        client.write_frame(&frame).await.unwrap();

        assert_eq!(client.read_frame().await.unwrap().unwrap(), frame);
        listener.handle.await.unwrap().unwrap();
    }

    /// 对端 drop 连接后，`read_frame` 返回 `Ok(None)`（EOF 语义）
    #[tokio::test]
    async fn read_frame_reports_eof_on_peer_close() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            // 只 accept、不读写：client drop 后应读到 EOF
            let (stream, _) = listener.accept().await.unwrap();
            let mut conn = Connection::new(stream);
            assert!(conn.read_frame().await.unwrap().is_none());
        });

        let client = Connection::connect(&addr.to_string()).await.unwrap();
        drop(client); // 直接关闭
        server.await.unwrap();
    }

    /// 半部拆分：读循环与写回由不同代码路径协作完成
    #[tokio::test]
    async fn split_halves_cooperate() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = Connection::new(stream).into_split();
            // 读一帧、原样写回——读半部与写半部各自独占，无锁无共享
            let frame = reader.read_frame().await.unwrap().unwrap();
            writer.write_frame(&frame).await.unwrap();
        });

        let mut client = Connection::connect(&addr.to_string()).await.unwrap();
        let frame = Frame::new(Cmd::Ping, 1, 0, Bytes::new());
        client.write_frame(&frame).await.unwrap();
        assert_eq!(client.read_frame().await.unwrap().unwrap(), frame);

        server.await.unwrap();
    }

    /// 测试脚手架：单连接 echo 服务器（收一帧回一帧）
    struct TcpStreamEchoListener {
        addr: String,
        handle: tokio::task::JoinHandle<Result<(), TransportError>>,
    }

    impl TcpStreamEchoListener {
        async fn spawn() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let handle = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let mut conn = Connection::new(stream);
                let frame = conn.read_frame().await?.expect("应读到一帧");
                conn.write_frame(&frame).await?;
                Ok(())
            });
            Self { addr, handle }
        }
    }
}
