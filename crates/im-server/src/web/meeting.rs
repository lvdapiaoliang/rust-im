//! 会议模块（阶段 9）：LiveKit SFU 的入会令牌（JWT）签发。
//!
//! ```text
//!   Vue 前端 ──POST /api/groups/{id}/meeting/token──▶ im-server
//!              ◀── { token, url, room } ──
//!   Vue 前端 ──token + url──────────────────────────▶ LiveKit SFU
//!              ◀═══ WebRTC 媒体（SFU 转发，非 P2P）═══
//! ```
//!
//! 阶段 8 的 P2P 管道在两人时是最优解，但 N 人全连接需要
//! N×(N-1)/2 条管道——8 人会议每人上行 7 路视频，带宽瞬间爆炸。
//! SFU（Selective Forwarding Unit）换拓扑：每人只上行一路，服务器
//! 选择性转发给其余人。LiveKit 是开源 SFU 的事实标准，我们自建
//! im-server 里唯一要做的就是**签发入会令牌**——认证在我们手里
//! （是不是群成员我们说了算），媒体转发外包给 SFU。
//!
//! 令牌是标准 JWT（HS256）。没有引入 jsonwebtoken 依赖：HMAC-SHA256
//! 本体约 20 行、base64url 编码约 15 行，全部手写并用 RFC 4231 的
//! 官方测试向量验证——**签名是安全边界，亲手写一遍才知道边界在哪**
//! （对照阶段 0 手写帧编解码的同一理由：造轮子的价值在理解，不在替代）。

use serde_json::json;

/// HMAC-SHA256 的块大小（字节）——密钥超出先做一次 SHA-256（RFC 2104）。
const HMAC_BLOCK: usize = 64;

/// 会议令牌有效期：2 小时（会议的合理时长上限；过短会中途掉线，
/// 过长则失窃令牌的暴露窗口变大——与登录令牌 7 天是不同的权衡）。
const MEETING_TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(2 * 3600);

/// `LiveKit` 连接配置：全部可由环境变量覆盖，默认值即官方 docker-compose
/// 的本地开发凭据（`devkey`/`secret`）——**零配置可跑通本机演示**，
/// 生产换环境变量即可，代码不动。
#[derive(Debug, Clone)]
pub struct LiveKitConfig {
    /// 前端连接地址（`ws://` 或 `wss://`，含端口）。
    pub url: String,
    /// API key（JWT 的 `iss`）。
    pub api_key: String,
    /// API secret（JWT 的 HMAC 签名密钥）。
    pub api_secret: String,
}

impl LiveKitConfig {
    /// 从环境变量读取，缺省回落本地开发默认值（空字符串视为未设置——
    /// 显式空值没有合法场景，不该把「未配置」伪装成「连空地址」）。
    ///
    /// - `IM_LIVEKIT_URL`（默认 `ws://127.0.0.1:7880`）
    /// - `IM_LIVEKIT_API_KEY`（默认 `devkey`）
    /// - `IM_LIVEKIT_API_SECRET`（默认 `secret`）
    #[must_use]
    pub fn from_env() -> Self {
        let read = |key: &str, fallback: &str| match std::env::var(key) {
            Ok(v) if !v.is_empty() => v,
            _ => fallback.to_string(),
        };
        Self {
            url: read("IM_LIVEKIT_URL", "ws://127.0.0.1:7880"),
            api_key: read("IM_LIVEKIT_API_KEY", "devkey"),
            api_secret: read("IM_LIVEKIT_API_SECRET", "secret"),
        }
    }
}

/// 会议房间名：群 ID 派生（一群最多一间常驻会议室，不持久化——
/// `LiveKit` 侧房间在最后一人离开后自动回收，我们的 DB 零新增表）。
#[must_use]
pub fn room_name(group_id: u64) -> String {
    format!("im-meeting-{group_id}")
}

