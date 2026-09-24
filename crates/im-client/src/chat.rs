//! `ChatState`：会话状态 reducer（阶段 4）。
//!
//! UI 状态 = fold(事件流)：把 [`ClientEvent`] 依次吃进纯状态转移
//! 函数，任何时刻的状态完全由「事件序列 + 初始状态」决定——
//! Redux 风格的 reducer。收益：
//!
//! - **UI 无状态**：TUI（阶段 4 后半）只做两件事——渲染状态、
//!   把按键翻译成命令。渲染逻辑零分支噪声；
//! - **可测试**：测试 reducer 不需要起服务端/客户端/定时器，
//!   「喂事件序列、断言状态」就是全部（本模块的测试就是这么写的）。
//!
//! # 算法/模式落点
//!
//! - **BTreeMap<u64, Conversation>**：会话按 peer 有序（侧栏列表
//!   天然字典序），B 树思想（有序 + 范围扫描）；
//! - **reducer 模式**：事件驱动的纯状态机（事件溯源的内存版——
//!   事件流即真相，状态只是缓存）。

use std::collections::BTreeMap;

use bytes::Bytes;
use im_protocol::Msg;

use crate::client::ClientEvent;

/// 单条出站消息的投递状态（入站消息天然是 [`SendStatus::Delivered`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendStatus {
    /// 已入重发表、未确认（UI 显示「转圈」）。
    Sending,
    /// 服务端已确认（Ack 核销）。
    Delivered,
    /// 重试耗尽，放弃（UI 显示红色感叹号）。
    Failed,
}

/// 一条会话内消息（UI 视图模型：入站出站统一形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMsg {
    /// 是否本人发出（决定气泡左右侧）。
    pub from_me: bool,
    /// 对端 ID（会话归属）。
    pub peer: u64,
    /// 消息内容。
    pub content: Bytes,
    /// 发送方本地去重键（出站消息的核销索引）。
    pub client_msg_id: u64,
    /// 服务端全局 ID（出站消息 Ack 前为 `None`）。
    pub msg_id: Option<u64>,
    /// 投递状态。
    pub status: SendStatus,
}

/// 一个会话：与某对端的消息流 + 未读数。
#[derive(Debug, Default)]
pub struct Conversation {
    /// 按到达顺序追加（时间序即展示序）。
    pub messages: Vec<ChatMsg>,
    /// 未读条数（选中会话时由 UI 清零）。
    pub unread: u32,
}

/// 聊天状态（reducer 的累积结果）。
#[derive(Debug, Default)]
pub struct ChatState {
    conversations: BTreeMap<u64, Conversation>,
    connected: bool,
}

