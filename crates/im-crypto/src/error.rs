//! 加密层错误类型。
//!
//! 与传输层（[`TransportError`](im_transport::TransportError)）的分工：
//! 这里只覆盖**密码学材料与协议自身**的失败——证书生成/解析、
//! rustls 配置构造、E2EE 密钥协商/棘轮状态违规；
//! 网络读写失败仍然归属传输层。

/// 加密层错误：材料生成、配置构造、E2EE 协议失败的统一表达。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CryptoError {
    /// 证书生成/签名失败（rcgen / ring 底层）。
    #[error("certificate generation failed: {0}")]
    Rcgen(#[from] rcgen::Error),

    /// rustls 配置构造或证书解析失败。
    #[error("rustls error: {0}")]
    Rustls(#[from] rustls::Error),

    /// E2EE：对方预密钥包签名校验失败（身份存疑，应中止会话建立）。
    #[error("prekey signature verification failed")]
    BadSignature,

    /// E2EE：棘轮状态违规——解密乱序/无法定位消息密钥时返回。
    ///
    /// 棘轮设计目标就是「不因乱序而永久失步」，收到它通常意味着
    /// 实现缺陷或密钥状态被外部破坏。
    #[error("ratchet state violation: {0}")]
    Ratchet(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Display 输出可读（日志友好性检查）
    #[test]
    fn display_is_informative() {
        let e = CryptoError::BadSignature;
        assert!(e.to_string().contains("signature"));
    }
}
