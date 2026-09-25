// 通话（阶段 8）：WebRTC P2P 音视频 + 远程桌面的会话状态机。
//
// 拓扑分工：媒体流走 P2P 直连（低延迟），协商走 WS 信令（signal 信封，
// 服务端只转发不解释——见 docs/14）。状态机：
//   idle ─呼叫→ calling ─对方接听→ connecting ─P2P 建立→ connected
//     │                └拒绝/离线→ idle
//     └来电→ incoming ─接听→ connecting ─挂断/断开→ idle
//
// 信令不重试不补投（服务器侧 peer_offline 即时反馈）——呼叫失败
// 靠用户重拨，这是实时协商与消息可靠性（ack/离线队列）的本质差异。

import { ref } from 'vue'
import { defineStore } from 'pinia'
import { useWsStore } from './ws'
import { useChatStore } from './chat'
import type { CallMedia, CallSignal, ErrorPayload, SignalEvent } from '@/types'

export type CallStatus = 'idle' | 'calling' | 'incoming' | 'connecting' | 'connected'

/**
 * ICE 服务器：只用公网 STUN（打洞后发现彼此的公网地址）。
 * 无 TURN 中继——P2P 打不通（对称型 NAT）就失败，教育取舍见 docs/14。
 */
const ICE_SERVERS: RTCIceServer[] = [{ urls: 'stun:stun.l.google.com:19302' }]

/** 请求摄像头/麦克风（通话形态）。 */
async function requestCamera(): Promise<MediaStream> {
  return navigator.mediaDevices.getUserMedia({ video: true, audio: true })
}

/** 请求屏幕捕获（远程桌面形态：共享方只发屏幕轨，不带麦克风）。 */
async function requestScreen(): Promise<MediaStream> {
  return navigator.mediaDevices.getDisplayMedia({ video: true, audio: false })
}