/// 签发入会令牌：标准三段 JWT（`header.payload.signature`）。
///
/// `sub` 用用户 ID、`name` 用昵称（`LiveKit` 界面上显示的名字）；
/// `video` 是 `LiveKit` 的授权载荷（`grants`）——`roomJoin` + 房间名 +
/// 发布/订阅双许可。**授权在签名里完成**：SFU 只验签不放权，
/// 「谁能进这间房」的裁决在我们服务端（`is_member` 门槛，见 api.rs）。
///
/// # Panics
///
/// claims/header 序列化失败时 panic——载荷全是字符串与数字，
/// `serde_json` 不会失败（这个 `expect` 与全项目「发号器保证」同款）。
#[must_use]
pub fn sign_meeting_token(
    cfg: &LiveKitConfig,
    user_id: u64,
    display_name: &str,
    room: &str,
) -> String {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        + MEETING_TOKEN_TTL.as_secs();

    let header = json!({ "alg": "HS256", "typ": "JWT" });
    let claims = json!({
        "iss": cfg.api_key,
        "sub": user_id.to_string(),      // 雪花 ID 串化——与全项目 JSON 约定一致
        "name": display_name,
        "exp": exp,
        "video": {
            "roomJoin": true,
            "room": room,
            "canPublish": true,
            "canSubscribe": true,
        },
    });

    // 三段拼接：前两段 base64url 无填充，签名覆盖「header.payload」
    let payload = b64url(serde_json::to_string(&claims).expect("JSON 载荷可序列化").as_bytes());
    let signed = format!(
        "{}.{}",
        b64url(serde_json::to_string(&header).expect("JSON 头可序列化").as_bytes()),
        payload,
    );
    let sig = hmac_sha256(cfg.api_secret.as_bytes(), signed.as_bytes());
    format!("{signed}.{}", b64url(&sig))
}

/// RFC 2104 HMAC-SHA256：`H(K XOR opad, H(K XOR ipad, msg))`。
///
/// 密钥短于块大小右侧补零；超出块大小先哈希一次（哈希后恰好 32 字节
/// < 64，不再需要补零）。两个 64 字节的 pad 只依赖密钥——签名多次
/// 时可以预计算，这里签名频率是「进会议室」级，不值得缓存。
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    // 密钥归一化到块大小
    let mut key_block = [0u8; HMAC_BLOCK];
    if key.len() > HMAC_BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let ipad = 0x36; // 0011 0110
    let opad = 0x5c; // 0101 1100
    let mut inner = Sha256::new();
    let mut outer = Sha256::new();
    for byte in &key_block {
        inner.update([byte ^ ipad]);
        outer.update([byte ^ opad]);
    }
    inner.update(msg);
    let inner_digest = inner.finalize();
    outer.update(inner_digest);
    let digest = outer.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// base64url 编码（URL 安全字母表，无填充 `=`——JWT 规范要求）。
