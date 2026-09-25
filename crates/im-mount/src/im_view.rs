//! IM 数据 → 文件系统视图的协议映射（阶段 13）。
//!
//! # 挂载盘的本质：世界观翻译
//!
//! 操作系统不认识「联系人」「会话」「消息」——它只认识目录和
//! 文件。挂载盘就是一次**世界观翻译**：把 IM 的领域模型固定
//! 映射成 FS 树，让任何语言写的任何工具（grep、find、备份脚本、
//! 网盘同步）零成本消费 IM 数据：
//!
//! ```text
//!   IM 世界观                          FS 视图（本模块的映射约定）
//!   ─────────                          ──────────────────────────
//!   联系人 alice     ──────▶  /contacts/alice.txt      名片（JSON 行）
//!   会话历史（按人） ──────▶  /history/alice/2026-09-24.log
//!   会话历史（按日）          （一个文件 = 一天的对话，文本格式
//!                             谁都能 cat，不逼人先装 IM 客户端）
//!   共享文件         ──────▶  /files/<uuid>.bin         原始字节
//! ```
//!
//! 映射约定是**双端契约**（与协议字段同一性质：一旦有消费者依赖，
//! 改布局就是 breaking change）——所以集中在一个函数里，而不是
//! 散落在调用方各自拼路径。
//!
//! # 为什么只读
//!
//! 本模块构建的视图是 IM 数据的**投影**（projection）：数据主权
//! 在 IM 侧（协议/数据库），文件系统侧只读。从 FS 侧改文件再
//! 「同步回」IM 需要双向冲突合并——那是产品级的坑（见 docs/19
//! 的取舍记录），学习项目止步于单向投影。

use crate::error::FsError;
use crate::memfs::MemFs;

/// 联系人（IM 侧数据的最小模型——真实结构在 im-server 的表里，
/// 这里取挂载盘需要的投影字段）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contact {
    /// 显示名（也是 FS 视图里的文件名素材）
    pub name: String,
    /// 账号（名片内容的一部分）
    pub username: String,
}

/// 一条聊天消息（挂载盘投影所需的最小字段）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    /// 对话对端（决定 `/history/<peer>/` 的归档目录）
    pub peer: String,
    /// 发送者显示名（日志行里的前缀）
    pub from: String,
    /// 消息文本（富媒体的降级形态是 `[文件]`/`[图片]`——
    /// 与 TUI 侧阶段 6 的降级策略同一条纪律）
    pub text: String,
    /// 发送日期 `YYYY-MM-DD`（决定归到哪个日志文件；
    /// 由数据源给成字符串，日期运算不进语义层）
    pub date: String,
}

/// 视图的固定布局（双端契约，见模块文档）。
pub mod layout {
    /// 联系人名片的目录
    pub const CONTACTS_DIR: &str = "/contacts";
    /// 消息历史的根目录
    pub const HISTORY_DIR: &str = "/history";
    /// 共享文件的目录
    pub const FILES_DIR: &str = "/files";

    /// 联系人名片路径：`/contacts/<name>.txt`
    #[must_use]
    pub fn contact_card(name: &str) -> String {
        format!("{CONTACTS_DIR}/{name}.txt")
    }

    /// 消息日志路径：`/history/<peer>/<date>.log`
    #[must_use]
    pub fn history_log(peer: &str, date: &str) -> String {
        format!("{HISTORY_DIR}/{peer}/{date}.log")
    }
}

