// 会话与消息：好友/群列表 + 消息收发 + 离线同步 + 好友关系管理。
//
// 消息状态机（与后端协议对应）：
//   本地乐观插入（sending）→ ws 发出
//     → msg_ack 核销（sent，有 msg_id）
//     → error 信封（failed，如非好友被拒——载荷带 client_msg_id 可精确定位）
// 重连后 sync（since = 已见最大 msg_id）补投离线消息。
//
// 消息内容模型（阶段 6）：content 是判别联合 {"kind":...}（见 types.ts）；
// 服务端视为不透明字节，旧消息的裸字符串在展示层 parseContent 归一。

import { ref, computed } from 'vue'
import { defineStore } from 'pinia'
import { http } from '@/api/http'
import { useAuthStore } from './auth'
import { useWsStore } from './ws'
import type {
  ChatMessage,
  Conversation,
  ErrorPayload,
  FileMeta,
  FriendEvent,
  FriendRequestView,
  GroupMember,
  MessageBody,
  MyGroup,
  MsgPayload,
  MsgAckPayload,
  SyncRespPayload,
  User,
} from '@/types'

export const useChatStore = defineStore('chat', () => {
  const auth = useAuthStore()
  const ws = useWsStore()

  const friends = ref<User[]>([])
  const groups = ref<MyGroup[]>([])
  /** 群成员缓存：群 ID → 成员列表（进群会话时拉取，发送者名字反查用）。 */
  const members = ref<Record<string, GroupMember[]>>({})
  /** 收到的好友请求（待处理）。 */
  const incomingRequests = ref<FriendRequestView[]>([])
  /** 会话消息：peerId（好友或群）→ 按时间升序的消息数组。 */
  const messages = ref<Record<string, ChatMessage[]>>({})
  const activeId = ref<string | null>(null)

  /** 会话列表：好友 + 群（统一形态供列表渲染）。 */
  const conversations = computed<Conversation[]>(() => [
    ...friends.value.map((u) => ({ id: u.id, kind: 'friend' as const, name: u.display_name })),
    ...groups.value.map((g) => ({ id: g.id, kind: 'group' as const, name: g.name })),
  ])

  /** 当前会话的消息。 */
  const activeMessages = computed(() =>
    activeId.value === null ? [] : (messages.value[activeId.value] ?? []),
  )

  /** client_msg_id 生成器：进程内单调（重发复用同一值）。 */
  let clientSeq = 0

  /** 已见最大 msg_id（同步游标）。 */
  let lastMsgId = 0

  function append(peerId: string, message: ChatMessage): void {
    const list = messages.value[peerId] ?? []
    list.push(message)
    messages.value[peerId] = list
  }

  /** 拉好友与群列表。 */
  async function loadContacts(): Promise<void> {
    const [friendList, groupList] = await Promise.all([
      http.get<User[]>('/api/friends'),
      http.get<MyGroup[]>('/api/groups'),
    ])
    friends.value = friendList
    groups.value = groupList
  }

  /** 拉收到的待处理好友请求。 */
  async function loadRequests(): Promise<void> {
    const resp = await http.get<{ incoming: FriendRequestView[]; outgoing: FriendRequestView[] }>(
      '/api/friends/requests',
    )
    incomingRequests.value = resp.incoming
  }

  /** 按用户名精确查找用户（加好友入口；null = 没找到）。 */
  function searchUser(username: string): Promise<User | null> {
    return http.get<User | null>(`/api/users?username=${encodeURIComponent(username)}`)
  }

  /** 发起好友请求。 */
  async function sendFriendRequest(userId: string): Promise<void> {
    await http.post<FriendRequestView>('/api/friends/requests', { to: userId })
  }

  /** 接受请求：对方立即出现在我的好友列表（事件是推给对方的，我这侧直接改本地态）。 */
  async function acceptRequest(requestId: string): Promise<void> {
    await http.post(`/api/friends/requests/${requestId}/accept`)
    incomingRequests.value = incomingRequests.value.filter((r) => r.id !== requestId)
    await loadContacts()
  }

  /** 拒绝请求（服务端不推事件——拒绝是「没有事情发生」）。 */
  async function rejectRequest(requestId: string): Promise<void> {
    await http.post(`/api/friends/requests/${requestId}/reject`)
    incomingRequests.value = incomingRequests.value.filter((r) => r.id !== requestId)
  }

  /** 删除好友：本地立刻下架（对方靠 friend_removed 事件同步）。 */
  async function removeFriend(userId: string): Promise<void> {
    await http.delete(`/api/friends/${userId}`)
    friends.value = friends.value.filter((u) => u.id !== userId)
    if (activeId.value === userId) activeId.value = null
  }

  // ── 群组管理（阶段 7）──
  // 群消息收发不需要新 action：conversations/ingestIncoming/sendBody
  // 的 to 原生兼容群 ID；这里只补管理动作与成员名字反查。

  /** 建群：创建者即群主（后端响应不含 role，本地补齐）。 */
  async function createGroup(name: string): Promise<MyGroup> {
    const g = await http.post<{ id: string; name: string; owner_id: string }>('/api/groups', {
      name,
    })
    const mine: MyGroup = { ...g, role: 'owner' }
    if (!groups.value.some((x) => x.id === mine.id)) groups.value.push(mine)
    return mine
  }

  /** 拉群成员（缓存优先——重复进会话不重拉）。 */
  async function loadMembers(groupId: string): Promise<void> {
    if (members.value[groupId] !== undefined) return
    members.value[groupId] = await http.get<GroupMember[]>(`/api/groups/${groupId}/members`)
  }

  /** 拉人入群（仅群主；成功后失效重拉成员缓存）。 */
  async function addGroupMember(groupId: string, userId: string): Promise<void> {
    await http.post(`/api/groups/${groupId}/members`, { user_id: userId })
    delete members.value[groupId]
    await loadMembers(groupId)
  }

  /** 群内发送者显示名：成员缓存反查 → 好友表 → ID 尾号降级。 */
  function memberName(groupId: string, userId: string): string {
    const m = members.value[groupId]?.find((x) => x.id === userId)
    if (m !== undefined) return m.display_name
    const f = friends.value.find((u) => u.id === userId)
    if (f !== undefined) return f.display_name
    return `用户 ${userId.slice(-6)}`
  }

  /** 乐观插入 + 发出（所有发送形态共用：文本/表情/文件只是 content 不同）。 */
  function sendBody(to: string, body: MessageBody): void {
    const me = auth.user
    if (me === null) return
    clientSeq += 1
    append(to, {
      client_msg_id: clientSeq,
      from: me.id,
      to,
      content: body,
      status: 'sending',
      ts: Date.now(),
    })
    ws.send('msg', { to, client_msg_id: clientSeq, content: body })
  }

  /** 发送文本。 */
  function sendText(to: string, text: string): void {
    if (text.trim() === '') return
    sendBody(to, { kind: 'text', text })
  }

  /** 发送表情（Unicode emoji 原样放行；自定义表情走图片消息）。 */
  function sendEmoji(to: string, emoji: string): void {
    if (emoji === '') return
    sendBody(to, { kind: 'emoji', emoji })
  }

  /** 发送文件/图片消息（元数据进消息体，字节仍在文件服务）。 */
  function sendFileMessage(to: string, meta: FileMeta, kind: 'file' | 'image'): void {
    sendBody(to, {
      kind,
      file_id: meta.id,
      filename: meta.filename,
      size_bytes: meta.size_bytes,
    })
  }

  /** 上传并发送：图片按 image 发（前端可按 kind 决定渲染方式），其余按 file。 */
  async function uploadAndSend(to: string, file: File): Promise<void> {
    const meta = await http.upload<FileMeta>('/api/files', file)
    sendFileMessage(to, meta, file.type.startsWith('image/') ? 'image' : 'file')
  }

  /** 下行消息入库（含离线补投的）。 */
  function ingestIncoming(payload: MsgPayload): void {
    const me = auth.user
    if (me === null) return
    // 群消息按群 ID 归档（to 是群）；单聊按对端归档（from 是对方
    // ——服务端只把消息投给接收者，from===me 的单聊回显不存在）
    const isGroupMsg = groups.value.some((g) => g.id === payload.to)
    const peerId = isGroupMsg ? payload.to : payload.from
    append(peerId, {
      msg_id: payload.msg_id,
      client_msg_id: Number(payload.client_msg_id),
      from: payload.from,
      to: payload.to,
      content: payload.content,
      status: 'sent',
      ts: Date.now(),
    })
    lastMsgId = Math.max(lastMsgId, Number(payload.msg_id))
  }

  /** msg_ack：核销乐观消息（sending → sent）。 */
  function handleAck(payload: MsgAckPayload): void {
    const clientMsgId = Number(payload.client_msg_id)
    for (const list of Object.values(messages.value)) {
      const pending = list.find((m) => m.client_msg_id === clientMsgId && m.status === 'sending')
      if (pending) {
        pending.status = 'sent'
        pending.msg_id = payload.msg_id
        return
      }
    }
  }

  /** error 信封：带 client_msg_id 的（消息被拒）精确置为 failed。 */
  function handleError(payload: ErrorPayload): void {
    if (payload.client_msg_id === undefined) return
    const clientMsgId = Number(payload.client_msg_id)
    for (const list of Object.values(messages.value)) {
      const pending = list.find((m) => m.client_msg_id === clientMsgId && m.status === 'sending')
      if (pending) {
        pending.status = 'failed'
        pending.failReason = payload.message
        return
      }
    }
  }

  /** event 信封：好友事件（实时刷新列表/请求，离线方靠下次拉取兜底）。 */
  function handleFriendEvent(payload: FriendEvent): void {
    switch (payload.kind) {
      case 'friend_request':
        if (!incomingRequests.value.some((r) => r.id === payload.request.id)) {
          incomingRequests.value.push(payload.request)
        }
        break
      case 'friend_accepted':
        // 对方接受了我：立即出现在好友列表（by 是接受者）
        if (!friends.value.some((u) => u.id === payload.by.id)) {
          friends.value.push(payload.by)
        }
        break
      case 'friend_removed':
        friends.value = friends.value.filter((u) => u.id !== payload.user.id)
        if (activeId.value === payload.user.id) activeId.value = null
        break
    }
  }

  /** 离线同步：按游标拉一批（空批 = 没有更多）。 */
  function sync(): void {
    ws.send('sync', { since: lastMsgId })
  }

  /** 订阅 WS 信封（连接生命周期方调用一次；返回取消函数）。 */
  function bind(): () => void {
    const offs = [
      ws.on('msg', (payload) => ingestIncoming(payload as MsgPayload)),
      ws.on('msg_ack', (payload) => handleAck(payload as MsgAckPayload)),
      ws.on('error', (payload) => handleError(payload as ErrorPayload)),
      ws.on('event', (payload) => handleFriendEvent(payload as FriendEvent)),
      ws.on('sync_resp', (payload) => {
        const { messages: batch } = payload as SyncRespPayload
        for (const m of batch) ingestIncoming(m)
      }),
      ws.on('welcome', (payload, envelope) => {
        // welcome 到达 = 新连接就绪：立刻补投离线（空批也是一次确认）
        const w = payload as { session_id: string; reason: string }
        if (w.session_id !== '0') sync()
        else if (!w.reason.includes('already online')) {
          console.warn('welcome 拒绝:', w.reason, envelope)
        }
      }),
    ]
    return () => offs.forEach((off) => off())
  }

  return {
    friends,
    groups,
    members,
    incomingRequests,
    conversations,
    messages,
    activeId,
    activeMessages,
    loadContacts,
    loadRequests,
    searchUser,
    sendFriendRequest,
    acceptRequest,
    rejectRequest,
    removeFriend,
    createGroup,
    loadMembers,
    addGroupMember,
    memberName,
    sendText,
    sendEmoji,
    sendFileMessage,
    uploadAndSend,
    sync,
    bind,
  }
})
