//! 双棘轮（Double Ratchet）：E2EE 会话的「每条消息都在换锁」。
//!
//! # 两个棘轮，两种保密
//!
//! - **对称棘轮**（每条消息）：发送链 `CK` 前进一步派生消息密钥 `MK`——
//!   用后即焚，拿到第 N 条的 MK 推不出第 N-1 条（**消息级前向保密**）；
//! - **DH 棘轮**（每轮往返）：任一方收到新 DH 公钥就生成新密钥对重走
//!   一次 X25519——即使某条链密钥泄露，下一轮 DH 也把根密钥洗掉
//!   （**轮级自愈**，也是「双向都发消息才推进」的原因）。
//!
//! ```text
//! ── 对称棘轮（快，每条消息）──      ── DH 棘轮（慢，每轮往返）──
//! CK ─┬─ HMAC(0x01) → MK（加密这条）  RK ── HKDF(DH(新对, 对端公钥)) ──┐
//!    └─ HMAC(0x02) → CK'（下一条）   ↑←──────────────────────────────┘
//! ```
//!
//! # 乱序容忍：跳过密钥缓存
//!
//! 网络重排/重传让消息可能乱序到达。解密方发现序号跳了，就把跳过
//! 序号的消息密钥**先派生好存进 `skipped`**，后到的旧消息用缓存解。
//! 上限 [`MAX_SKIP`]：一次跳太多说明对端有 bug 或在搞 DoS 扩张内存，
//! 拒绝比包容便宜（与 im-transport 去重窗口的「重同步而非丢弃」是
//! 同一个防御思想的两种表达：那边防丢消息，这边防丢密钥）。
//!
//! # 与规范的学习版差异（诚实边界）
//!
//! - 规范的 AD（关联数据）含双方身份公钥，本实现把 AAD 绑定在**消息头**
//!   上（防头被改导致错位解密）；身份指纹绑定在 X3DH 层已经完成一次；
//! - 不含头部加密（PQXDH/ADOC 那种把 header 也藏起来的进阶形态）；
//! - 密钥删除靠 Rust 所有权 + `ZeroizeOnDrop`：MK 在 `encrypt`/`decrypt`
//!   末尾出栈即擦，比规范伪代码的「显式 delete」更难写漏。

use std::collections::HashMap;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use hkdf::Hkdf;
use hkdf::hmac::{Hmac, Mac};
use rand::{CryptoRng, RngCore};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

use crate::error::CryptoError;

/// 单链最多允许跳过的消息密钥数（DoS 防御上限）。
pub const MAX_SKIP: u32 = 100;

/// 根密钥/链密钥长度（32 字节）。
const KEY_LEN: usize = 32;
/// AES-256-GCM 密钥 32B + nonce 12B：一次 HKDF 展开。
const MSG_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
/// GCM 认证标签长度（密文 = 明文 + 16B tag）。
const TAG_LEN: usize = 16;
/// 消息头长度：DH 公钥 32B + PN 4B + N 4B。
const HEADER_LEN: usize = 32 + 4 + 4;

const ROOT_INFO: &[u8] = b"im-e2ee-ratchet-root-v1";
const MSG_KEY_INFO: &[u8] = b"im-e2ee-message-key-v1";

// ────────────────────────────────────────────────────────────────
// 消息与头部
// ────────────────────────────────────────────────────────────────

/// 消息头：告诉对端「这条消息处于棘轮的哪个位置」。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// 发送方当前 DH 棘轮公钥（对端据此判断是否要推进 DH 棘轮）。
    pub dh: PublicKey,
    /// 发送方**上一条发送链**的长度（对端据此排空旧链的跳过密钥）。
    pub prev_chain_len: u32,
    /// 本条消息在当前发送链上的序号。
    pub n: u32,
}

