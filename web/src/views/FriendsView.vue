<script setup lang="ts">
// 好友管理页：搜索加好友 / 待处理请求 / 好友列表（删除）。
//
// 数据全部走 chat store（好友事件实时刷列表——订阅在 App.vue 的 bind 里）；
// 本页只做加载与操作，不碰 WS。
import { onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useChatStore } from '@/stores/chat'
import { ApiError } from '@/api/http'
import type { User } from '@/types'

const router = useRouter()
const chat = useChatStore()

// ── 搜索加好友 ──
const searchName = ref('')
const found = ref<User | null>(null)
const searchMsg = ref('')
const searching = ref(false)

async function search(): Promise<void> {
  searchMsg.value = ''
  found.value = null
  const name = searchName.value.trim()
  if (name === '') return
  searching.value = true
  try {
    found.value = await chat.searchUser(name)
    if (found.value === null) searchMsg.value = '没有找到该用户'
  } catch (e) {
    searchMsg.value = e instanceof ApiError ? e.message : '查询失败'
  } finally {
    searching.value = false
  }
}

const requesting = ref(false)
const requestMsg = ref('')

async function sendRequest(): Promise<void> {
  if (found.value === null) return
  requesting.value = true
  requestMsg.value = ''
  try {
    await chat.sendFriendRequest(found.value.id)
    requestMsg.value = '请求已发送，等待对方处理'
    found.value = null
    searchName.value = ''
  } catch (e) {
    requestMsg.value = e instanceof ApiError ? e.message : '发送失败'
  } finally {
    requesting.value = false
  }
}

// ── 待处理请求 / 好友列表 ──
const loadError = ref('')
const busy = ref(false)

async function act(task: () => Promise<void>): Promise<void> {
  busy.value = true
  try {
    await task()
  } catch (e) {
    loadError.value = e instanceof ApiError ? e.message : '操作失败'
  } finally {
    busy.value = false
  }
}

function toChat(userId: string): void {
  chat.activeId = userId
  void router.push({ name: 'chat' })
}

onMounted(async () => {
  try {
    await Promise.all([chat.loadContacts(), chat.loadRequests()])
  } catch {
    loadError.value = '加载失败，请刷新重试'
  }
})
</script>

<template>
  <div class="friends-page">
    <header class="page-header">
      <button class="back" @click="router.push({ name: 'chat' })">← 返回聊天</button>
      <h2>好友</h2>
    </header>

    <p v-if="loadError" class="error">{{ loadError }}</p>

    <section class="card">
      <h3>添加好友</h3>
      <div class="search-row">
        <input
          v-model="searchName"
          placeholder="输入对方用户名（精确匹配）"
          @keydown.enter="search"
        />
        <button class="primary" :disabled="searching" @click="search">查找</button>
      </div>
      <p v-if="searchMsg" class="hint">{{ searchMsg }}</p>
      <div v-if="found" class="user-card">
        <div class="avatar">{{ found.display_name.charAt(0) }}</div>
        <div class="meta">
          <div class="name">{{ found.display_name }}</div>
          <div class="sub">{{ found.username }}</div>
        </div>
        <button class="primary" :disabled="requesting" @click="sendRequest">加好友</button>
      </div>
      <p v-if="requestMsg" class="hint">{{ requestMsg }}</p>
    </section>

    <section class="card">
      <h3>收到的请求（{{ chat.incomingRequests.length }}）</h3>
      <p v-if="chat.incomingRequests.length === 0" class="hint">暂无待处理请求</p>
      <div v-for="req in chat.incomingRequests" :key="req.id" class="user-card">
        <div class="avatar">{{ req.from_username.charAt(0) }}</div>
        <div class="meta">
          <div class="name">{{ req.from_username }}</div>
          <div class="sub">请求加你为好友</div>
        </div>
        <div class="actions">
          <button class="primary" :disabled="busy" @click="act(() => chat.acceptRequest(req.id))">
            接受
          </button>
          <button :disabled="busy" @click="act(() => chat.rejectRequest(req.id))">拒绝</button>
        </div>
      </div>
    </section>

    <section class="card">
      <h3>我的好友（{{ chat.friends.length }}）</h3>
      <p v-if="chat.friends.length === 0" class="hint">还没有好友，先去添加</p>
      <div v-for="friend in chat.friends" :key="friend.id" class="user-card">
        <div class="avatar">{{ friend.display_name.charAt(0) }}</div>
        <div class="meta">
          <div class="name">{{ friend.display_name }}</div>
          <div class="sub">{{ friend.username }}</div>
        </div>
        <div class="actions">
          <button @click="toChat(friend.id)">发消息</button>
          <button class="danger" :disabled="busy" @click="act(() => chat.removeFriend(friend.id))">
            删除
          </button>
        </div>
      </div>
    </section>
  </div>
</template>

<style scoped>
.friends-page {
  height: 100%;
  overflow-y: auto;
  background: var(--bg);
  padding: 0 16px 24px;
  max-width: 640px;
  margin: 0 auto;
}

.page-header {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 16px 0;
}

.page-header h2 {
  margin: 0;
}

.back {
  background: none;
  color: var(--accent);
  padding: 4px 8px;
}

.card {
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 10px;
  padding: 16px;
  margin-bottom: 16px;
}

.card h3 {
  margin: 0 0 12px;
  font-size: 14px;
}

.search-row {
  display: flex;
  gap: 8px;
}

.hint {
  color: var(--text-dim);
  font-size: 13px;
  margin: 10px 0 0;
}

.error {
  color: var(--danger);
}

.user-card {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 10px 0;
  border-bottom: 1px solid var(--border);
}

.user-card:last-child {
  border-bottom: none;
}

.avatar {
  width: 40px;
  height: 40px;
  border-radius: 8px;
  background: var(--accent);
  color: #fff;
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
}

.meta {
  flex: 1;
  min-width: 0;
}

.name {
  font-weight: 500;
}

.sub {
  font-size: 12px;
  color: var(--text-dim);
}

.actions {
  display: flex;
  gap: 8px;
}

.actions button {
  padding: 6px 12px;
  font-size: 13px;
}

button.danger {
  background: none;
  color: var(--danger);
}
</style>
