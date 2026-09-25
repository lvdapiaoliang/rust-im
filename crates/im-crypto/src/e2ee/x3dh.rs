//! X3DH（Extended Triple Diffie-Hellman）密钥协商：E2EE 会话的「第一次握手」。
//!
//! # 它解决什么
//!
//! Alice 想和 Bob 开启端到端加密会话，但 Bob **此刻可能不在线**——
//! 普通的交互式密钥交换（TLS 那种）做不到。X3DH 让 Bob 预先把一捆
//! 「预密钥」上传服务器，Alice 拉下来后**单方面**就能算出双方共享的
//! 会话根密钥 SK，Bob 上线后用同样的输入算出同一个 SK。
//!
//! # 密钥清单（Signal 规范）
//!
//! | 密钥 | 属主 | 生命周期 | 用途 |
//! |------|------|----------|------|
//! | IK | 双方 | 长期 | 身份密钥（X25519 做 DH + 指纹展示） |
//! | SPK | Bob | 中期，可轮换 | 签名预密钥（IK 的 Ed25519 签名背书，防换预密钥攻击） |
//! | OPK | Bob | 一次性 | 一次性预密钥（用一次即焚，提供额外的前向保密维度） |
//! | EK | Alice | 一次性 | 发起临时密钥 |
//!
//! # 三（四）重 DH 与域分离
//!
//! ```text
//! SK = HKDF(F ‖ DH1 ‖ DH2 ‖ DH3 ‖ [DH4])
//! DH1 = DH(IK_A, SPK_B)   身份绑定：SK 与双方长期身份挂钩
//! DH2 = DH(EK_A, IK_B)    Alice 知道对方是 IK_B（对 Bob 认证）
//! DH3 = DH(EK_A, SPK_B)   前向保密：EK 一次性
//! DH4 = DH(EK_A, OPK_B)   Bob 侧前向保密（OPK 存在时）
//! ```
//!
//! `F = 0xFF × 32` 是规范规定的域分离前缀：X25519 输出可能以连续
//! 0 字节开头，固定前缀防止 DH 输出被误读成序列化结构。
//!
//! # 本实现的学习版简化（诚实边界）
//!
//! - **信任模型**：Bob 不校验 Alice 的 IK_A 真伪（生产里通过 SAFETY
//!   NUMBER/指纹比对或信任链；学习版按 TOFU——首次使用即信任，
//!   MITM 风险在 docs/18 §四 讨论）；
//! - SPK 的签名**必须**校验（否则服务器可给 Alice 掉包假预密钥——
//!   这一步是本模块 `BadSignature` 错误的全部意义）；
//! - OPK 用尽时允许降级为三重 DH（Signal 同样允许，标记为
//!   `one_time_pre_key_id: None`）。

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use rand::{CryptoRng, RngCore};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::ZeroizeOnDrop;

use crate::error::CryptoError;

/// HKDF 输出长度：一个根密钥（32 字节）。
const SK_LEN: usize = 32;
/// 域分离前缀：0xFF × 32（规范规定）。
const F: [u8; 32] = [0xFF; 32];
/// HKDF info 标签：把输出钉在「X3DH 根密钥」这个用途上。
const HKDF_INFO: &[u8] = b"im-e2ee-x3dh-v1";

// ────────────────────────────────────────────────────────────────
// 密钥对
// ────────────────────────────────────────────────────────────────

/// 身份密钥对：Ed25519（签名）+ X25519（DH）双密钥组。
///
/// 长期身份——泄露等于身份被冒充，`ZeroizeOnDrop` 保证私钥在内存里
/// 的生命周期结束时被覆写（`Box<[u8]>` 级别的彻底擦除由 dalek 内部处理）。
#[derive(ZeroizeOnDrop)]
pub struct IdentityKeyPair {
    signing: SigningKey,
    dh: StaticSecret,
}

