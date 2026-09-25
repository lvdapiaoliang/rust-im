//! # e2ee：端到端加密（Signal 协议学习实现）
//!
//! 两个子模块拼出一次完整的 E2EE 会话生命周期：
//!
//! ```text
//! ① Bob 上架          ② Alice 开聊              ③ 之后每条消息
//! ───────────         ────────────────           ──────────────
//! 生成 IK/SPK/OPK  →  拉取 PreKeyBundle       →  双棘轮自动接管：
//! 上传服务器          X3DH 单方面算出 SK          对称棘轮每条换密钥，
//! （x3dh::respond      发首条消息带上协商材料      DH 棘轮每轮洗根密钥
//!    待 Bob 上线）     Bob respond 算出同一 SK     （ratchet）
//! ```
//!
//! 协议编排（时序、状态机、跳过缓存）全部手写；
//! 密码学原语（X25519/Ed25519/HKDF/HMAC/AES-GCM）用审计过的库。
//!
//! 完整设计文档：`docs/18-tls-e2ee.md`。

pub mod ratchet;
pub mod x3dh;

pub use ratchet::{MAX_SKIP, RatchetMessage, RatchetState};
pub use x3dh::{
    IdentityKeyPair, IdentityPublicKey, OneTimePreKey, PreKeyBundle, SessionKey, SignedPreKey,
    X3dhInitiation, initiate, respond,
};