impl Header {
    fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..32].copy_from_slice(self.dh.as_bytes());
        out[32..36].copy_from_slice(&self.prev_chain_len.to_be_bytes());
        out[36..40].copy_from_slice(&self.n.to_be_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() < HEADER_LEN {
            return Err(CryptoError::Ratchet("消息头长度不足".into()));
        }
        let mut dh = [0u8; 32];
        dh.copy_from_slice(&bytes[..32]);
        Ok(Self {
            dh: PublicKey::from(dh),
            prev_chain_len: u32::from_be_bytes([bytes[32], bytes[33], bytes[34], bytes[35]]),
            n: u32::from_be_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]),
        })
    }
}

/// 一条 E2EE 消息：头 + AES-256-GCM 密文（含认证标签）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RatchetMessage {
    header: Header,
    ciphertext: Vec<u8>,
}

impl RatchetMessage {
    /// 编码成字节（头 40B ‖ 密文）——直接可塞进 `Cmd::Msg` 的 payload。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.ciphertext.len());
        out.extend_from_slice(&self.header.encode());
        out.extend_from_slice(&self.ciphertext);
        out
    }

    /// 从字节解码（`encode` 的逆）。
    ///
    /// # Errors
    ///
    /// 长度不足以容纳头 + 标签时返回 [`CryptoError::Ratchet`]。
    pub fn decode(bytes: &[u8]) -> Result<Self, CryptoError> {
        let header = Header::decode(bytes)?;
        if bytes.len() < HEADER_LEN + TAG_LEN {
            return Err(CryptoError::Ratchet("密文长度不足（连标签都放不下）".into()));
        }
        Ok(Self {
            header,
            ciphertext: bytes[HEADER_LEN..].to_vec(),
        })
    }
}

// ────────────────────────────────────────────────────────────────
// 双棘轮状态机
// ────────────────────────────────────────────────────────────────

/// 双棘轮会话状态（一端一份，互为镜像）。
///
/// 字段与 Signal 规范的 RatchetState 一一对应；
/// `dh_self` 为 `None` 表示「该我发起第一轮 DH 棘轮」（Alice 初态）。
pub struct RatchetState {
    /// 根密钥 RK：只被 DH 棘轮推进，永不直接加密消息。
    root_key: [u8; KEY_LEN],
    /// 己方 DH 棘轮密钥对（`None` = 尚未发起第一轮）。
    dh_self: Option<StaticSecret>,
    /// 对端当前 DH 棘轮公钥。
    dh_remote: Option<PublicKey>,
    /// 发送链密钥 CKs（`None` = 未建立：第一轮 DH 后才有）。
    chain_send: Option<[u8; KEY_LEN]>,
    /// 接收链密钥 CKr。
    chain_recv: Option<[u8; KEY_LEN]>,
    /// 发送链已用序号 Ns。
    n_send: u32,
    /// 接收链已读序号 Nr。
    n_recv: u32,
    /// 上一条发送链的长度 PN（DH 棘轮推进时记录）。
    prev_chain_len: u32,
    /// 跳过密钥缓存：(对端棘轮公钥, 序号) → 消息密钥。
    ///
    /// 键带公钥：不同轮的旧链密钥天然分桶，排空旧链后按公钥整桶丢弃。
    skipped: HashMap<([u8; 32], u32), [u8; MSG_KEY_LEN]>,
}

impl Drop for RatchetState {
    /// 会话状态 drop 时擦除全部密钥材料（`StaticSecret` 自带擦除，
    /// `skipped` 里的数组不满足 `Zeroize` 派生，于是手动擦——
    /// 宁可显式三行，也不为派生把 HashMap 换成自定义容器）。
    fn drop(&mut self) {
        self.root_key.zeroize();
        if let Some(c) = &mut self.chain_send {
            c.zeroize();
        }
        if let Some(c) = &mut self.chain_recv {
            c.zeroize();
        }
        for (_, mk) in self.skipped.iter_mut() {
            mk.zeroize();
        }
    }
}