/// 身份公钥（可自由传播）：Ed25519 验证钥 + X25519 公钥。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityPublicKey {
    verifying: VerifyingKey,
    dh: PublicKey,
}

impl IdentityKeyPair {
    /// 生成新身份（一次性动作，账号生命周期内复用）。
    #[must_use]
    pub fn generate(rng: &mut (impl CryptoRng + RngCore)) -> Self {
        Self {
            signing: SigningKey::generate(rng),
            dh: StaticSecret::random_from_rng(rng),
        }
    }

    /// 身份公钥（分发给通信对端/服务器）。
    #[must_use]
    pub fn public(&self) -> IdentityPublicKey {
        IdentityPublicKey {
            verifying: self.signing.verifying_key(),
            dh: PublicKey::from(&self.dh),
        }
    }

    /// 用身份私钥签名（SPK 背书用）。
    fn sign(&self, msg: &[u8]) -> Signature {
        self.signing.sign(msg)
    }
}

/// 签名预密钥（Bob 侧）：由身份密钥背书的中期 DH 密钥。
///
/// 签名内容是 `key_id ‖ public`——把 key_id 拴进签名，
/// 服务器无法把 A 预密钥的签名搬到 B 预密钥上（防跨键挪用）。
pub struct SignedPreKey {
    /// 预密钥 ID（服务器寻址/轮换管理）。
    pub key_id: u32,
    secret: StaticSecret,
    /// 预密钥公钥（进 PreKeyBundle）。
    pub public: PublicKey,
    /// 身份密钥对 `key_id ‖ public` 的 Ed25519 签名。
    pub signature: Signature,
}

impl SignedPreKey {
    /// 生成并让身份密钥当场签名。
    #[must_use]
    pub fn generate(identity: &IdentityKeyPair, key_id: u32, rng: &mut (impl CryptoRng + RngCore)) -> Self {
        let secret = StaticSecret::random_from_rng(rng);
        let public = PublicKey::from(&secret);
        let mut signed = Vec::with_capacity(4 + 32);
        signed.extend_from_slice(&key_id.to_be_bytes());
        signed.extend_from_slice(public.as_bytes());
        Self { key_id, secret, public, signature: identity.sign(&signed) }
    }
}

/// 一次性预密钥（Bob 侧）：用一次即焚。
pub struct OneTimePreKey {
    /// 预密钥 ID。
    pub key_id: u32,
    secret: StaticSecret,
    /// 预密钥公钥。
    pub public: PublicKey,
}

impl OneTimePreKey {
    /// 生成新的一次性预密钥。
    #[must_use]
    pub fn generate(key_id: u32, rng: &mut (impl CryptoRng + RngCore)) -> Self {
        let secret = StaticSecret::random_from_rng(rng);
        let public = PublicKey::from(&secret);
        Self { key_id, secret, public }
    }
}

// ────────────────────────────────────────────────────────────────
// PreKeyBundle：Bob 上传服务器、Alice 拉取的「公开开胃菜」
// ────────────────────────────────────────────────────────────────

/// Bob 的预密钥捆（全部是**公钥**，可以明文放服务器）。
#[derive(Clone)]
pub struct PreKeyBundle {
    /// Bob 的身份公钥。
    pub identity: IdentityPublicKey,
    /// 签名预密钥：(key_id, 公钥, IK 签名)。
    pub signed_pre_key: (u32, PublicKey, Signature),
    /// 一次性预密钥（服务器库存用尽时为 `None`）。
    pub one_time_pre_key: Option<(u32, PublicKey)>,
}

impl SignedPreKey {
    /// 导出公开部分进 bundle（私钥留在 Bob 手里）。
    #[must_use]
    pub fn bundle_part(&self) -> (u32, PublicKey, Signature) {
        (self.key_id, self.public, self.signature)
    }
}

// ────────────────────────────────────────────────────────────────
// X3DH 本体
// ────────────────────────────────────────────────────────────────