impl ChatState {
    /// 初始状态（未连接、无会话）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// reducer：吃一个事件，转移状态。纯函数——无 IO、无时钟、无随机。
    ///
    /// `self_id` 用于把「我发的消息回显」（自己的多端同步）识别为
    /// 出站方向；当前单端登录下仅起防御作用。
    pub fn on_event(&mut self, event: &ClientEvent, self_id: u64) {
        match event {
            ClientEvent::Connected { .. } => self.connected = true,
            ClientEvent::Disconnected => self.connected = false,
            ClientEvent::Rejected { .. } => self.connected = false,
            ClientEvent::MessageQueued {
                client_msg_id,
                to,
                content,
            } => {
                // 发送中：转圈条目登记进与接收者的会话
                self.entry(*to).messages.push(ChatMsg {
                    from_me: true,
                    peer: *to,
                    content: content.clone(),
                    client_msg_id: *client_msg_id,
                    msg_id: None,
                    status: SendStatus::Sending,
                });
            }
            ClientEvent::Message(msg) => {
                let _ = self_id; // 单端登录：Message 事件只来自对端
                self.accept_incoming(msg);
            }
            ClientEvent::SyncBatch(messages) => {
                for msg in messages {
                    self.accept_incoming(msg);
                }
            }
            ClientEvent::Ack {
                msg_id,
                client_msg_id,
            } => {
                // 核销「转圈」条目：client_msg_id 是跨重发稳定的索引。
                // 新消息在尾部，倒序找平均更快（手速有限，条目很少）
                'outer: for conversation in self.conversations.values_mut() {
                    for msg in conversation.messages.iter_mut().rev() {
                        if msg.from_me && msg.client_msg_id == *client_msg_id {
                            msg.status = SendStatus::Delivered;
                            msg.msg_id = Some(*msg_id);
                            break 'outer;
                        }
                    }
                }
            }
            ClientEvent::SendFailed { client_msg_id } => {
                'outer: for conversation in self.conversations.values_mut() {
                    for msg in conversation.messages.iter_mut().rev() {
                        if msg.from_me && msg.client_msg_id == *client_msg_id {
                            msg.status = SendStatus::Failed;
                            break 'outer;
                        }
                    }
                }
            }
        }
    }

    /// 会话列表（peer 升序——BTreeMap 迭代天然有序）。
    pub fn peers(&self) -> Vec<u64> {
        self.conversations.keys().copied().collect()
    }

    /// 某个会话（无会话返回 `None`）。
    pub fn conversation(&self, peer: u64) -> Option<&Conversation> {
        self.conversations.get(&peer)
    }

    /// 是否在线。
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// 标记已读（选中会话时由 UI 调用）。
    pub fn mark_read(&mut self, peer: u64) {
        if let Some(conversation) = self.conversations.get_mut(&peer) {
            conversation.unread = 0;
        }
    }

    /// 入站消息进对应会话。幂等：同 `(from, client_msg_id)` 只记一次——
    /// 事件流上游已有去重窗口，这里是防御性兜底（reducer 幂等 =
    /// 事件重放永远安全，这是事件溯源风格的前提）。
    fn accept_incoming(&mut self, msg: &Msg) {
        let conversation = self.entry(msg.from);
        if conversation
            .messages
            .iter()
            .any(|m| !m.from_me && m.client_msg_id == msg.client_msg_id)
        {
            return;
        }
        conversation.messages.push(ChatMsg {
            from_me: false,
            peer: msg.from,
            content: msg.content.clone(),
            client_msg_id: msg.client_msg_id,
            msg_id: Some(msg.msg_id),
            status: SendStatus::Delivered,
        });
        conversation.unread += 1;
    }

    /// 取（或建）与 peer 的会话。
    fn entry(&mut self, peer: u64) -> &mut Conversation {
        self.conversations.entry(peer).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incoming(from: u64, client_msg_id: u64, msg_id: u64, text: &str) -> ClientEvent {
        ClientEvent::Message(Msg {
            from,
            to: 9,
            msg_id,
            client_msg_id,
            content: Bytes::copy_from_slice(text.as_bytes()),
        })
    }

    /// 出站生命周期：转圈 → 送达。
    #[test]
    fn outgoing_lifecycle_sending_to_delivered() {
        let mut state = ChatState::new();
        state.on_event(
            &ClientEvent::MessageQueued {
                client_msg_id: 1,
                to: 7,
                content: Bytes::from_static(b"hello"),
            },
            9,
        );
        let conversation = state.conversation(7).expect("会话已建立");
        assert_eq!(conversation.messages.len(), 1);
        assert_eq!(conversation.messages[0].status, SendStatus::Sending);
        assert_eq!(conversation.messages[0].msg_id, None);

        state.on_event(
            &ClientEvent::Ack {
                msg_id: 500,
                client_msg_id: 1,
            },
            9,
        );
        let conversation = state.conversation(7).expect("会话已建立");
        assert_eq!(conversation.messages[0].status, SendStatus::Delivered);
        assert_eq!(conversation.messages[0].msg_id, Some(500));
    }

    /// 发送失败：转圈 → 失败。
    #[test]
    fn send_failed_marks_failed() {
        let mut state = ChatState::new();
        state.on_event(
            &ClientEvent::MessageQueued {
                client_msg_id: 3,
                to: 2,
                content: Bytes::from_static(b"try"),
            },
            9,
        );
        state.on_event(&ClientEvent::SendFailed { client_msg_id: 3 }, 9);
        assert_eq!(
            state.conversation(2).unwrap().messages[0].status,
            SendStatus::Failed
        );
    }

    /// 入站：进对应会话、未读递增；重复事件幂等。
    #[test]
    fn incoming_unread_and_idempotent() {
        let mut state = ChatState::new();
        state.on_event(&incoming(7, 100, 1000, "hi"), 9);
        state.on_event(&incoming(7, 100, 1000, "hi"), 9); // 重复投递
        let conversation = state.conversation(7).unwrap();
        assert_eq!(conversation.messages.len(), 1, "重复消息只记一次");
        assert_eq!(conversation.unread, 1, "未读不虚增");

        state.mark_read(7);
        assert_eq!(state.conversation(7).unwrap().unread, 0);

        // 另一会话互不影响
        state.on_event(&incoming(8, 100, 1001, "yo"), 9);
        assert_eq!(state.peers(), vec![7, 8], "会话按 peer 升序");
        assert_eq!(state.conversation(8).unwrap().unread, 1);
    }

    /// 离线补投批量入库；连接状态随事件切换。
    #[test]
    fn sync_batch_and_connection_state() {
        let mut state = ChatState::new();
        assert!(!state.is_connected());
        state.on_event(&ClientEvent::Connected { session_id: 42 }, 9);
        assert!(state.is_connected());

        let batch = ClientEvent::SyncBatch(vec![
            Msg {
                from: 7,
                to: 9,
                msg_id: 10,
                client_msg_id: 1,
                content: Bytes::from_static(b"a"),
            },
            Msg {
                from: 7,
                to: 9,
                msg_id: 11,
                client_msg_id: 2,
                content: Bytes::from_static(b"b"),
            },
        ]);
        state.on_event(&batch, 9);
        assert_eq!(state.conversation(7).unwrap().messages.len(), 2);

        state.on_event(&ClientEvent::Disconnected, 9);
        assert!(!state.is_connected());
    }
}