export const useCallStore = defineStore('call', () => {
  const ws = useWsStore()
  const chat = useChatStore()

  const status = ref<CallStatus>('idle')
  /** 对端用户 ID。 */
  const peerId = ref<string | null>(null)
  /** 通话形态：呼叫方决定，offer 里携带，接听方按它决定是否采集本地流。 */
  const media = ref<CallMedia>('audio-video')
  const localStream = ref<MediaStream | null>(null)
  const remoteStream = ref<MediaStream | null>(null)
  /** 呼叫/协商失败的人话提示（UI 顶部展示）。 */
  const failMsg = ref('')

  // RTCPeerConnection 不进 ref：它是命令式对象，Vue 的代理会干扰其内部状态
  let pc: RTCPeerConnection | null = null

  /** 对端显示名（会话列表/好友表反查，查不到降级 ID 尾号）。 */
  function peerName(): string {
    const id = peerId.value
    if (id === null) return ''
    const conv = chat.conversations.find((c) => c.id === id)
    if (conv !== undefined) return conv.name
    const f = chat.friends.find((u) => u.id === id)
    if (f !== undefined) return f.display_name
    return `用户 ${id.slice(-6)}`
  }

  /** 发信令到指定对端（from 由服务端裁决，客户端无法伪造）。 */
  function sendSignalTo(to: string, signal: CallSignal): void {
    ws.send('signal', { to, signal })
  }

  /** 发信令（统一出口：to 始终是当前对端）。 */
  function sendSignal(signal: CallSignal): void {
    if (peerId.value !== null) sendSignalTo(peerId.value, signal)
  }

  /** 建 P2P 连接对象 + 挂事件回调（每次通话新建，不复用旧连接）。 */
  function newPc(): RTCPeerConnection {
    const conn = new RTCPeerConnection({ iceServers: ICE_SERVERS })
    // 远端轨道到达：整体挂到 remoteStream（屏幕轨/摄像头轨都从这里出）
    conn.ontrack = (e) => {
      const [stream] = e.streams
      remoteStream.value = stream
    }
    // 本端候选生成：每发现一条网络路径就实时告诉对端（协商的"边探路边汇报"）
    conn.onicecandidate = (e) => {
      if (e.candidate !== null) sendSignal({ call: 'candidate', candidate: e.candidate.toJSON() })
    }
    // 连接状态机：failed/断开即收摊（P2P 的"死"由浏览器判定，不由我们猜）
    conn.onconnectionstatechange = () => {
      if (conn.connectionState === 'failed' || conn.connectionState === 'disconnected') {
        failMsg.value = '连接中断'
        cleanup()
      }
      if (conn.connectionState === 'connected') status.value = 'connected'
    }
    return conn
  }

  /** 把本地流挂进 P2P 连接（发送轨道）。 */
  function addLocalTracks(conn: RTCPeerConnection, stream: MediaStream): void {
    for (const track of stream.getTracks()) conn.addTrack(track, stream)
  }

  /** 收摊：关连接、停轨道、状态复位（挂断/被拒/失败共用）。 */
  function cleanup(): void {
    pc?.close()
    pc = null
    localStream.value?.getTracks().forEach((t) => t.stop())
    localStream.value = null
    remoteStream.value = null
    status.value = 'idle'
    peerId.value = null
  }

  /** 发起呼叫（呼叫方）：采集 → offer → 信令。 */
  async function startCall(target: string, kind: CallMedia): Promise<void> {
    if (status.value !== 'idle') return // 单通话并发上限：同时只有一路
    failMsg.value = ''
    peerId.value = target
    media.value = kind
    try {
      localStream.value = kind === 'screen' ? await requestScreen() : await requestCamera()
    } catch (e) {
      cleanup()
      failMsg.value = e instanceof Error ? `媒体采集失败：${e.message}` : '媒体采集失败'
      return
    }
    pc = newPc()
    addLocalTracks(pc, localStream.value)
    status.value = 'calling'
    const offer = await pc.createOffer()
    await pc.setLocalDescription(offer)
    // sdp 类型是 string|undefined（lib 定义），协商体内 sdp 空意味着 P2P 无法建立
    if (offer.sdp === undefined) {
      failMsg.value = 'SDP 生成失败'
      cleanup()
      return
    }
    sendSignal({ call: 'offer', sdp: offer.sdp, media: kind })
  }

  /** 接听（被呼方）：按 offer 形态决定本地采集，answer 回去。 */
  async function accept(): Promise<void> {
    if (status.value !== 'incoming' || pc === null) return
    failMsg.value = ''
    // 远程桌面形态：接听方只看不发（无需摄像头）；
    // 通话形态：接听方也要出自己的画面
    if (media.value === 'audio-video') {
      try {
        localStream.value = await requestCamera()
        addLocalTracks(pc, localStream.value)
      } catch (e) {
        failMsg.value = e instanceof Error ? `摄像头不可用：${e.message}` : '摄像头不可用'
        // 不终止通话：对方可能只想让我听/看
      }
    }
    const answer = await pc.createAnswer()
    await pc.setLocalDescription(answer)
    if (answer.sdp === undefined) {
      failMsg.value = 'SDP 生成失败'
      cleanup()
      return
    }
    sendSignal({ call: 'answer', sdp: answer.sdp })
    status.value = 'connecting'
  }

  /** 拒绝来电。 */
  function reject(): void {
    if (status.value !== 'incoming') return
    sendSignal({ call: 'reject' })
    cleanup()
  }

  /** 挂断（任意一方，任意时刻）。 */
  function hangup(): void {
    if (status.value === 'idle') return
    sendSignal({ call: 'hangup' })
    cleanup()
  }

  /** 信令到达分发（按 call 标签判别——与服务端不解释语义对应，前端是唯一解释者）。 */
  function onSignal(payload: SignalEvent): void {
    // 不在通话且不是来电 offer：过期信令（重连后的迟到 candidate 等）——丢弃
    const incoming = status.value === 'idle' && payload.signal.call === 'offer'
    if (status.value === 'idle' && !incoming) return
    if (status.value !== 'idle' && payload.from !== peerId.value) return // 第三方信令：当前只支持单通话，忽略

    switch (payload.signal.call) {
      case 'offer': {
        if (status.value !== 'idle') {
          // 正忙：礼貌拒绝（被呼方视角的「占线」）——不动当前通话的任何状态
          sendSignalTo(payload.from, { call: 'reject' })
          return
        }
        peerId.value = payload.from
        media.value = payload.signal.media
        pc = newPc()
        status.value = 'incoming'
        // offer 先存起来（接听动作在用户手上，candidate 却会先到）
        void pc.setRemoteDescription({ type: 'offer', sdp: payload.signal.sdp })
        break
      }
      case 'answer': {
        if (pc === null) return
        void pc.setRemoteDescription({ type: 'answer', sdp: payload.signal.sdp })
        status.value = 'connecting'
        break
      }
      case 'candidate': {
        // 候选先于 offer 到达的乱序由 addIceCandidate 内部缓冲容忍
        void pc?.addIceCandidate(payload.signal.candidate)
        break
      }
      case 'hangup': {
        failMsg.value = '对方已挂断'
        cleanup()
        break
      }
      case 'reject': {
        failMsg.value = '对方拒绝了通话'
        cleanup()
        break
      }
    }
  }

  /** 错误信封里与通话相关的部分（呼叫时对方离线/非好友）。 */
  function onCallError(payload: ErrorPayload): void {
    if (status.value !== 'calling') return
    if (payload.code === 'peer_offline') failMsg.value = '对方不在线'
    else if (payload.code === 'not_friend') failMsg.value = '仅好友之间可以通话'
    else return
    cleanup()
  }

  /** 订阅 WS 信封（连接生命周期方调用一次；返回取消函数）。 */
  function bind(): () => void {
    const offs = [
      ws.on('event', (payload) => {
        const p = payload as SignalEvent
        if (p.kind === 'signal') onSignal(p)
      }),
      ws.on('error', (payload) => onCallError(payload as ErrorPayload)),
    ]
    return () => offs.forEach((off) => off())
  }

  return {
    status,
    peerId,
    media,
    localStream,
    remoteStream,
    failMsg,
    peerName,
    startCall,
    accept,
    reject,
    hangup,
    bind,
  }
})