/// X3DH 输出的共享根密钥。
///
/// 不实现 `Debug`/`Clone`——密钥材料不该能被随手打印或复制；
/// `ZeroizeOnDrop` 保证 drop 时擦除。
#[derive(ZeroizeOnDrop)]
pub struct SessionKey([u8; SK_LEN]);

impl SessionKey {
    /// 暴露原始字节（喂给双棘轮初始化的唯一出口）。
    #[must_use]
    pub fn expose(&self) -> [u8; SK_LEN] {
        self.0
    }
}

/// Alice → Bob 首条消息必须携带的协商材料（明文可见，全是公钥）。
///
/// Bob 拿着它 + 自己的私钥算出与 Alice 相同的 SK。
/// 手写紧凑编码：X25519 公钥 32B + key_id 4B，与 im-protocol 的
/// 手写字节布局同一风格（E2EE 载荷走 IM 帧 payload，能省一字节是一字节）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X3dhInitiation {
    /// Alice 的身份公钥（Bob 侧 TOFU 记住她）。
    pub initiator_identity: IdentityPublicKey,
    /// Alice 的一次性临时公钥。
    pub ephemeral: PublicKey,
    /// Alice 使用的 Bob 签名预密钥 ID。
    pub signed_pre_key_id: u32,
    /// Alice 消耗的 Bob 一次性预密钥 ID（`None` = 库存用尽走三重 DH）。
    pub one_time_pre_key_id: Option<u32>,
}

/// 发起（Alice）：拿 Bob 的公开 bundle 单方面算出会话根密钥。
///
/// # Errors
///
/// - SPK 签名校验失败（[`CryptoError::BadSignature`]）——服务器掉包、
///   传输损坏、或 Bob 轮换密钥后 bundle 过期，一律拒绝开聊。
pub fn initiate(
    initiator: &IdentityKeyPair,
    bundle: &PreKeyBundle,
    rng: &mut (impl CryptoRng + RngCore),
) -> Result<(SessionKey, X3dhInitiation), CryptoError> {
    // 1. 验 SPK 签名：用 bundle 自带的身份验证钥。签名覆盖
    //    key_id ‖ public，验签输入必须与生成时完全一致
    let (spk_id, spk_public, spk_signature) = &bundle.signed_pre_key;
    let mut signed = Vec::with_capacity(4 + 32);
    signed.extend_from_slice(&spk_id.to_be_bytes());
    signed.extend_from_slice(spk_public.as_bytes());
    bundle
        .identity
        .verifying
        .verify(&signed, spk_signature)
        .map_err(|_| CryptoError::BadSignature)?;

    // 2. 生成一次性临时密钥
    let ephemeral = StaticSecret::random_from_rng(rng);

    // 3. 三（四）重 DH：输入顺序 = 规范规定的 DH1‖DH2‖DH3‖[DH4]
    let ik_a = PublicKey::from(&initiator.dh);
    let mut ikm = Vec::with_capacity(F.len() + 4 * 32);
    ikm.extend_from_slice(&F);
    ikm.extend_from_slice(initiator.dh.diffie_hellman(spk_public).as_bytes()); // DH1
    ikm.extend_from_slice(ephemeral.diffie_hellman(&bundle.identity.dh).as_bytes()); // DH2
    ikm.extend_from_slice(ephemeral.diffie_hellman(spk_public).as_bytes()); // DH3
    if let Some((_, opk_public)) = bundle.one_time_pre_key {
        ikm.extend_from_slice(ephemeral.diffie_hellman(&opk_public).as_bytes()); // DH4
    }

    // 4. HKDF 收敛成根密钥
    let sk = derive_sk(&ikm);

    let initiation = X3dhInitiation {
        initiator_identity: initiator.public(),
        ephemeral: PublicKey::from(&ephemeral),
        signed_pre_key_id: *spk_id,
        one_time_pre_key_id: bundle.one_time_pre_key.as_ref().map(|(id, _)| *id),
    };
    Ok((SessionKey(sk), initiation))
}

