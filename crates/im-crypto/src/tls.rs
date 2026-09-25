//! TLS 材料管理（阶段 12）。
//!
//! 本模块只负责**材料**：生成证书、构造 rustls 配置；
//! 把 `TcpStream` 包成 TLS 流的**装配**在 `im-transport::tls`——
//! 加密层与传输层各管一半，是这个 workspace 一贯的职责切分
//! （对照：FrameDecoder 造帧，Connection 搬运帧）。
//!
//! # 证书拓扑：真实 PKI 的微缩版
//!
//! ```text
//!   自签 CA（rust-im demo CA，is_ca = Ca）
//!        │ 签发
//!        ▼
//!   服务端叶子证书（SAN: localhost, 127.0.0.1）
//! ```
//!
//! 客户端信任锚只放 **CA 公钥**，叶子证书由 CA 签发——
//! 这与「客户端直接信任单张自签证书」有本质区别：
//! 叶子私钥泄露时可以只轮换叶子（CA 不动），信任拓扑是对的。
//! 生产环境把 CA 换成 Let's Encrypt / 内部 KMS 即可，
//! 本函数的存在意义是让 TLS 链路**不依赖任何外部文件**就能跑起来。
//!
//! # 安全口径（诚实边界）
//!
//! 这是**演示/测试口径**的密钥材料：进程内生成、不落盘、不接 KMS。
//! 足够证明 TLS 链路（握手、证书校验、记录层加解密）真实工作，
//! 不等于生产密钥管理——后者在 docs/18 §六 有专门讨论。

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use rustls::crypto::ring as ring_provider;

use crate::error::CryptoError;

/// 一套可用的 TLS 材料：CA 证书 + 服务端叶子证书/私钥。
///
/// 惰性存储 DER（rustls 的原生格式，PEM 只在需要落盘/人工检查时转换），
/// `server_config` / `client_config` 可重复调用——配置构造是纯函数。
#[derive(Debug, Clone)]
pub struct TlsMaterial {
    /// CA 自签证书（客户端信任锚）。
    ca_cert_der: CertificateDer<'static>,
    /// 服务端叶子证书（SAN: `localhost` + `127.0.0.1`）。
    server_cert_der: CertificateDer<'static>,
    /// 服务端叶子私钥（PKCS#8 DER）。
    server_key_der: PrivateKeyDer<'static>,
}

impl TlsMaterial {
    /// 生成一套演示材料：自签 CA → 由 CA 签发的服务端叶子证书。
    ///
    /// SAN（Subject Alternative Name）同时含 DNS `localhost` 与 IP
    /// `127.0.0.1`——rustls 客户端按 ServerName 严格匹配 SAN，
    /// 两者都写，直连 IP 和写主机名的两种连法都能通过校验。
    ///
    /// # Errors
    ///
    /// 密钥生成或证书编码失败（源自 rcgen / ring 底层）时返回
    /// [`CryptoError::Rcgen`]。
    pub fn generate_demo() -> Result<Self, CryptoError> {
        // ── 1. CA：自签 + 无约束基本限制（真实 CA 会加 pathlen 约束）──
        let ca_key = rcgen::KeyPair::generate()?;
        let mut ca_params =
            rcgen::CertificateParams::new(vec!["rust-im demo CA".into()]).map_err(Box::new)?;
        ca_params.distinguished_name.push(rcgen::DnType::CommonName, "rust-im demo CA");
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key)?;

        // ── 2. 服务端叶子：CA 签发，SAN 覆盖 localhost 与环回 IP ──
        let server_key = rcgen::KeyPair::generate()?;
        let mut server_params = rcgen::CertificateParams::new(vec![
            "localhost".into(),
            "127.0.0.1".into(),
        ])
        .map_err(Box::new)?;
        server_params.distinguished_name.push(rcgen::DnType::CommonName, "localhost");
        let server_cert = server_params.signed_by(&ca_cert, &ca_key)?;

        Ok(Self {
            ca_cert_der: CertificateDer::from(ca_cert.der()),
            server_cert_der: CertificateDer::from(server_cert.der()),
            server_key_der: PrivateKeyDer::Pkcs8(server_key.serialized_der().to_vec().into()),
        })
    }

    /// CA 证书的 PEM（信任分发用：发给每个客户端的 `ca.pem`）。
    #[must_use]
    pub fn ca_cert_pem(&self) -> String {
        rustls::pki_types::pem::PemObject::to_pem(&self.ca_cert_der)
    }

    /// 服务端叶子证书的 PEM。
    #[must_use]
    pub fn server_cert_pem(&self) -> String {
        rustls::pki_types::pem::PemObject::to_pem(&self.server_cert_der)
    }

    /// 构造服务端配置：单证书 + 无客户端认证（mTLS 在 docs/18 §六 讨论）。
    ///
    /// 显式钉死 ring provider（`builder_with_provider`）而不是依赖
    /// 进程级默认——库不该偷偷调用 `install_default` 污染宿主进程的全局状态
    /// （若宿主已装了别的 provider，覆盖是静默 bug）。
    ///
    /// # Errors
    ///
    /// 证书/私钥不匹配或解析失败时返回 [`CryptoError::Rustls`]。
    pub fn server_config(&self) -> Result<ServerConfig, CryptoError> {
        ServerConfig::builder_with_provider(Arc::new(ring_provider::default_provider()))
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(
                vec![self.server_cert_der.clone()],
                self.server_key_der.clone_key(),
            )
            .map_err(CryptoError::Rustls)
    }

    /// 构造客户端配置：信任锚只有我们的 CA（默认系统根证书全部不信）。
    ///
    /// 「只信自己的 CA」是 IM 传输层的正确默认：公共根证书对私有服务
    /// 没有意义，反而扩大攻击面（任何公共 CA 签发的证书都能 MITM）。
    ///
    /// # Errors
    ///
    /// CA 证书加入信任锚失败（解析错误）时返回 [`CryptoError::Rustls`]。
    pub fn client_config(&self) -> Result<ClientConfig, CryptoError> {
        let mut roots = RootCertStore::empty();
        roots
            .add(self.ca_cert_der.clone())
            .map_err(|e| CryptoError::Rustls(rustls::Error::General(e.to_string())))?;
        ClientConfig::builder_with_provider(Arc::new(ring_provider::default_provider()))
            .with_safe_defaults()
            .with_root_certificates(roots)
            .with_no_client_auth()
            .map_err(CryptoError::Rustls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 材料生成 + 双端配置构造全链路可用（进程内生成，无外部依赖）
    #[test]
    fn generate_and_build_configs() {
        let material = TlsMaterial::generate_demo().unwrap();
        assert!(material.server_config().is_ok(), "服务端配置应构造成功");
        assert!(material.client_config().is_ok(), "客户端配置应构造成功");
    }

    /// PEM 输出带标准 armor（可人工检查/落盘分发）
    #[test]
    fn pem_has_armor() {
        let material = TlsMaterial::generate_demo().unwrap();
        let ca_pem = material.ca_cert_pem();
        assert!(ca_pem.starts_with("-----BEGIN CERTIFICATE-----"), "PEM 应有标准头: {ca_pem}");
        assert!(ca_pem.contains("-----END CERTIFICATE-----"), "PEM 应有标准尾");
    }

    /// 两次生成的材料互不相同（密钥随机性 Sanity check）
    #[test]
    fn materials_are_random() {
        let a = TlsMaterial::generate_demo().unwrap();
        let b = TlsMaterial::generate_demo().unwrap();
        assert_ne!(a.ca_cert_pem(), b.ca_cert_pem(), "CA 证书应逐次随机");
    }
}
