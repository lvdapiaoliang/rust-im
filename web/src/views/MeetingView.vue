<script setup lang="ts">
// 会议页（阶段 9）：进入即领票连房，离开/掉线自动回群组页。
//
// 媒体挂载沿用 CallOverlay 的命令式桥接（srcObject 不能走模板绑定）；
// 远端格用「模板 ref 回调收集元素 + watch 重挂」两步——轨道订阅事件
// 随时到达，元素与流的先后顺序不确定，重挂逻辑要两边都兜住。
import { onMounted, onUnmounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useMeetingStore } from '@/stores/meeting'

const route = useRoute()
const router = useRouter()
const meeting = useMeetingStore()

const localEl = ref<HTMLVideoElement | null>(null)
/** 远端 video 元素表：identity → element（模板 ref 回调写入）。 */
const remoteEls = new Map<string, HTMLVideoElement>()

/** 模板 ref 回调：元素挂载/卸载时维护表 + 立即挂一次流。 */
function bindRemote(id: string, el: unknown): void {
  const video = el as HTMLVideoElement | null
  if (video === null) {
    remoteEls.delete(id)
    return
  }
  remoteEls.set(id, video)
  video.srcObject = meeting.remotes[id]?.stream ?? null
}

/** 轨道事件可能晚于元素挂载：remotes 一变就全表重挂。 */
watch(
  () => meeting.remotes,
  () => {
    for (const [id, el] of remoteEls) el.srcObject = meeting.remotes[id]?.stream ?? null
  },
)

/** 本地预览（muted 防回声）。 */
watch(
  () => [meeting.status, meeting.localStream],
  () => {
    if (localEl.value !== null) localEl.value.srcObject = meeting.localStream
  },
)

/** 退出回群组页（掉线收摊后 status 回 idle，watch 这里引导退出）。 */
watch(
  () => meeting.status,
  (s) => {
    if (s === 'idle' && meeting.error === '') void router.push({ name: 'groups' })
  },
)

onMounted(() => {
  void meeting.join(String(route.params.groupId))
})

// 组件卸载兜底离开（浏览器后退等路径不走「离开」按钮）
onUnmounted(() => {
  meeting.leave()
})
</script>

<template>
  <div class="meeting">
    <header class="meeting-head">
      <h2>群会议 · {{ meeting.roomName }}</h2>
      <span v-if="meeting.status === 'joining'" class="hint">正在连接…</span>
      <span v-else-if="meeting.status === 'connected'" class="hint ok">
        已连接（{{ Object.keys(meeting.remotes).length + 1 }} 人）
      </span>
    </header>

    <!-- 失败态：本机没有 LiveKit 服务时主要停在这里（诚实展示错误原文） -->
    <div v-if="meeting.status === 'idle'" class="fail">
      <p>{{ meeting.error || '会议已结束' }}</p>
      <button class="primary" @click="router.push({ name: 'groups' })">返回群组</button>
    </div>

    <!-- 视频格：本地一格 + 每个远端成员一格 -->
    <div v-else class="tiles">
      <div class="tile mine">
        <video ref="localEl" autoplay playsinline muted />
        <div class="name">我</div>
      </div>
      <div v-for="(member, id) in meeting.remotes" :key="id" class="tile">
        <video :ref="(el) => bindRemote(String(id), el)" autoplay playsinline />
        <div class="name">{{ member.name }}</div>
      </div>
    </div>

    <!-- 控制条 -->
    <footer class="controls">
      <button :class="{ off: !meeting.micOn }" @click="meeting.toggleMic()">
        {{ meeting.micOn ? '🎤 静音' : '🎤 已静音' }}
      </button>
      <button :class="{ off: !meeting.camOn }" @click="meeting.toggleCam()">
        {{ meeting.camOn ? '📷 关摄像头' : '📷 已关' }}
      </button>
      <button :class="{ on: meeting.screenOn }" @click="meeting.toggleScreen()">
        {{ meeting.screenOn ? '🖥 停止共享' : '🖥 共享屏幕' }}
      </button>
      <button class="leave" @click="meeting.leave()">离开会议</button>
    </footer>
  </div>
</template>

<style scoped>
.meeting {
  height: 100%;
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 16px;
  box-sizing: border-box;
  background: #14161a;
  color: #e8eaed;
}

.meeting-head {
  display: flex;
  align-items: baseline;
  gap: 12px;
}

.meeting-head h2 {
  font-size: 18px;
  margin: 0;
}

.hint {
  color: #9aa0a6;
  font-size: 13px;
}

.hint.ok {
  color: #81c995;
}

.fail {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 12px;
  color: #f28b82;
}

.tiles {
  flex: 1;
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
  gap: 10px;
  align-content: start;
  overflow-y: auto;
}

.tile {
  position: relative;
  aspect-ratio: 16 / 10;
  background: #000;
  border-radius: 10px;
  overflow: hidden;
}

.tile video {
  width: 100%;
  height: 100%;
  object-fit: cover;
  display: block;
}

.tile .name {
  position: absolute;
  left: 8px;
  bottom: 6px;
  padding: 2px 8px;
  border-radius: 6px;
  background: rgba(0, 0, 0, 0.55);
  font-size: 12px;
}

.controls {
  display: flex;
  justify-content: center;
  gap: 10px;
}

.controls button {
  padding: 9px 16px;
  border: none;
  border-radius: 8px;
  background: #2b2f36;
  color: #e8eaed;
  cursor: pointer;
  font-size: 14px;
}

.controls button:hover {
  background: #3a3f47;
}

.controls button.off {
  color: #fdd663;
}

.controls button.on {
  background: #8ab4f8;
  color: #14161a;
}

.controls button.leave {
  background: #c5221f;
}

.controls button.leave:hover {
  background: #d93025;
}
</style>