/// 响应（Bob）：从首条消息的协商材料 + 自己的私钥，算出同一个根密钥。
///
/// # Errors
///
/// - `initiation` 引用的 SPK/OPK 与传入密钥的 key_id 不一致
///   （服务器错投或消息被篡改）时返回 [`CryptoError::Ratchet`]。
pub fn respond(
    responder: &IdentityKeyPair,
    spk: &SignedPreKey,
    opk: Option<&OneTimePreKey>,
    initiation: &X3dhInitiation,
) -> Result<SessionKey, CryptoError> {
    // key_id 一致性：Alice 用的必须是 Bob 现在持有的那把预密钥
    if initiation.signed_pre_key_id != spk.key_id {
        return Err(CryptoError::Ratchet(format!(
            "signed pre-key id mismatch: initiation={} local={}",
            initiation.signed_pre_key_id, spk.key_id
        )));
    }
    if let (Some(used), Some(local)) = (initiation.one_time_pre_key_id, opk) {
        if used != local.key_id {
            return Err(CryptoError::Ratchet(format!(
                "one-time pre-key id mismatch: initiation={used} local={}",
                local.key_id
            )));
        }
    }
    // Alice 用了 OPK 但 Bob 这边已消耗/不存在：两侧 DH 输入会不对称，
    // SK 必然不同——宁可拒绝也不静默产出对不上的密钥（错误比不一致便宜）
    if initiation.one_time_pre_key_id.is_some() && opk.is_none() {
        return Err(CryptoError::Ratchet("initiation 引用的 OPK 已被消耗".to_string()));
    }

    let mut ikm = Vec::with_capacity(F.len() + 4 * 32);
    ikm.extend_from_slice(&F);
    ikm.extend_from_slice(spk.secret.diffie_hellman(&initiation.initiator_identity.dh).as_bytes()); // DH1
    ikm.extend_from_slice(
        responder.dh.diffie_hellman(&initiation.ephemeral).as_bytes(),
    ); // DH2
    ikm.extend_from_slice(spk.secret.diffie_hellman(&initiation.ephemeral).as_bytes()); // DH3
    if let (Some(_), Some(local)) = (initiation.one_time_pre_key_id, opk) {
        ikm.extend_from_slice(local.secret.diffie_hellman(&initiation.ephemeral).as_bytes()); // DH4
    }

    Ok(SessionKey(derive_sk(&ikm)))
}

/// HKDF-SHA256 收敛（`Hkdf` 构造时已隐式做 extract）。
fn derive_sk(ikm: &[u8]) -> [u8; SK_LEN] {
    let hk = Hkdf::<Sha256>::new(None, ikm);
    let mut sk = [0u8; SK_LEN];
    hk.expand(HKDF_INFO, &mut sk).expect("SK_LEN 与 HKDF 输出上限匹配");
    sk
}

// ────────────────────────────────────────────────────────────────
// X3dhInitiation 编解码（进首条消息的帧 payload）
// ────────────────────────────────────────────────────────────────

