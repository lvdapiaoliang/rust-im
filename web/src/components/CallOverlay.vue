<script setup lang="ts">
// 通话覆盖层（阶段 8）：来电/呼叫中/通话中三态 UI。
//
// 挂在 App.vue（全局）：来电可能发生在任何路由下，覆盖层不属于
// 任何页面。媒体渲染用命令式 srcObject 挂载（<video :src> 不支持
// MediaStream），watch 在轨道就绪/状态变化时挂流。
import { ref, watch } from 'vue'
import { useCallStore } from '@/stores/call'

const call = useCallStore()

const remoteEl = ref<HTMLVideoElement | null>(null)
const localEl = ref<HTMLVideoElement | null>(null)

/** 把流挂到 video 元素（srcObject 不在模板绑定能力内——命令式桥接）。 */
function attach(el: HTMLVideoElement | null, stream: MediaStream | null): void {
  if (el !== null) el.srcObject = stream
}

watch(
  () => [call.status, call.remoteStream],
  () => attach(remoteEl.value, call.remoteStream),
)
watch(
  () => [call.status, call.localStream],
  () => attach(localEl.value, call.localStream),
)
</script>

<template>
  <!-- 两种可见态：通话中（各阶段 UI）或收摊后的失败提示（用户需要知道为什么挂了） -->
  <div v-if="call.status !== 'idle' || call.failMsg !== ''" class="call-overlay">
    <div class="call-panel">
      <!-- 纯提示态（通话已结束但用户需要看到原因） -->
      <div v-if="call.status === 'idle'" class="notice">
        <p class="fail-msg">{{ call.failMsg }}</p>
        <button class="decline" @click="call.failMsg = ''">关闭</button>
      </div>

      <template v-else>
      <!-- 失败提示（通话中发生） -->
      <p v-if="call.failMsg" class="fail-msg">{{ call.failMsg }}</p>

      <!-- 来电 -->
      <div v-if="call.status === 'incoming'" class="incoming">
        <div class="avatar">{{ call.peerName().charAt(0) }}</div>
        <div class="name">{{ call.peerName() }}</div>
        <div class="sub">
          {{ call.media === 'screen' ? '邀请共享屏幕' : '邀请视频通话' }}
        </div>
        <div class="actions">
          <button class="accept" @click="call.accept()">接听</button>
          <button class="decline" @click="call.reject()">拒绝</button>
        </div>
      </div>

      <!-- 呼叫中 / 接通中 / 通话中 -->
      <template v-else>
        <div class="video-stage">
          <video ref="remoteEl" autoplay playsinline class="remote" />
          <video
            v-if="call.localStream !== null"
            ref="localEl"
            autoplay
            playsinline
            muted
            class="local"
          />
        </div>
        <div class="status-line">
          {{ call.peerName() }} ·
          {{
            call.status === 'calling'
              ? '呼叫中…'
              : call.status === 'connecting'
                ? '建立 P2P 连接…'
                : call.media === 'screen'
                  ? '远程桌面'
                  : '通话中'
          }}
        </div>
        <button class="decline big" @click="call.hangup()">挂断</button>
      </template>
      </template>
    </div>
  </div>
</template>

<style scoped>
.call-overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.6);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 100;
}

.call-panel {
  width: min(640px, 92vw);
  background: var(--panel, #1c1f26);
  border-radius: 14px;
  padding: 20px;
  display: flex;
  flex-direction: column;
  gap: 14px;
  align-items: center;
}

.fail-msg {
  color: #ff8f8f;
  font-size: 13px;
  margin: 0;
}

.notice {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 12px;
  padding: 12px 0;
}

.notice .fail-msg {
  font-size: 15px;
}

.incoming {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 10px;
  padding: 16px 0;
}

.avatar {
  width: 64px;
  height: 64px;
  border-radius: 50%;
  background: #4c8bf5;
  color: #fff;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 28px;
}

.name {
  font-weight: 600;
  color: #fff;
}

.sub {
  font-size: 13px;
  color: #aab;
}

.actions {
  display: flex;
  gap: 16px;
  margin-top: 12px;
}

.accept {
  background: #2ecc71;
  color: #fff;
  border: none;
  border-radius: 8px;
  padding: 10px 28px;
  font-size: 15px;
  cursor: pointer;
}

.decline {
  background: #e74c3c;
  color: #fff;
  border: none;
  border-radius: 8px;
  padding: 10px 28px;
  font-size: 15px;
  cursor: pointer;
}

.decline.big {
  padding: 10px 48px;
}

.video-stage {
  position: relative;
  width: 100%;
  aspect-ratio: 16 / 9;
  background: #000;
  border-radius: 10px;
  overflow: hidden;
}

.remote {
  width: 100%;
  height: 100%;
  object-fit: contain;
}

.local {
  position: absolute;
  right: 10px;
  bottom: 10px;
  width: 130px;
  border-radius: 6px;
  border: 1px solid rgba(255, 255, 255, 0.3);
}

.status-line {
  color: #aab;
  font-size: 14px;
}
</style>
