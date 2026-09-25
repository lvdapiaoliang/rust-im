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

/** 群成员视图（GET /api/groups/{id}/members 的项）。 */
export interface GroupMember {
  id: string
  username: string
  display_name: string
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

/** 消息正文（判别联合，kind 标签区分形态——与 docs/12-web-protocol.md §消息内容模型对应）。 */
export type MessageBody =
  | { kind: 'text'; text: string }
  | { kind: 'emoji'; emoji: string }
  | { kind: 'image'; file_id: string; filename: string; size_bytes: number }
  | { kind: 'file'; file_id: string; filename: string; size_bytes: number }

/**
 * 消息内容：新消息是 MessageBody（对象）；阶段 5 的旧消息可能是裸字符串，
 * 展示层统一经 parseContent 归一（服务端从不解释 content，兼容是纯前端职责）。
 */
export type MessageContent = MessageBody | string

/** 内容归一：裸字符串/无法识别的对象都降级为 text（展示降级优于丢消息）。 */
export function parseContent(content: MessageContent): MessageBody {
  if (typeof content === 'string') return { kind: 'text', text: content }
  if (content !== null && typeof content === 'object' && 'kind' in content) {
    const body = content as MessageBody
    if (body.kind === 'text' || body.kind === 'emoji' || body.kind === 'image' || body.kind === 'file') {
      return body
    }
  }
  return { kind: 'text', text: JSON.stringify(content) }
}

/** 聊天消息（本地状态：含发送状态机）。 */
export interface ChatMessage {
  /** 服务端分配的全局 ID（本地乐观消息为空）。 */
  msg_id?: string
  /** 客户端去重键（跨重发稳定，ack 核销用）。 */
  client_msg_id: number
  from: string
  to: string
  content: MessageContent
  /** sending = 已发出未确认；sent = 服务端已接管；failed = 服务端拒绝（如非好友）。 */
  status: 'sending' | 'sent' | 'failed'
  /** 拒绝原因（failed 时展示）。 */
  failReason?: string
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

/** 下行 error 信封载荷（client_msg_id 仅在消息被拒时携带——关联乐观消息用）。 */
export interface ErrorPayload {
  code: string
  message: string
  client_msg_id?: string
}

/** 文件元数据（POST /api/files 响应）。 */
export interface FileMeta {
  id: string
  owner_id: string
  filename: string
  size_bytes: number
  sha256: string
}

/** 好友请求视图（REST 出参与事件载荷共用）。 */
export interface FriendRequestView {
  id: string
  from_user: string
  from_username: string
  to_user: string
  to_username: string
  status: string
}

/** 下行 event 信封载荷（好友事件；kind 判别，后端新增事件零协议改动）。 */
export type FriendEvent =
  | { kind: 'friend_request'; request: FriendRequestView }
  | { kind: 'friend_accepted'; user: User; by: User }
  | { kind: 'friend_removed'; user: User }