impl X3dhInitiation {
    /// 编码成字节（紧凑布局，带 OPK 存在位）。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 32 + 32 + 4 + 4);
        // 标志字节：bit0 = 有 OPK
        out.push(u8::from(self.one_time_pre_key_id.is_some()));
        out.extend_from_slice(self.initiator_identity.dh.as_bytes());
        out.extend_from_slice(self.initiator_identity.verifying.as_bytes());
        out.extend_from_slice(self.ephemeral.as_bytes());
        out.extend_from_slice(&self.signed_pre_key_id.to_be_bytes());
        if let Some(id) = self.one_time_pre_key_id {
            out.extend_from_slice(&id.to_be_bytes());
        }
        out
    }

    /// 从字节解码（`encode` 的逆）。
    ///
    /// # Errors
    ///
    /// 长度不足或身份公钥字节非法时返回 [`CryptoError::Ratchet`]。
    pub fn decode(bytes: &[u8]) -> Result<Self, CryptoError> {
        let err = |what: &str| CryptoError::Ratchet(format!("x3dh initiation decode failed: {what}"));
        if bytes.len() < 1 + 32 + 32 + 32 + 4 {
            return Err(err("长度不足"));
        }
        let has_opk = bytes[0] & 1 == 1;
        let mut dh = [0u8; 32];
        dh.copy_from_slice(&bytes[1..33]);
        let mut verifying = [0u8; 32];
        verifying.copy_from_slice(&bytes[33..65]);
        let mut ephemeral = [0u8; 32];
        ephemeral.copy_from_slice(&bytes[65..97]);
        let signed_pre_key_id = u32::from_be_bytes([bytes[97], bytes[98], bytes[99], bytes[100]]);
        let one_time_pre_key_id = if has_opk {
            if bytes.len() < 101 + 4 {
                return Err(err("声明了 OPK 却没有 OPK 字段"));
            }
            Some(u32::from_be_bytes([bytes[101], bytes[102], bytes[103], bytes[104]]))
        } else {
            None
        };
        let verifying = VerifyingKey::from_bytes(&verifying).map_err(|_| err("身份验证钥字节非法"))?;
        Ok(Self {
            initiator_identity: IdentityPublicKey {
                verifying,
                dh: PublicKey::from(dh),
            },
            ephemeral: PublicKey::from(ephemeral),
            signed_pre_key_id,
            one_time_pre_key_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    /// 测试脚手架：Bob 的完整密钥货架（身份 + SPK + 一个 OPK）+ bundle。
    struct BobShelf {
        identity: IdentityKeyPair,
        spk: SignedPreKey,
        opk: OneTimePreKey,
    }

    impl BobShelf {
        fn new() -> Self {
            let mut rng = OsRng;
            let identity = IdentityKeyPair::generate(&mut rng);
            let spk = SignedPreKey::generate(&identity, 1, &mut rng);
            let opk = OneTimePreKey::generate(7, &mut rng);
            Self { identity, spk, opk }
        }

        fn bundle(&self) -> PreKeyBundle {
            PreKeyBundle {
                identity: self.identity.public(),
                signed_pre_key: self.spk.bundle_part(),
                one_time_pre_key: Some((self.opk.key_id, self.opk.public)),
            }
        }
    }

    /// 主线用例：Alice initiate，Bob respond，双方 SK 相同，且与重新协商不同
    #[test]
    fn both_sides_derive_same_session_key() {
        let mut rng = OsRng;
        let alice = IdentityKeyPair::generate(&mut rng);
        let bob = BobShelf::new();

        let (sk_a, initiation) = initiate(&alice, &bob.bundle(), &mut rng).unwrap();
        let sk_b = respond(&bob.identity, &bob.spk, Some(&bob.opk), &initiation).unwrap();

        assert_eq!(sk_a.expose(), sk_b.expose(), "X3DH 两侧必须收敛到同一 SK");
    }

    /// 两次协商的 SK 互不相同（临时密钥的随机性进入 HKDF 输入）
    #[test]
    fn session_keys_are_fresh_per_initiation() {
        let mut rng = OsRng;
        let alice = IdentityKeyPair::generate(&mut rng);
        let bob = BobShelf::new();
        let (sk1, _) = initiate(&alice, &bob.bundle(), &mut rng).unwrap();
        let (sk2, _) = initiate(&alice, &bob.bundle(), &mut rng).unwrap();
        assert_ne!(sk1.expose(), sk2.expose());
    }

    /// 安全主线：SPK 签名被换（服务器掉包攻击）必须拒之门外
    #[test]
    fn tampered_spk_signature_is_rejected() {
        let mut rng = OsRng;
        let alice = IdentityKeyPair::generate(&mut rng);
        let bob = BobShelf::new();

        // 攻击者：用自己的身份 + Bob 名义的假预密钥组装 bundle
        let mallory = IdentityKeyPair::generate(&mut rng);
        let fake_spk = SignedPreKey::generate(&mallory, 1, &mut rng);
        let forged = PreKeyBundle {
            identity: bob.identity.public(), // 冒充 Bob 的身份
            signed_pre_key: fake_spk.bundle_part(),   // 但预密钥是 Mallory 的
            one_time_pre_key: Some((7, bob.opk.public)),
        };

        assert!(matches!(initiate(&alice, &forged, &mut rng), Err(CryptoError::BadSignature)));
    }

    /// SPK 公钥被翻转一个字节：签名不再匹配，同样拒绝
    #[test]
    fn flipped_spk_public_is_rejected() {
        let mut rng = OsRng;
        let alice = IdentityKeyPair::generate(&mut rng);
        let bob = BobShelf::new();
        let (id, public, signature) = bob.spk.bundle_part();
        // 翻转公钥首字节，签名不动——验签必败（防「签名移植」攻击）
        let mut flipped = [0u8; 32];
        flipped.copy_from_slice(public.as_bytes());
        flipped[0] ^= 0xFF;
        let bundle = PreKeyBundle {
            identity: bob.identity.public(),
            signed_pre_key: (id, PublicKey::from(flipped), signature),
            one_time_pre_key: None,
        };
        assert!(matches!(initiate(&alice, &bundle, &mut rng), Err(CryptoError::BadSignature)));
    }

    /// key_id 错位：Alice 用了 Bob 已轮换丢弃的旧 SPK，Bob 必须拒绝
    #[test]
    fn stale_pre_key_id_is_rejected() {
        let mut rng = OsRng;
        let alice = IdentityKeyPair::generate(&mut rng);
        let bob = BobShelf::new();

        let (_, mut initiation) = initiate(&alice, &bob.bundle(), &mut rng).unwrap();
        initiation.signed_pre_key_id += 1; // 模拟陈旧引用
        let err = respond(&bob.identity, &bob.spk, Some(&bob.opk), &initiation);
        assert!(matches!(err, Err(CryptoError::Ratchet(_))));
    }

    /// 编解码往返：encode → decode 得到等值结构（含 OPK 与不含 OPK 两种）
    #[test]
    fn initiation_codec_roundtrip() {
        let mut rng = OsRng;
        let alice = IdentityKeyPair::generate(&mut rng);
        let bob = BobShelf::new();

        let (_, initiation) = initiate(&alice, &bob.bundle(), &mut rng).unwrap();
        let decoded = X3dhInitiation::decode(&initiation.encode()).unwrap();
        assert_eq!(decoded, initiation);

        // OPK 用尽的三重 DH 形态
        let bundle = PreKeyBundle {
            identity: bob.identity.public(),
            signed_pre_key: bob.spk.bundle_part(),
            one_time_pre_key: None,
        };
        let (_, initiation3) = initiate(&alice, &bundle, &mut rng).unwrap();
        let decoded3 = X3dhInitiation::decode(&initiation3.encode()).unwrap();
        assert_eq!(decoded3, initiation3);
        assert!(decoded3.one_time_pre_key_id.is_none());
    }

    /// 三重 DH（无 OPK）同样收敛一致：库存用尽不阻断会话建立
    #[test]
    fn triple_dh_without_opk_still_converges() {
        let mut rng = OsRng;
        let alice = IdentityKeyPair::generate(&mut rng);
        let bob = BobShelf::new();
        let bundle = PreKeyBundle {
            identity: bob.identity.public(),
            signed_pre_key: bob.spk.bundle_part(),
            one_time_pre_key: None,
        };
        let (sk_a, initiation) = initiate(&alice, &bundle, &mut rng).unwrap();
        let sk_b = respond(&bob.identity, &bob.spk, None, &initiation).unwrap();
        assert_eq!(sk_a.expose(), sk_b.expose());
    }
}
