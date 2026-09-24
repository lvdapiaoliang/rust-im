// WS 连接：信封收发 + 自动重连（指数退避）+ 应用层心跳。
//
// 设计要点：
// - 「连接」与「业务」解耦：本 store 只维护一条连接与订阅表，
//   业务（chat）通过 on(type, cb) 订阅信封——连接重建后
//   业务自己决定要不要 sync 补投；
// - 心跳是应用层 ping/pong（浏览器无法发自定义 WS 控制帧），
//   30s 一次；连续 pong 丢失视为半开连接，主动断开触发重连；
// - 重连退避 1s 起步、上限 15s，避免服务端故障时的连接风暴。

import { ref } from 'vue'
import { defineStore } from 'pinia'
import type { Envelope } from '@/types'

export type WsStatus = 'connecting' | 'open' | 'closed'

/** 心跳间隔： pong 在 2 个间隔内未到即判半开。 */
const HEARTBEAT_INTERVAL_MS = 30_000
/** 重连退避：起步与上限。 */
const RECONNECT_BASE_MS = 1_000
const RECONNECT_MAX_MS = 15_000

type Handler = (payload: unknown, envelope: Envelope) => void

export const useWsStore = defineStore('ws', () => {
  const status = ref<WsStatus>('closed')

  let socket: WebSocket | null = null
  let seq = 0
  let attempts = 0 // 连续失败次数（成功后清零）
  let heartbeatTimer: ReturnType<typeof setInterval> | null = null
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null
  let missedPongs = 0
  let manualClose = false

  /** 信封订阅表：type → 回调集合。 */
  const handlers = new Map<string, Set<Handler>>()

  /** 订阅某类信封（返回取消函数）。 */
  function on(type: string, handler: Handler): () => void {
    let set = handlers.get(type)
    if (!set) {
      set = new Set()
      handlers.set(type, set)
    }
    set.add(handler)
    return () => set.delete(handler)
  }

  function dispatch(envelope: Envelope): void {
    const set = handlers.get(envelope.type)
    if (set) for (const handler of set) handler(envelope.payload, envelope)
  }

  /** 发送信封（seq 自动递增；未连接时静默丢弃——重连后靠 ack/sync 兜底）。 */
  function send(type: string, payload: unknown): void {
    if (socket === null || socket.readyState !== WebSocket.OPEN) return
    seq += 1
    socket.send(JSON.stringify({ type, seq, ack: 0, payload }))
  }

  function stopTimers(): void {
    if (heartbeatTimer !== null) clearInterval(heartbeatTimer)
    if (reconnectTimer !== null) clearTimeout(reconnectTimer)
    heartbeatTimer = null
    reconnectTimer = null
  }

  function scheduleReconnect(): void {
    if (manualClose) return
    status.value = 'closed'
    // 指数退避：1s、2s、4s、8s，封顶 15s
    const delay = Math.min(RECONNECT_BASE_MS * 2 ** attempts, RECONNECT_MAX_MS)
    attempts += 1
    reconnectTimer = setTimeout(connect, delay)
  }

  function startHeartbeat(): void {
    missedPongs = 0
    heartbeatTimer = setInterval(() => {
      // 两个周期没收到任何 pong：判半开连接，主动断开走重连
      missedPongs += 1
      if (missedPongs > 2) {
        socket?.close()
        return
      }
      send('ping', {})
    }, HEARTBEAT_INTERVAL_MS)
  }

  /** 建立连接（幂等：已有活连接则跳过）。 */
  function connect(): void {
    if (socket !== null && socket.readyState <= WebSocket.OPEN) return
    const auth = localStorage.getItem('im_token')
    if (auth === null) return // 未登录：不连

    manualClose = false
    status.value = 'connecting'
    // 同源路径（开发期走 vite 代理，生产同源部署），token 走 query
    const ws = new WebSocket(`/ws?token=${encodeURIComponent(auth)}`)
    socket = ws

    ws.onopen = () => {
      attempts = 0
      status.value = 'open'
      startHeartbeat()
    }

    ws.onmessage = (event) => {
      let envelope: Envelope
      try {
        envelope = JSON.parse(event.data as string) as Envelope
      } catch {
        return // 坏信封：忽略（服务端不会发；防御未来升级错配）
      }
      if (envelope.type === 'pong' || envelope.type === 'msg') missedPongs = 0
      dispatch(envelope)
    }

    ws.onclose = () => {
      stopTimers()
      socket = null
      scheduleReconnect()
    }

    ws.onerror = () => {
      // onclose 随后会触发，重连调度集中在那里
    }
  }

  /** 主动断开（登出用）：不触发重连。 */
  function disconnect(): void {
    manualClose = true
    stopTimers()
    socket?.close()
    socket = null
    status.value = 'closed'
  }

  return { status, connect, disconnect, send, on }
})