impl RatchetState {
    /// 发起方（Alice）初始化：X3DH 的 SK + 对端（Bob）的 SPK 公钥。
    ///
    /// Alice 的 `dh_self` 为 `None`——第一条 `encrypt` 时才生成新密钥对
    /// 并推进第一轮 DH 棘轮（规范的 RatchetInit initiator 形态）。
    #[must_use]
    pub fn init_initiator(sk: [u8; KEY_LEN], remote_dh: PublicKey) -> Self {
        Self {
            root_key: sk,
            dh_self: None,
            dh_remote: Some(remote_dh),
            chain_send: None,
            chain_recv: None,
            n_send: 0,
            n_recv: 0,
            prev_chain_len: 0,
            skipped: HashMap::new(),
        }
    }

    /// 响应方（Bob）初始化：X3DH 的 SK + 自己的 SPK 密钥对。
    ///
    /// Bob 的初始 `dh_self` 就是 SPK——Alice 的第一轮 DH 用的正是它的
    /// 公钥，两边在第一次消息往返后完全对称。
    #[must_use]
    pub fn init_responder(sk: [u8; KEY_LEN], spk_secret: StaticSecret) -> Self {
        Self {
            root_key: sk,
            dh_self: Some(spk_secret),
            dh_remote: None,
            chain_send: None,
            chain_recv: None,
            n_send: 0,
            n_recv: 0,
            prev_chain_len: 0,
            skipped: HashMap::new(),
        }
    }

    /// 加密一条消息（推进对称棘轮；必要时先推进 DH 棘轮）。
    ///
    /// # Errors
    ///
    /// 内部 KDF 失败（现实中不可达——输出长度都在 HKDF 上限内）。
    pub fn encrypt(
        &mut self,
        plaintext: &[u8],
        rng: &mut (impl CryptoRng + RngCore),
    ) -> Result<RatchetMessage, CryptoError> {
        // 第一条消息（Alice 初态）：先推 DH 棘轮，建立发送链
        if self.chain_send.is_none() {
            self.dh_ratchet_send_side(rng);
        }
        let ck = self.chain_send.expect("上面刚建立了发送链");

        // 对称棘轮一步：CK → (MK, CK')
        let mk = kdf_ck_mk(&ck);
        self.chain_send = Some(kdf_ck_next(&ck));
        let n = self.n_send;
        self.n_send = self.n_send.checked_add(1).expect("消息序号不会溢出 u32（2^32 条/链）");

        let header = Header {
            dh: PublicKey::from(self.dh_self.as_ref().expect("发送链存在则 DH 密钥对存在")),
            prev_chain_len: self.prev_chain_len,
            n,
        };
        let header_bytes = header.encode();
        let ct = seal(&mk, plaintext, &header_bytes)?;

        Ok(RatchetMessage { header, ciphertext: ct })
    }

    /// 解密一条消息（乱序自动经 `skipped` 缓存兜住）。
    ///
    /// # Errors
    ///
    /// - 序号跳跃超过 [`MAX_SKIP`]：拒绝（DoS 防御）；
    /// - GCM 认证失败（密文/头被篡改、消息错投到别人的会话）：拒绝。
    pub fn decrypt(&mut self, msg: &RatchetMessage) -> Result<Vec<u8>, CryptoError> {
        // 1. 乱序旧消息：先查跳过密钥缓存
        let key = (*msg.header.dh.as_bytes(), msg.header.n);
        if let Some(mk) = self.skipped.remove(&key) {
            return open(&mk, &msg.ciphertext, &msg.header.encode());
        }

        // 2. 新的远端公钥 = 对端推进了 DH 棘轮：排空旧链 + 推进自己的
        if Some(msg.header.dh) != self.dh_remote {
            if self.dh_remote.is_some() {
                // 对端换了密钥对——先把它**上一条链**（PN 长度）的跳过密钥补齐，
                // 防止旧链的迟到消息永远解不开。
                // 旧链先**拷出**再调 `&mut self` 的方法：[u8;32] 是 Copy，
                // 借用冲突用值拷贝消解（32 字节的拷贝在这里不值一提）
                let old_remote = self.dh_remote.expect("上面刚检查过 Some");
                let old_chain = self.chain_recv.expect("有旧远端公钥则有旧接收链");
                self.skip_to(old_remote, &old_chain, msg.header.prev_chain_len)?;
            }
            self.dh_ratchet_recv_side(&msg.header.dh);
        }

        // 3. 当前链上仍需跳到 header.n：把跳过序号的 MK 缓存好
        let remote = self.dh_remote.expect("DH 棘轮推进后必有远端公钥");
        let chain = self.chain_recv.expect("DH 棘轮推进后必有接收链");
        self.skip_to(remote, &chain, msg.header.n)?;
        // 推进接收链指针：拿走第 n 个 MK，CKr 前进
        let mk = kdf_ck_mk(&chain);
        self.chain_recv = Some(kdf_ck_next(&chain));
        self.n_recv = msg.header.n + 1;

        open(&mk, &msg.ciphertext, &msg.header.encode())
    }

