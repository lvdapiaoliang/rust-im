//! # im-protocol：IM 二进制协议层
//!
//! 职责（阶段 1 实现）：
//! - 自定义二进制帧格式：`magic + version + cmd + flags + seq + ack + len + payload + crc`
//! - 命令字体系：握手 / 心跳 / 单聊 / 群聊 / ACK / 消息同步
//! - varint / zigzag 编码，帧编解码器（处理 TCP 粘包 / 半包）
//!
//! 设计原则：本 crate 是**纯逻辑**，不依赖任何 IO（tokio 等），
//! 这样协议编解码可以用最快的单元测试与 proptest 模糊测试覆盖，
//! 也方便未来替换底层传输（TCP → QUIC）而协议层不动。
//!
//! 学习文档：`docs/04-protocol-design.md`（阶段 1 编写）

/// 协议魔数：每个帧的固定开头，用于快速识别本协议的流量（类似 PNG 头）
pub const MAGIC: u16 = 0x494D; // "IM"

/// 协议版本号：预留协议演进空间，握手时可协商
pub const VERSION: u8 = 1;

#[cfg(test)]
mod tests {
    /// 阶段 0 骨架测试：验证 crate 可编译、常量符合预期
    #[test]
    fn magic_is_im() {
        assert_eq!(super::MAGIC, 0x494D);
        assert_eq!(super::VERSION, 1);
    }
}
