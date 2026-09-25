// 会议（阶段 9）：LiveKit SFU 群会议的连接与轨道编排。
//
// 拓扑：N 人会议不再走阶段 8 的 P2P 全连接（N×(N-1)/2 条管道），
// 而是每人上行一路给 SFU、SFU 选择性转发——上行带宽与人数解耦。
// 我们的职责只剩两件事：领票（REST 换 JWT，资格由服务端 is_member
// 裁决）与把远端轨道编排成可渲染的 MediaStream。
//
// 状态机：idle ─领票+connect→ joining ─轨道发布完成→ connected
//           └────── 任何一步失败 / 离开 / 掉线 ──→ idle（全量收摊）

import { ref } from 'vue'
import { defineStore } from 'pinia'
import {
  Room,
  RoomEvent,
  createLocalTracks,
  type RemoteTrack,
  type RemoteTrackPublication,
  type RemoteParticipant,
} from 'livekit-client'
import { http } from '@/api/http'
import type { MeetingTicket } from '@/types'

export type MeetingStatus = 'idle' | 'joining' | 'connected'

/** 远端成员的渲染单元：名字 + 音视频合流的 MediaStream。 */
export interface RemoteMember {
  /** 昵称（JWT name 字段，签发时后端写入）。 */
  name: string
  /** 该成员的音视频轨道合流（<video> 元素音视频通吃）；无轨道时为 null。 */
  stream: MediaStream | null
}

export const useMeetingStore = defineStore('meeting', () => {
  const status = ref<MeetingStatus>('idle')
  /** 连接/信令失败的人话提示（无 LiveKit 服务时是主要出口）。 */
  const error = ref('')
  /** 当前房间名（群 ID 派生，UI 展示用）。 */
  const roomName = ref('')
  const micOn = ref(true)
  const camOn = ref(true)
  const screenOn = ref(false)
  /** 远端成员表：identity（用户 ID 字符串）→ 渲染单元。 */
  const remotes = ref<Record<string, RemoteMember>>({})
  /** 本地预览流（自己的摄像头/麦克风，muted 播放防回声）。 */
  const localStream = ref<MediaStream | null>(null)

  // Room 是命令式对象，不进 ref——Vue 的深度代理会干扰其内部状态
  // （与 call.ts 的 RTCPeerConnection 同一条框架边界纪律）。
  let room: Room | null = null

  /** 领票 + 连接 + 发布本地轨道。 */
  async function join(groupId: string): Promise<void> {
    if (status.value !== 'idle') return
    error.value = ''
    status.value = 'joining'
    try {
      // 领票失败（非成员 403 / 未登录 401）直接进 catch 展示
      const ticket = await http.post<MeetingTicket>(
        `/api/groups/${groupId}/meeting/token`,
      )
      roomName.value = ticket.room

      room = new Room({ adaptiveStream: true, dynacast: true })
      registerEvents(room)
      await room.connect(ticket.url, ticket.token)

      // 本地轨道：摄像头 + 麦克风进场即开（屏幕共享按需再开）
      const tracks = await createLocalTracks({ video: true, audio: true })
      localStream.value = new MediaStream(tracks.map((t) => t.mediaStreamTrack))
      for (const track of tracks) {
        await room.localParticipant.publishTrack(track)
      }
      status.value = 'connected'
    } catch (e) {
      error.value = e instanceof Error ? e.message : '连接会议失败'
      cleanup()
    }
  }

  /** 订阅远端轨道变化——事件回调是唯一写入 remotes 的地方。 */
  function registerEvents(r: Room): void {
    // 轨道到达：合进该成员的 MediaStream（新旧成员同一逻辑）
    r.on(
      RoomEvent.TrackSubscribed,
      (track: RemoteTrack, _pub: RemoteTrackPublication, p: RemoteParticipant) => {
        const member = remotes.value[p.identity] ?? { name: p.name, stream: new MediaStream() }
        member.name = p.name || p.identity
        member.stream?.addTrack(track.mediaStreamTrack)
        // 换新对象触发响应式（Record 整体替换比深层 patch 直白）
        remotes.value = { ...remotes.value, [p.identity]: member }
      },
    )
    // 轨道离开（对方关摄像头/掉线重连）：从合流里摘除，video 元素自然黑屏
    r.on(
      RoomEvent.TrackUnsubscribed,
      (track: RemoteTrack, _pub: RemoteTrackPublication, p: RemoteParticipant) => {
        const member = remotes.value[p.identity]
        if (member === undefined) return
        member.stream?.removeTrack(track.mediaStreamTrack)
        remotes.value = { ...remotes.value }
      },
    )
    // 成员离场：整卡移除
    r.on(RoomEvent.ParticipantDisconnected, (p: RemoteParticipant) => {
      const next = { ...remotes.value }
      delete next[p.identity]
      remotes.value = next
    })
    // 自己掉线/被踢：全量收摊（UI 由 MeetingView 的路由守卫收口）
    r.on(RoomEvent.Disconnected, () => {
      cleanup()
    })
  }

  /** 离开会议（主动）：断连 + 收摊。 */
  function leave(): void {
    room?.disconnect()
    cleanup()
  }

  /** 全量收摊：停本地轨道、清状态——任何出口路径都汇到这里。 */
  function cleanup(): void {
    localStream.value?.getTracks().forEach((t) => t.stop())
    localStream.value = null
    room = null
    remotes.value = {}
    micOn.value = true
    camOn.value = true
    screenOn.value = false
    status.value = 'idle'
  }

  /** 静音/取消静音（服务端无关——SFU 侧轨道照发，静音在发布端）。 */
  async function toggleMic(): Promise<void> {
    if (room === null) return
    micOn.value = !micOn.value
    await room.localParticipant.setMicrophoneEnabled(micOn.value)
  }

  /** 开/关摄像头。 */
  async function toggleCam(): Promise<void> {
    if (room === null) return
    camOn.value = !camOn.value
    await room.localParticipant.setCameraEnabled(camOn.value)
  }

  /**
   * 开/关屏幕共享：SFU 的红利——共享只是「多发布一条 track」，
   * 服务器转给所有人；P2P 时代这要跟每个对端单独协商。
   */
  async function toggleScreen(): Promise<void> {
    if (room === null) return
    screenOn.value = !screenOn.value
    await room.localParticipant.setScreenShareEnabled(screenOn.value)
  }

  return {
    status,
    error,
    roomName,
    micOn,
    camOn,
    screenOn,
    remotes,
    localStream,
    join,
    leave,
    toggleMic,
    toggleCam,
    toggleScreen,
  }
})