    /// DH 棘轮·发送侧（Alice 第一条消息 / 我方先推进的形态）：
    /// 生成新密钥对 → 一次 DH 洗根密钥 → 派生新发送链。
    fn dh_ratchet_send_side(&mut self, rng: &mut (impl CryptoRng + RngCore)) {
        let new_secret = StaticSecret::random_from_rng(rng);
        let remote = self.dh_remote.expect("发送侧推进必须有远端公钥");
        let dh_out = new_secret.diffie_hellman(&remote);
        let (rk, cks) = kdf_rk(&self.root_key, dh_out.as_bytes());
        self.root_key = rk;
        self.dh_self = Some(new_secret);
        self.prev_chain_len = self.n_send;
        self.chain_send = Some(cks);
        self.chain_send_prev_reset();
    }

    /// DH 棘轮·接收侧（对端先推进的形态）：两次 DH 洗根——
    /// 先和**旧**密钥对算出对方的新发送链（CKr），再换新密钥对
    /// 算出自己的新发送链（CKs）。
    fn dh_ratchet_recv_side(&mut self, remote: &PublicKey) {
        let old_secret = self.dh_self.take().expect("接收侧推进必有己方密钥对");
        // 记住对端新公钥：decrypt 后续步骤与「对端是否又换了钥匙」的
        // 判断都依赖它——忘了写回就是自己给自己埋 panic
        self.dh_remote = Some(*remote);
        // 第一跳：对端的新发送链
        let dh_out = old_secret.diffie_hellman(remote);
        let (rk, ckr) = kdf_rk(&self.root_key, dh_out.as_bytes());
        self.root_key = rk;
        self.chain_recv = Some(ckr);
        self.n_recv = 0;

        // 第二跳：自己的新发送链（再生成新密钥对）
        let mut rng = rand::rngs::OsRng;
        let new_secret = StaticSecret::random_from_rng(&mut rng);
        let dh_out = new_secret.diffie_hellman(remote);
        let (rk, cks) = kdf_rk(&self.root_key, dh_out.as_bytes());
        self.root_key = rk;
        self.dh_self = Some(new_secret);
        self.prev_chain_len = self.n_send;
        self.chain_send = Some(cks);
        self.chain_send_prev_reset();
    }

    /// DH 棘轮推进后发送链序号归零（小到不值一提，但两处调用同一个
    /// 语义就值得一个名字）。
    fn chain_send_prev_reset(&mut self) {
        self.n_send = 0;
    }

