//! # im-crypto：加密层
//!
//! 职责（阶段 7 实现）：
//! - TLS/mTLS 材料管理（配合 im-transport 的 rustls）
//! - E2EE 端到端加密：Signal 协议
//!   - X3DH 密钥协商（首次建立共享密钥）
//!   - 双棘轮 Double Ratchet（前向保密 + 乱序消息解密）
//! - 密钥本地安全存储
//!
//! 学习文档：`docs/10-e2ee.md`（阶段 7 编写）

/// 密钥字节数（X25519 私钥长度，为阶段 7 预留）
pub const KEY_LEN: usize = 32;

#[cfg(test)]
mod tests {
    #[test]
    fn key_len_is_32() {
        assert_eq!(super::KEY_LEN, 32);
    }
}