fn b64url(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let bytes = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let word = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        // 尾块只产出实际字节对应的字符数（1 字节→2 字符，2 字节→3 字符）
        let chars = match chunk.len() {
            1 => 2,
            2 => 3,
            _ => 4,
        };
        for i in 0..chars {
            let shift = 18 - i * 6;
            out.push(ALPHABET[((word >> shift) & 0x3f) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231 官方测试向量（HMAC-SHA256）——签名是安全边界，
    /// 正确性不靠目测。
    #[test]
    fn hmac_matches_rfc4231() {
        // Test Case 1：密钥 0x0b × 20
        let key1 = [0x0bu8; 20];
        let out1 = hmac_sha256(&key1, b"Hi There");
        assert_eq!(hex(&out1), "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        // Test Case 2：密钥 "Jefe"，消息 "what do ya want for nothing?"
        let out2 = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(hex(&out2), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        // Test Case 6：密钥超块大小（131 字节 0xaa）——验证先哈希再入 pad 的分支
        let key6 = [0xaau8; 131];
        let out6 = hmac_sha256(&key6, b"Test Using Larger Than Block-Size Key - Hash Key First");
        assert_eq!(hex(&out6), "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;

        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(out, "{b:02x}");
        }
        out
    }

    /// base64url：对照标准库实现手工验证几个边界（1/2/3 字节尾块）。
    #[test]
    fn b64url_known_vectors() {
        assert_eq!(b64url(&[0]), "AA");
        assert_eq!(b64url(&[0, 0]), "AAA");
        assert_eq!(b64url(&[0, 0, 0]), "AAAA");
        assert_eq!(b64url(b"ab"), "YWI");
        assert_eq!(b64url(b"abc"), "YWJj");
        // RFC 4648 §5 的 URL 安全示例（含 +/ → -_ 的字母表差异）
        assert_eq!(b64url(&[0xfb, 0xef]), "--8");
        assert_eq!(b64url(&[0xff]), "_w");
    }

    /// JWT 三段结构：解回 claims 验证 iss/sub/room/grants，且签名可复算。
    /// （自签自验只证明确定性；与 `LiveKit` 的互认靠手工联调——文档里
    /// 记录了本机无 Docker 的验证缺口。）
    #[test]
    fn meeting_token_is_valid_jwt_shape() {
        let cfg = LiveKitConfig {
            url: "ws://127.0.0.1:7880".into(),
            api_key: "devkey".into(),
            api_secret: "secret".into(),
        };
        let token = sign_meeting_token(&cfg, 42, "阿黄", "im-meeting-7");

        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT 必须是三段");
        assert_eq!(parts[0], "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9", "HS256 标准头");

        // 载荷段解回 JSON（先补齐 padding 再交给 serde）
        let padded = match parts[1].len() % 4 {
            2 => format!("{}==", parts[1]),
            3 => format!("{}=", parts[1]),
            _ => parts[1].to_string(),
        };
        let raw =
            String::from_utf8(base64_decode(padded.as_bytes()).expect("载荷是合法 base64url"))
                .expect("载荷是 UTF-8 JSON");
        let claims: serde_json::Value = serde_json::from_str(&raw).expect("载荷可解析");
        assert_eq!(claims["iss"], "devkey");
        assert_eq!(claims["sub"], "42");
        assert_eq!(claims["name"], "阿黄");
        assert_eq!(claims["video"]["room"], "im-meeting-7");
        assert_eq!(claims["video"]["roomJoin"], true);

        // 签名可复算（同一密钥同一输入 → 同一签名）
        let expect =
            b64url(&hmac_sha256(b"secret", format!("{}.{}", parts[0], parts[1]).as_bytes()));
        assert_eq!(parts[2], expect);
    }

    /// 标准字母表解码（测试专用——生产只签不解，验证是 `LiveKit` 的事）。
    ///
    /// 窄化转换全部安全：6 位值拼进 24 位窗口再按 8 位切——掩码之外
    /// 没有位能进入高位，`u8` 永不截断到错误值。
    #[allow(clippy::cast_possible_truncation)]
    fn base64_decode(input: &[u8]) -> Result<Vec<u8>, &'static str> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut vals = Vec::with_capacity(input.len());
        for &b in input {
            if b == b'=' {
                continue; // 填充符：补齐到 4 倍数后送进来的，不承载数据
            }
            let v = ALPHABET.iter().position(|&a| a == b).ok_or("非法字符")?;
            vals.push(v as u8);
        }
        let mut out = Vec::with_capacity(vals.len() * 3 / 4);
        for chunk in vals.chunks(4) {
            let mut word = 0u32;
            for (i, v) in chunk.iter().enumerate() {
                word |= u32::from(*v) << (18 - i * 6);
            }
            out.push((word >> 16) as u8);
            if chunk.len() > 2 {
                out.push((word >> 8) as u8);
            }
            if chunk.len() > 3 {
                out.push(word as u8);
            }
        }
        Ok(out)
    }
}
