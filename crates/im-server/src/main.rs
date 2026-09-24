//! # im-server：IM 服务端
//!
//! 职责（阶段 3 实现）：
//! - 网关接入层：每连接一个 task + 有界 channel 背压
//! - 会话路由表（`DashMap`）：user_id → 连接
//! - 群消息扇出、离线消息、消息持久化
//! - 分布式预留：一致性哈希路由
//!
//! 学习文档：`docs/06-server-arch.md`

fn main() -> anyhow::Result<()> {
    // 阶段 0：占位入口。阶段 3 将替换为真正的网关启动逻辑。
    println!("im-server：阶段 0 骨架，等待阶段 3 实现网关接入层");
    Ok(())
}