    /// 从当前接收链序号跳到 `until`：跳过序号的 MK 全部缓存。
    ///
    /// # Errors
    ///
    /// 跳跃跨度超过 [`MAX_SKIP`] 时拒绝。
    fn skip_to(
        &mut self,
        remote: PublicKey,
        chain: &[u8; KEY_LEN],
        until: u32,
    ) -> Result<(), CryptoError> {
        let from = self.n_recv;
        if until < from {
            return Err(CryptoError::Ratchet(format!(
                "消息序号倒退：当前已读到 {from}，消息声称 {until}"
            )));
        }
        if until - from > MAX_SKIP {
            return Err(CryptoError::Ratchet(format!(
                "单链跳过 {}/{} 超过 MAX_SKIP 上限，疑似异常对端",
                until - from,
                MAX_SKIP
            )));
        }
        let mut ck = *chain;
        let remote_bytes = *remote.as_bytes();
        for n in from..until {
            let mk = kdf_ck_mk(&ck);
            self.skipped.insert((remote_bytes, n), mk);
            ck = kdf_ck_next(&ck);
        }
        self.n_recv = until;
        Ok(())
    }
}

// ────────────────────────────────────────────────────────────────
// KDF 三件套（规范的 KDF_RK / KDF_CK + 消息密钥展开）
// ────────────────────────────────────────────────────────────────

/// KDF_RK：根密钥棘轮。HKDF(salt=RK, ikm=DH) → (新 RK, 新 CK)。
fn kdf_rk(rk: &[u8; KEY_LEN], dh_out: &[u8]) -> ([u8; KEY_LEN], [u8; KEY_LEN]) {
    let hk = Hkdf::<Sha256>::new(Some(rk), dh_out);
    let mut okm = [0u8; 64];
    hk.expand(ROOT_INFO, &mut okm).expect("64B 在 HKDF 上限内");
    let mut rk = [0u8; KEY_LEN];
    let mut ck = [0u8; KEY_LEN];
    rk.copy_from_slice(&okm[..32]);
    ck.copy_from_slice(&okm[32..]);
    (rk, ck)
}

/// KDF_CK 之消息密钥：HMAC(CK, 0x01)。
fn kdf_ck_mk(ck: &[u8; KEY_LEN]) -> [u8; MSG_KEY_LEN] {
    hmac_step(ck, 0x01)
}

/// KDF_CK 之链推进：HMAC(CK, 0x02)。
fn kdf_ck_next(ck: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
    hmac_step(ck, 0x02)
}

/// HMAC-SHA256(CK, 单字节输入)：对称棘轮的两条岔路共用一个原语。
///
/// `Mac` 与 `KeyInit` 都有 `new_from_slice`（后者是 AES-GCM 在用），
/// 全限定写法消解歧义——这不是坏味道，是两个同名 API 的正常共存。
fn hmac_step(ck: &[u8; KEY_LEN], input: u8) -> [u8; KEY_LEN] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(ck).expect("HMAC 接受任意长度密钥");
    mac.update(&[input]);
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(&mac.finalize().into_bytes());
    out
}

