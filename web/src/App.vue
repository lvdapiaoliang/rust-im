<script setup lang="ts">
// 根组件：路由出口 + WS 连接生命周期（登录态驱动）。
//
// 连接放在这里而不是聊天页：好友页等兄弟路由共享同一条连接，
// 切页不断线（业务订阅由 chat.bind 统一注册，连接重建后 welcome→sync 兜底）。
import { onMounted, watch } from 'vue'
import { useAuthStore } from '@/stores/auth'
import { useWsStore } from '@/stores/ws'
import { useChatStore } from '@/stores/chat'

const auth = useAuthStore()
const ws = useWsStore()
const chat = useChatStore()

let unbind: (() => void) | null = null

function connect(): void {
  unbind?.()
  unbind = chat.bind()
  ws.connect()
}

function disconnect(): void {
  ws.disconnect()
  unbind?.()
  unbind = null
}

onMounted(() => {
  if (auth.isLoggedIn) connect()
  // 登录/登出都汇聚到这里（main.ts 的 restore 在挂载前完成）
  watch(
    () => auth.isLoggedIn,
    (loggedIn) => (loggedIn ? connect() : disconnect()),
  )
})
</script>

<template>
  <router-view />
</template>
