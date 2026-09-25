<script setup lang="ts">
// 聊天页：三栏布局（自头像 | 会话列表 | 聊天窗）。
// WS 连接与信封订阅在 App.vue（与好友页共享），这里只拉联系人。
import { onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { useChatStore } from '@/stores/chat'
import ConversationList from '@/components/ConversationList.vue'
import ChatWindow from '@/components/ChatWindow.vue'

const auth = useAuthStore()
const chat = useChatStore()
const router = useRouter()

const loadError = ref('')

onMounted(async () => {
  try {
    await chat.loadContacts()
  } catch {
    loadError.value = '联系人加载失败，请刷新重试'
  }
})

function logout(): void {
  // 断连由 App.vue 的 isLoggedIn 监听统一处理（登出 → 断开）
  auth.logout()
  void router.push({ name: 'login' })
}
</script>

<template>
  <div class="chat-layout">
    <aside class="me-bar">
      <div class="avatar">{{ auth.user?.display_name?.charAt(0) ?? '?' }}</div>
      <div class="me-name">{{ auth.user?.display_name }}</div>
      <button class="nav" title="好友管理" @click="router.push({ name: 'friends' })">好友</button>
      <button class="nav" title="群组管理" @click="router.push({ name: 'groups' })">群组</button>
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

.nav {
  background: none;
  color: #aab;
  padding: 4px;
}

.nav:hover {
  color: #fff;
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
