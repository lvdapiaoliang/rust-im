//! TUI：ratatui 三栏聊天界面（阶段 4）。
//!
//! 职责严格二分——这正是 [`crate::chat`] reducer 设计的兑现：
//!
//! - **渲染**：把 [`ChatState`] 画到终端（每轮循环全量重绘，
//!   ratatui 的 diff 只写变化的单元格，全量重绘不等于全量刷屏）；
//! - **输入**：把按键翻译成 [`ClientHandle`] 命令或本地 UI 状态
//!   （选中会话/输入缓冲/滚动偏移）。
//!
//! 业务逻辑为零——去重、重传、持久化全部在 `client_loop` 侧，
//! TUI 对协议一无所知。
//!
//! # 布局
//!
//! ```text
//! ┌────────┬──────────────────────┐
//! │ 会话    │ 消息区（当前会话）      │
//! │ 列表    │  ▶ 我发的（已送达）     │
//! │        │  ~ 我发的（发送中）     │
//! │        │  ✗ 我发的（失败）       │
//! │        │  < 对方发的             │
//! │        ├──────────────────────┤
//! │        │ > 输入框               │
//! └────────┴──────────────────────┘
//! ```
//!
//! # 按键
//!
//! | 按键 | 动作 |
//! |------|------|
//! | `Tab` | 在会话间轮转 |
//! | `Enter` | 发送输入行给当前会话 |
//! | `Backspace` | 删一个字符 |
//! | `PageUp` / `PageDown` | 消息区上/下翻 |
//! | `Esc` / `Ctrl-C` | 退出 |
//!
//! 输入行支持两个命令：`/to <user_id>`（切换或新建会话）与
//! `/quit`（退出）。

use bytes::Bytes;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};
use tokio::sync::mpsc;

use crate::chat::{ChatMsg, ChatState, SendStatus};
use crate::client::{ClientConfig, ClientEvent, ClientHandle, run_client};

/// TUI 入口：起客户端 + 跑界面循环，退出时恢复终端并关停客户端。
///
/// # Errors
///
/// 终端初始化/渲染 IO 失败时返回错误（原始屏幕已恢复）。
pub async fn run(config: ClientConfig) -> anyhow::Result<()> {
    let user_id = config.user_id;

    let (events_tx, mut events_rx) = mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = im_transport::shutdown_channel();
    let handle = run_client(config, events_tx, shutdown_rx).await;

    // init/restore 负责备用屏 + raw mode + panic hook（崩溃也恢复终端）
    let mut terminal = ratatui::init();
    let result = ui_loop(&mut terminal, &handle, &mut events_rx, user_id).await;
    ratatui::restore();
    shutdown_tx.trigger();
    result
}

/// 输入行的解析结果（纯函数，可单测）。
#[derive(Debug, PartialEq, Eq)]
enum InputAction {
    /// 发送内容给当前会话。
    Send { content: String },
    /// 切换（或新建）到指定会话。
    SwitchTo(u64),
    /// 退出。
    Quit,
    /// 无动作：空行、命令格式错误。
    Ignored,
}

/// 解析一行输入。命令以 `/` 开头，其余视为聊天内容。
fn parse_input(line: &str) -> InputAction {
    let line = line.trim();
    if line.is_empty() {
        return InputAction::Ignored;
    }
    if let Some(rest) = line.strip_prefix('/') {
        let (cmd, arg) = rest.split_once(' ').unwrap_or((rest, ""));
        return match (cmd, arg.trim().parse::<u64>()) {
            ("to", Ok(peer)) => InputAction::SwitchTo(peer),
            ("quit", _) => InputAction::Quit,
            // `/to` 格式错误与未知命令同样无动作（解析失败落入通配）
            _ => InputAction::Ignored,
        };
    }
    InputAction::Send { content: line.to_string() }
}

/// TUI 本地状态（不属于 [`ChatState`] 的「视口」部分）。
struct Viewport {
    /// 输入缓冲。
    input: String,
    /// 当前选中会话（`None` = 尚无）。
    selected: Option<u64>,
    /// 消息区向上滚动偏移（0 = 跟随最新）。
    scroll: u16,
    /// 输入框标题上的提示（最近一次的引导/错误信息）。
    hint: String,
}

impl Viewport {
    fn new() -> Self {
        Self {
            input: String::new(),
            selected: None,
            scroll: 0,
            hint: "/to <user_id> 新会话 · Tab 切换 · /quit 退出".to_string(),
        }
    }

    /// 选中某会话：清未读、回到底部。
    fn select(&mut self, chat: &mut ChatState, peer: u64) {
        self.selected = Some(peer);
        chat.mark_read(peer);
        self.scroll = 0;
        self.hint = format!("与 {peer} 对话中 · Enter 发送");
    }
}

