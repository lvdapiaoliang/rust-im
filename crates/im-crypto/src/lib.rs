//! # im-crypto：加密层
//!
//! 职责（阶段 12 实现，阶段 13 扩展 QUIC 配置）：
//! - **TLS 材料**（[`tls`]）：自签 CA + 服务端叶子证书（真实 PKI 拓扑的
//!   微缩版），构造 rustls 双端配置（TLS over TCP 与 QUIC 各一套，
//!   QUIC 那套钉 TLS 1.3 + ALPN）——流包装的装配在 `im-transport::tls`，
//!   QUIC 装配在 `im-transport::quic`；
//! - **E2EE 端到端加密**（`e2ee`）：Signal 协议的协议编排层手写。
//!   密码学原语（X25519/HKDF/AES-GCM/Ed25519）用审计过的库，
//!   与阶段 9 手写 HMAC 是同一条边界：造轮子的价值在协议层理解，
//!   不在重写分组密码：
//!   - X3DH 密钥协商（首次建立共享密钥）
//!   - 双棘轮 Double Ratchet（前向保密 + 乱序消息解密）
//!
//! 学习文档：`docs/18-tls-e2ee.md`（阶段 12 编写）

pub mod e2ee;
pub mod error;
pub mod tls;

pub use e2ee::{RatchetMessage, RatchetState, SessionKey, X3dhInitiation, initiate, respond};
pub use error::CryptoError;
pub use tls::{QUIC_ALPN, TlsMaterial};

/// 密钥字节数（X25519 私钥长度，阶段 0 为分层预留，阶段 12 起实际使用）
pub const KEY_LEN: usize = 32;

#[cfg(test)]
mod tests {
    #[test]
    fn key_len_is_32() {
        assert_eq!(super::KEY_LEN, 32);
    }
}
