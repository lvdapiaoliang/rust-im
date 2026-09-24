// 与后端 web 模块对应的类型定义。
// 约定：所有雪花 ID 一律是字符串（63 位超出 JS Number 安全范围，
// 数字形态会静默丢精度——与后端 web::serde_id / WS 信封同一约定）。

/** 用户（REST 出入参与本地状态共用）。 */
export interface User {
  id: string
  username: string
  display_name: string
}

/** 我加入的群。 */
export interface MyGroup {
  id: string
  name: string
  owner_id: string
  role: 'owner' | 'member'
}

/** 会话列表项（好友或群的统一形态，聊天窗按它路由消息）。 */
export interface Conversation {
  /** 消息收发目标 ID（好友用户 ID 或群 ID）。 */
  id: string
  kind: 'friend' | 'group'
  /** 展示名。 */
  name: string
}

/** 消息内容：阶段 5 只有文本；阶段 6 扩展 {"kind":"file"|...}。 */
export type MessageContent = string

/** 聊天消息（本地状态：含发送状态机）。 */
export interface ChatMessage {
  /** 服务端分配的全局 ID（本地乐观消息为空）。 */
  msg_id?: string
  /** 客户端去重键（跨重发稳定，ack 核销用）。 */
  client_msg_id: number
  from: string
  to: string
  content: MessageContent
  /** sending = 已发出未确认；sent = 服务端已接管（有 msg_id）。 */
  status: 'sending' | 'sent'
  ts: number
}

/** WS 信封（协议见 docs/12-web-protocol.md）。 */
export interface Envelope {
  type: string
  seq: number
  ack: number
  payload: unknown
}

/** 下行 msg 信封载荷。 */
export interface MsgPayload {
  from: string
  to: string
  msg_id: string
  client_msg_id: string
  content: MessageContent
}

/** 下行 msg_ack 信封载荷。 */
export interface MsgAckPayload {
  msg_id: string
  client_msg_id: string
}

/** 下行 sync_resp 信封载荷。 */
export interface SyncRespPayload {
  messages: MsgPayload[]
}

/** 下行 welcome 信封载荷。 */
export interface WelcomePayload {
  session_id: string
  reason: string
}

/** 下行 error 信封载荷。 */
export interface ErrorPayload {
  code: string
  message: string
}