/// 主循环：每轮「重绘 → select 等待（按键 / 客户端事件）」。
///
/// 事件到达才重绘（不空转），两类事件都会改变屏幕内容。
async fn ui_loop(
    terminal: &mut ratatui::DefaultTerminal,
    handle: &ClientHandle,
    events: &mut mpsc::Receiver<ClientEvent>,
    user_id: u64,
) -> anyhow::Result<()> {
    let mut chat = ChatState::new();
    let mut view = Viewport::new();
    let mut keys = EventStream::new();

    loop {
        terminal.draw(|f| draw(f, &chat, &view, user_id))?;

        tokio::select! {
            key = keys.next() => {
                let Some(Ok(Event::Key(k))) = key else {
                    continue; // 未知终端事件（焦点/ resize 单独处理）
                };
                if k.kind != KeyEventKind::Press {
                    continue; // Windows 终端会同时上报 Press/Release
                }
                match (k.code, k.modifiers) {
                    (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    (KeyCode::Tab, _) => {
                        if let Some(next) = next_peer(&chat, view.selected) {
                            view.select(&mut chat, next);
                        }
                    }
                    (KeyCode::PageUp, _) => view.scroll = view.scroll.saturating_add(5),
                    (KeyCode::PageDown, _) => view.scroll = view.scroll.saturating_sub(5),
                    (KeyCode::Backspace, _) => {
                        view.input.pop();
                    }
                    (KeyCode::Enter, _) => match parse_input(&view.input) {
                        InputAction::Quit => return Ok(()),
                        InputAction::SwitchTo(peer) => {
                            view.select(&mut chat, peer);
                            view.input.clear();
                        }
                        InputAction::Send { content } => {
                            if let Some(peer) = view.selected {
                                let content = Bytes::copy_from_slice(content.as_bytes());
                                // 客户端已退出（如被拒）时发送失败——界面层面
                                // 无事可做：事件流随后也会结束，循环自然退出
                                let _ = handle.send_msg(peer, content).await;
                                view.scroll = 0;
                                view.input.clear();
                            } else {
                                view.hint = "先 /to <user_id> 选择会话".to_string();
                            }
                        }
                        InputAction::Ignored => view.input.clear(),
                    },
                    (KeyCode::Char(c), _) => view.input.push(c),
                    _ => {}
                }
            }
            event = events.recv() => {
                let Some(event) = event else {
                    // 客户端退出（登录被拒/内部错误）：跟着退
                    return Ok(());
                };
                chat.on_event(&event, user_id);
                // 没有选中会话时自动跟进最新出现的会话（第一条消息
                // 不该被「先 /to」挡在门外）
                if view.selected.is_none() {
                    if let Some(&peer) = chat.peers().last() {
                        view.select(&mut chat, peer);
                    }
                }
                // 正盯着的会话来消息 = 正在看，即刻清未读
                if let Some(peer) = view.selected {
                    chat.mark_read(peer);
                }
            }
        }
    }
}

/// Tab 轮转的下一会话：无会话返回 `None`。
fn next_peer(chat: &ChatState, current: Option<u64>) -> Option<u64> {
    let peers = chat.peers();
    if peers.is_empty() {
        return None;
    }
    let next = match current {
        None => peers[0],
        Some(cur) => {
            peers.iter().position(|p| *p == cur).map_or(peers[0], |i| peers[(i + 1) % peers.len()])
        }
    };
    Some(next)
}

/// 一轮完整渲染（纯函数：只读状态、只画）。
fn draw(f: &mut Frame, chat: &ChatState, view: &Viewport, user_id: u64) {
    let [sidebar, main] =
        Layout::horizontal([Constraint::Length(22), Constraint::Min(0)]).areas(f.area());
    let [messages_area, input_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).areas(main);

    draw_sidebar(f, chat, view, sidebar);
    draw_messages(f, chat, view, messages_area);
    draw_input(f, view, input_area, user_id);
}

/// 左栏：会话列表（选中 ▸ 标记 + 未读计数）。
fn draw_sidebar(f: &mut Frame, chat: &ChatState, view: &Viewport, area: ratatui::layout::Rect) {
    let items: Vec<Line> = chat
        .peers()
        .iter()
        .map(|&peer| {
            let unread = chat.conversation(peer).map_or(0, |c| c.unread);
            let marker = if view.selected == Some(peer) { "▸ " } else { "  " };
            if unread > 0 {
                Line::from(format!("{marker}{peer} [{unread}]")).fg(Color::Yellow)
            } else {
                Line::from(format!("{marker}{peer}"))
            }
        })
        .collect();
    f.render_widget(Paragraph::new(items).block(Block::bordered().title(" 会话 ")), area);
}

/// 右上：当前会话的消息流。
fn draw_messages(f: &mut Frame, chat: &ChatState, view: &Viewport, area: ratatui::layout::Rect) {
    let (title, lines) = match view.selected.and_then(|p| chat.conversation(p).map(|c| (p, c))) {
        Some((peer, conversation)) => {
            (format!(" 与 {peer} 的对话 "), conversation.messages.iter().map(render_msg).collect())
        }
        None => (" 尚无会话 ".to_string(), vec![Line::from("输入 /to <user_id> 开始聊天")]),
    };
    f.render_widget(
        Paragraph::new(lines).scroll((view.scroll, 0)).block(Block::bordered().title(title)),
        area,
    );
}

/// 右下：输入框 + 提示 + 光标。
fn draw_input(f: &mut Frame, view: &Viewport, area: ratatui::layout::Rect, user_id: u64) {
    let line = Line::from(format!("> {}", view.input));
    // Line::width 按显示宽度计（中文占 2 列也算对），光标定位不偏移
    let text_width = u16::try_from(line.width()).unwrap_or(u16::MAX);
    let cursor_x = area.x + 2 + text_width.min(area.width.saturating_sub(3));

    f.render_widget(
        Paragraph::new(line)
            .block(Block::bordered().title(format!(" [user {user_id}] {} ", view.hint))),
        area,
    );
    f.set_cursor_position((cursor_x, area.y + 1));
}

/// 一条消息渲染成一行：
/// 出站按状态着色（送达绿/发送中灰/失败红），入站 `< ` 前缀。
fn render_msg(msg: &ChatMsg) -> Line<'static> {
    let text = display_text(&msg.content);
    if msg.from_me {
        match msg.status {
            SendStatus::Delivered => Line::from(format!("▶ {text}")).fg(Color::Green),
            SendStatus::Sending => Line::from(format!("~ {text}")).fg(Color::DarkGray),
            SendStatus::Failed => Line::from(format!("✗ {text}")).fg(Color::Red),
        }
    } else {
        Line::from(format!("< {text}"))
    }
}

