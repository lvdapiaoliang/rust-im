// 会话与消息：好友/群列表 + 消息收发 + 离线同步。
//
// 消息状态机（与后端协议对应）：
//   本地乐观插入（sending）→ ws 发出 → msg_ack 核销（sent，有 msg_id）
// 重连后 sync（since = 已见最大 msg_id）补投离线消息。

import { ref, computed } from 'vue'
import { defineStore } from 'pinia'
import { http } from '@/api/http'
import { useAuthStore } from './auth'
import { useWsStore } from './ws'
import type {
  ChatMessage,
  Conversation,
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

  /** 发送文本消息：本地乐观插入 + WS 发出（ack 回来核销状态）。 */
  function sendText(to: string, text: string): void {
    const me = auth.user
    if (me === null || text.trim() === '') return
    clientSeq += 1
    const message: ChatMessage = {
      client_msg_id: clientSeq,
      from: me.id,
      to,
      content: text,
      status: 'sending',
      ts: Date.now(),
    }
    append(to, message)
    ws.send('msg', {
      to,
      client_msg_id: message.client_msg_id,
      content: text,
    })
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

  /** 离线同步：按游标拉一批（空批 = 没有更多）。 */
  function sync(): void {
    ws.send('sync', { since: lastMsgId })
  }

  /** 订阅 WS 信封（组件挂载时调用一次；返回取消函数）。 */
  function bind(): () => void {
    const offs = [
      ws.on('msg', (payload) => ingestIncoming(payload as MsgPayload)),
      ws.on('msg_ack', (payload) => handleAck(payload as MsgAckPayload)),
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
    conversations,
    messages,
    activeId,
    activeMessages,
    loadContacts,
    sendText,
    sync,
    bind,
  }
})
