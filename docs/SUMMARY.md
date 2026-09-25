# Summary

文档目录（mdBook 唯一的目录来源）。阅读顺序 = 编号顺序：语言地基 → 协议与传输 →
服务端与客户端 → Web 与社交 → 性能与工程；20 号踩坑实录全程可随时查阅。

- [00 - 项目路线图](00-roadmap.md)

# 语言地基（先打地基，再看 IM）

- [01 - 所有权、借用与生命周期](01-rust-core.md)
- [02 - Send、Sync 与 Pin](02-send-sync-pin.md)
- [03 - async/await 与 Tokio](03-async-tokio.md)

# 协议与传输

- [04 - 二进制协议设计](04-protocol-design.md)
- [05 - 传输层设计](05-network-tokio.md)

# 服务端与客户端

- [06 - 服务端架构](06-server-arch.md)
- [07 - 客户端：消息可靠性、本地库与 TUI](07-client.md)

# Web 与社交

- [12 - Web 协议：REST、WS 信封、事件推送与富媒体](12-web-protocol.md)
- [13 - 群消息扇出与 2 万人在线](13-group-fanout.md)
- [14 - WebRTC 音视频与远程桌面](14-webrtc.md)
- [15 - 群会议与屏幕共享（LiveKit SFU）](15-meeting.md)

# 性能与工程

- [16 - 性能压测与三级里程碑](16-perf.md)
- [17 - FFI SDK：C ABI / JNI / 内存契约](17-ffi.md)
- [18 - TLS 与 E2EE：rustls + Signal 双棘轮 + 类型状态](18-tls-e2ee.md)
- [19 - QUIC 与挂载盘：多路复用传输 + FS 视图](19-quic-fuse.md)

# 附录

- [20 - Rust 全栈踩坑与填坑实录](20-rust-pitfalls.md)