/// 消息内容 → 展示文本（阶段 6 富媒体降级）：
///
/// 服务端的 `content` 是不透明字节，Web 端会发 `{"kind":"file"|...}` 对象。
/// TCP/TUI 只认文本——能解析出 kind 标签就降级为 `[文件] 名字` 形态，
/// 解析不了按老规矩 lossy 原样展示（降级显示优于丢消息，二进制协议不膨胀）。
fn display_text(content: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(content) else {
        return String::from_utf8_lossy(content).into_owned();
    };
    let Some(kind) = value.get("kind").and_then(serde_json::Value::as_str) else {
        return String::from_utf8_lossy(content).into_owned();
    };
    let filename = || value.get("filename").and_then(serde_json::Value::as_str).unwrap_or("");
    match kind {
        "text" => value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        // 表情直接展示（Unicode emoji 在终端里本身就是文本）
        "emoji" => value
            .get("emoji")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("[表情]")
            .to_string(),
        "image" => format!("[图片] {}", filename()),
        "file" => format!("[文件] {}", filename()),
        _ => String::from_utf8_lossy(content).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_send_content() {
        assert_eq!(parse_input("你好"), InputAction::Send { content: "你好".to_string() });
        assert_eq!(
            parse_input("  带空格 的内容  "),
            InputAction::Send { content: "带空格 的内容".to_string() }
        );
    }

    #[test]
    fn parse_commands() {
        assert_eq!(parse_input("/to 42"), InputAction::SwitchTo(42));
        assert_eq!(parse_input("/to abc"), InputAction::Ignored);
        assert_eq!(parse_input("/quit"), InputAction::Quit);
        assert_eq!(parse_input("/unknown x"), InputAction::Ignored);
    }

    #[test]
    fn parse_empty_is_ignored() {
        assert_eq!(parse_input(""), InputAction::Ignored);
        assert_eq!(parse_input("   "), InputAction::Ignored);
    }

    /// 富媒体降级：纯文本/非 kind JSON 原样展示，kind 对象降级。
    #[test]
    fn display_text_degrades_rich_content() {
        // 纯文本（阶段 4 的原生形态）：原样
        assert_eq!(display_text(b"hello"), "hello");
        // JSON 但无 kind 标签：不是内容模型，原样
        assert_eq!(display_text(br#"{"a":1}"#), r#"{"a":1}"#);
        // Web 端新形态：text 提取正文
        assert_eq!(display_text(br#"{"kind":"text","text":"你好"}"#), "你好");
        // 表情直接展示（终端里 emoji 本就是文本）
        assert_eq!(display_text(br#"{"kind":"emoji","emoji":"👍"}"#), "👍");
        // 文件/图片降级为占位提示 + 文件名
        assert_eq!(
            display_text(br#"{"kind":"file","file_id":"7","filename":"a.pdf","size_bytes":1}"#),
            "[文件] a.pdf"
        );
        assert_eq!(
            display_text(br#"{"kind":"image","file_id":"8","filename":"p.png","size_bytes":2}"#),
            "[图片] p.png"
        );
    }

    /// Tab 轮转：无会话 → None；有会话循环切换。
    #[test]
    fn next_peer_rotates() {
        let mut chat = ChatState::new();
        assert_eq!(next_peer(&chat, None), None);

        // 造两个会话（peer 2、7）
        for peer in [2, 7] {
            chat.on_event(
                &ClientEvent::MessageQueued { client_msg_id: 1, to: peer, content: Bytes::new() },
                9,
            );
        }
        assert_eq!(next_peer(&chat, None), Some(2));
        assert_eq!(next_peer(&chat, Some(2)), Some(7));
        assert_eq!(next_peer(&chat, Some(7)), Some(2)); // 回绕
        assert_eq!(next_peer(&chat, Some(99)), Some(2)); // 已不存在的选中
    }
}