/// 把 IM 侧数据构建成 FS 视图。
///
/// 幂等：重复调用同一批数据得到相同视图（构建逻辑不掺时间戳、
/// 不掺随机数——投影的确定性是可测试性的前提）。
///
/// # Errors
///
/// 数据里的名字撞了文件系统的安全约束（比如名字里带 `/`——
/// 文件名不能有路径分隔符，如实报错而不是静默改名：**映射约定
/// 不做暗地的字符替换**，改名策略属于调用方的数据清洗职责）。
pub fn build_im_view(contacts: &[Contact], messages: &[ChatMessage]) -> Result<MemFs, FsError> {
    let mut fs = MemFs::new();
    fs.mkdir(layout::CONTACTS_DIR)?;
    fs.mkdir(layout::HISTORY_DIR)?;
    fs.mkdir(layout::FILES_DIR)?;

    // 联系人名片：一行 JSON 风格的键值（够 cat/grep 用，不上 JSON 库——
    // 语义层零依赖的纪律，格式复杂化是视图消费者的事）
    for contact in contacts {
        let card = format!("name: {}\nusername: {}\n", contact.name, contact.username);
        fs.write_file(&layout::contact_card(&contact.name), card)?;
    }

    // 消息历史：按 peer 建目录、按日期归档文件。
    // 逐条 write_file 会反复覆写同一天文件——先在内存里按 (peer, date)
    // 聚合成整段日志再一次写入（教科书式的「先聚合再落盘」，
    // 也让本函数天然幂等）
    use std::collections::BTreeMap;
    let mut by_day: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for message in messages {
        by_day
            .entry((message.peer.clone(), message.date.clone()))
            .or_default()
            .push(format!("{}: {}", message.from, message.text));
    }
    for ((peer, date), lines) in by_day {
        let dir = format!("{}/{}", layout::HISTORY_DIR, peer);
        // 同一 peer 的目录只建一次：write_file 不建父目录（memfs 的
        // 显式规则），mkdir 的 AlreadyExists 错误对幂等构建是噪音，
        // 如下探一下再建——「查询后行动」而不是「行动后收拾错误」
        if fs.lookup(&dir).is_err() {
            fs.mkdir(&dir)?;
        }
        let log = lines.join("\n") + "\n";
        fs.write_file(&layout::history_log(&peer, &date), log)?;
    }
    Ok(fs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memfs::NodeKind;

    fn contacts() -> Vec<Contact> {
        vec![
            Contact { name: "alice".into(), username: "alice_wx".into() },
            Contact { name: "bob".into(), username: "bob_01".into() },
        ]
    }

    fn messages() -> Vec<ChatMessage> {
        vec![
            ChatMessage {
                peer: "alice".into(),
                from: "alice".into(),
                text: "早上好".into(),
                date: "2026-09-24".into(),
            },
            ChatMessage {
                peer: "alice".into(),
                from: "me".into(),
                text: "早上好！".into(),
                date: "2026-09-24".into(),
            },
            ChatMessage {
                peer: "alice".into(),
                from: "alice".into(),
                text: "昨天的文件收到没？".into(),
                date: "2026-09-23".into(),
            },
            ChatMessage {
                peer: "bob".into(),
                from: "bob".into(),
                text: "在吗".into(),
                date: "2026-09-24".into(),
            },
        ]
    }

    #[test]
    fn view_maps_contacts_and_history() {
        let fs = build_im_view(&contacts(), &messages()).unwrap();

        // 名片
        let card = fs.read(&layout::contact_card("alice")).unwrap();
        let card_text = String::from_utf8(card.to_vec()).unwrap();
        assert_eq!(card_text, "name: alice\nusername: alice_wx\n");

        // 根目录布局：三个固定目录
        let root = fs.read_dir("/").unwrap();
        let names: Vec<&str> = root.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["contacts", "files", "history"]);
        assert!(root.iter().all(|e| e.kind == NodeKind::Directory));

        // 联系人目录：两张名片
        let cards = fs.read_dir(layout::CONTACTS_DIR).unwrap();
        assert_eq!(cards.len(), 2);

        // 历史：alice 两天、bob 一天
        let alice_days = fs.read_dir("/history/alice").unwrap();
        let dates: Vec<&str> = alice_days.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(dates, vec!["2026-09-23.log", "2026-09-24.log"], "字典序且按日归档");
        let bob_days = fs.read_dir("/history/bob").unwrap();
        assert_eq!(bob_days.len(), 1);
    }

    #[test]
    fn same_day_messages_aggregate_into_one_log() {
        let fs = build_im_view(&contacts(), &messages()).unwrap();
        let log = fs.read(&layout::history_log("alice", "2026-09-24")).unwrap();
        let text = String::from_utf8(log.to_vec()).unwrap();
        assert_eq!(text, "alice: 早上好\nme: 早上好！\n", "同日两条聚合且保持时序");
    }

    #[test]
    fn build_is_idempotent() {
        // 同一批数据两次构建：视图逐字节相同（确定性投影）
        let a = build_im_view(&contacts(), &messages()).unwrap();
        let b = build_im_view(&contacts(), &messages()).unwrap();
        let log = layout::history_log("alice", "2026-09-24");
        assert_eq!(a.read(&log), b.read(&log));
        assert_eq!(a.read_dir("/").unwrap(), b.read_dir("/").unwrap());
    }

    #[test]
    fn slash_in_name_is_rejected_not_silently_renamed() {
        let bad = vec![Contact { name: "a/b".into(), username: "x".into() }];
        let result = build_im_view(&bad, &[]);
        // 名字带路径分隔符：write_file 会把它当嵌套路径 → 父目录不存在，
        // 报 NotFound——不静默改名（映射约定不做暗地字符替换）
        assert!(result.is_err());
    }

    #[test]
    fn view_composes_with_dir_cache() {
        // 组装件全通：视图 + LRU 目录缓存（阶段 13 三个模块合体）
        use crate::dir_cache::CachedFs;
        let fs = build_im_view(&contacts(), &messages()).unwrap();
        let mut cached = CachedFs::new(fs, 8);
        cached.read_dir("/history/alice").unwrap();
        cached.read_dir("/history/alice").unwrap();
        let (hits, misses) = cached.cache_stats();
        assert_eq!((hits, misses), (1, 1));
        // 读日志文件直通语义层
        let log = cached.read(&layout::history_log("alice", "2026-09-24")).unwrap();
        assert!(log.starts_with(b"alice: "));
    }
}
