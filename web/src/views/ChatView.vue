<script setup lang="ts">
// 聊天页：三栏布局（自头像 | 会话列表 | 聊天窗）。
// 生命周期职责：挂载时拉联系人、连 WS、订阅信封；卸载时全部撤销。
import { onMounted, onUnmounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { useWsStore } from '@/stores/ws'
import { useChatStore } from '@/stores/chat'
import ConversationList from '@/components/ConversationList.vue'
import ChatWindow from '@/components/ChatWindow.vue'

const auth = useAuthStore()
const ws = useWsStore()
const chat = useChatStore()
const router = useRouter()

const loadError = ref('')

let unbind: (() => void) | null = null

onMounted(async () => {
  unbind = chat.bind()
  try {
    await chat.loadContacts()
  } catch {
    loadError.value = '联系人加载失败，请刷新重试'
  }
  ws.connect()
})

onUnmounted(() => {
  unbind?.()
  ws.disconnect()
})

function logout(): void {
  ws.disconnect()
  auth.logout()
  void router.push({ name: 'login' })
}
</script>

<template>
  <div class="chat-layout">
    <aside class="me-bar">
      <div class="avatar">{{ auth.user?.display_name?.charAt(0) ?? '?' }}</div>
      <div class="me-name">{{ auth.user?.display_name }}</div>
      <button class="logout" title="登出" @click="logout">退出</button>
    </aside>

    <ConversationList
      :conversations="chat.conversations"
      :active-id="chat.activeId"
      @select="chat.activeId = $event"
    />

    <main class="chat-main">
      <p v-if="loadError" class="load-error">{{ loadError }}</p>
      <ChatWindow v-else :key="chat.activeId ?? 'none'" />
    </main>
  </div>
</template>

<style scoped>
.chat-layout {
  height: 100%;
  display: grid;
  grid-template-columns: 200px 260px 1fr;
}

.me-bar {
  background: #2b2f36;
  color: #fff;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 10px;
  padding: 20px 12px;
}

.avatar {
  width: 48px;
  height: 48px;
  border-radius: 50%;
  background: var(--accent);
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 20px;
}

.me-name {
  font-size: 13px;
  word-break: break-all;
  text-align: center;
}

.logout {
  margin-top: auto;
  background: none;
  color: #aab;
  padding: 4px;
}

.logout:hover {
  color: #fff;
}

.chat-main {
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.load-error {
  margin: auto;
  color: var(--danger);
}
</style>