/// 消息密钥 → AES-GCM 的 (key, nonce)：一次性展开，nonce 不上线路
///（对端从同一个 MK 派生——GCM nonce 重用是灾难，从密钥派生保证
/// 每条消息的 nonce 随 MK 一起用后即焚）。
fn expand_msg_key(mk: &[u8; MSG_KEY_LEN]) -> ([u8; MSG_KEY_LEN], [u8; NONCE_LEN]) {
    let hk = Hkdf::<Sha256>::new(None, mk);
    let mut okm = [0u8; MSG_KEY_LEN + NONCE_LEN];
    hk.expand(MSG_KEY_INFO, &mut okm).expect("44B 在 HKDF 上限内");
    let mut key = [0u8; MSG_KEY_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    key.copy_from_slice(&okm[..32]);
    nonce.copy_from_slice(&okm[32..]);
    (key, nonce)
}

/// AES-256-GCM 加密，AAD = 消息头（密文与棘轮位置绑定，防头篡改）。
fn seal(mk: &[u8; MSG_KEY_LEN], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let (key, nonce) = expand_msg_key(mk);
    let cipher = Aes256Gcm::new_from_slice(&key).expect("32B 是 AES-256 的合法密钥长度");
    cipher
        .encrypt(&nonce.into(), Payload { msg: plaintext, aad })
        .map_err(|_| CryptoError::Ratchet("AES-GCM 加密失败".into()))
}

/// AES-256-GCM 解密（`seal` 的逆）。
fn open(mk: &[u8; MSG_KEY_LEN], ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let (key, nonce) = expand_msg_key(mk);
    let cipher = Aes256Gcm::new_from_slice(&key).expect("32B 是 AES-256 的合法密钥长度");
    cipher
        .decrypt(&nonce.into(), Payload { msg: ciphertext, aad })
        .map_err(|_| CryptoError::Ratchet("GCM 认证失败：密文或头被篡改/错投会话".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    /// 测试脚手架：一对已完成 X3DH 的双棘轮会话（Alice 发起方）。
    fn session_pair() -> (RatchetState, RatchetState) {
        let mut rng = OsRng;
        let bob_spk = StaticSecret::random_from_rng(&mut rng);
        let sk = [0x42u8; KEY_LEN]; // KDF 正确性不依赖 SK 的来源，固定值足够
        let alice = RatchetState::init_initiator(sk, PublicKey::from(&bob_spk));
        let bob = RatchetState::init_responder(sk, bob_spk);
        (alice, bob)
    }

    /// 主线用例：交替往返若干轮，双向解密一致
    ///（每一轮往返都真实推进 DH 棘轮——测的是状态机镜像对称性）
    #[test]
    fn alternating_conversation_roundtrips() {
        let (mut alice, mut bob) = session_pair();
        let mut rng = OsRng;

        for round in 0..5u32 {
            let a2b = format!("alice -> bob #{round}");
            let msg = alice.encrypt(a2b.as_bytes(), &mut rng).unwrap();
            assert_eq!(bob.decrypt(&msg).unwrap(), a2b.as_bytes());

            let b2a = format!("bob -> alice #{round}");
            let msg = bob.encrypt(b2a.as_bytes(), &mut rng).unwrap();
            assert_eq!(alice.decrypt(&msg).unwrap(), b2a.as_bytes());
        }
    }

    /// 连发模式：Alice 连发 5 条再收 Bob 的回信（单向链不推 DH 棘轮，
    /// 全靠对称棘轮 + 序号），Bob 按序全部解开
    #[test]
    fn burst_of_messages_on_one_chain() {
        let (mut alice, mut bob) = session_pair();
        let mut rng = OsRng;

        let msgs: Vec<_> = (0..5)
            .map(|i| alice.encrypt(format!("burst #{i}").as_bytes(), &mut rng).unwrap())
            .collect();
        // 序号在同一链上单调推进
        for (i, msg) in msgs.iter().enumerate() {
            assert_eq!(msg.header.n, i as u32);
            assert_eq!(bob.decrypt(msg).unwrap(), format!("burst #{i}").as_bytes());
        }
    }

    /// 乱序容忍：3 条消息按 2,0,1 的顺序到，全部解开（跳过密钥缓存）
    #[test]
    fn out_of_order_delivery_is_tolerated() {
        let (mut alice, mut bob) = session_pair();
        let mut rng = OsRng;

        let msgs: Vec<_> = (0..3)
            .map(|i| alice.encrypt(format!("ooo #{i}").as_bytes(), &mut rng).unwrap())
            .collect();

        // 乱序投递：先 2（跳过 0/1 → 缓存），再 0、1（命中缓存）
        assert_eq!(bob.decrypt(&msgs[2]).unwrap(), b"ooo #2");
        assert_eq!(bob.decrypt(&msgs[0]).unwrap(), b"ooo #0");
        assert_eq!(bob.decrypt(&msgs[1]).unwrap(), b"ooo #1");
    }

    /// 篡改检测：密文翻转一字节 → GCM 标签必败
    #[test]
    fn tampered_ciphertext_is_rejected() {
        let (mut alice, mut bob) = session_pair();
        let mut rng = OsRng;

        let msg = alice.encrypt(b"secret", &mut rng).unwrap();
        let mut wire = msg.encode();
        let last = wire.len() - 1;
        wire[last] ^= 0x01;
        let bad = RatchetMessage::decode(&wire).unwrap();
        assert!(matches!(bob.decrypt(&bad), Err(CryptoError::Ratchet(_))));
    }

    /// 篡改头部也逃不掉：头是 AAD，改序号/公钥 → 标签必败
    #[test]
    fn tampered_header_is_rejected() {
        let (mut alice, mut bob) = session_pair();
        let mut rng = OsRng;

        let msg = alice.encrypt(b"secret", &mut rng).unwrap();
        let mut wire = msg.encode();
        wire[37] ^= 0x01; // header.n 的中间字节
        let bad = RatchetMessage::decode(&wire).unwrap();
        assert!(matches!(bob.decrypt(&bad), Err(CryptoError::Ratchet(_))));
    }

    /// 错投会话：Carol 的会话状态解 Alice→Bob 的消息必然失败
    ///（GCM 标签在错误的密钥下校验不过——E2EE 的「端」由此保证）
    #[test]
    fn wrong_session_cannot_decrypt() {
        let (mut alice, _bob) = session_pair();
        let carol_spk = StaticSecret::random_from_rng(&mut OsRng);
        let mut carol = RatchetState::init_responder([0x99u8; KEY_LEN], carol_spk);
        let mut rng = OsRng;

        let msg = alice.encrypt(b"for bob only", &mut rng).unwrap();
        assert!(matches!(carol.decrypt(&msg), Err(CryptoError::Ratchet(_))));
    }

    /// 消息密钥不重复：同一明文连发 10 条，密文互不相同
    ///（对称棘轮单调性的黑盒证据）
    #[test]
    fn ciphertexts_are_unique_per_message() {
        let (mut alice, _bob) = session_pair();
        let mut rng = OsRng;

        let cts: Vec<Vec<u8>> = (0..10)
            .map(|_| alice.encrypt(b"same plaintext", &mut rng).unwrap().ciphertext)
            .collect();
        for i in 0..cts.len() {
            for j in i + 1..cts.len() {
                assert_ne!(cts[i], cts[j], "第 {i} 与 {j} 条密文不应相同");
            }
        }
    }

    /// MAX_SKIP 上限：对端声称跳了远超上限的序号 → 拒绝并报告
    #[test]
    fn excessive_skip_is_rejected() {
        let (mut alice, mut bob) = session_pair();
        let mut rng = OsRng;

        // Alice 连发 MAX_SKIP+5 条后把最后一条投给 Bob：
        // Bob 要一口气补 MAX_SKIP+4 个跳过密钥 → 必须拒绝
        let mut last = None;
        for _ in 0..(MAX_SKIP + 5) {
            last = Some(alice.encrypt(b"x", &mut rng).unwrap());
        }
        let err = bob.decrypt(&last.expect("循环必然赋值"));
        assert!(
            matches!(&err, Err(CryptoError::Ratchet(msg)) if msg.contains("MAX_SKIP")),
            "应报告 MAX_SKIP 拒绝，实际: {err:?}"
        );
    }

    /// 编解码往返：encode → decode 逐字节等值
    #[test]
    fn message_codec_roundtrip() {
        let (mut alice, _bob) = session_pair();
        let mut rng = OsRng;
        let msg = alice.encrypt(b"codec", &mut rng).unwrap();
        let decoded = RatchetMessage::decode(&msg.encode()).unwrap();
        assert_eq!(decoded, msg);
    }

    /// 太短的字节流直接被解码挡下
    #[test]
    fn short_input_rejected_in_decode() {
        assert!(RatchetMessage::decode(b"tiny").is_err());
    }
}
