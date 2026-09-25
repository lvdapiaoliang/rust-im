<script setup lang="ts">
// 群组管理页：建群 / 我的群列表 / 群详情（成员列表 + 群主拉人）。
//
// 群会话收发不经过本页——chat store 的 conversations/ingestIncoming/
// sendBody 原生兼容群 ID；这里只补管理动作与成员缓存（名字反查用）。
import { computed, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useChatStore } from '@/stores/chat'
import { ApiError } from '@/api/http'
import type { User } from '@/types'

const router = useRouter()
const chat = useChatStore()

// ── 建群 ──
const groupName = ref('')
const createMsg = ref('')
const creating = ref(false)

async function create(): Promise<void> {
  const name = groupName.value.trim()
  if (name === '') return
  creating.value = true
  createMsg.value = ''
  try {
    await chat.createGroup(name)
    createMsg.value = `群「${name}」已创建，点下方成员即可进入`
    groupName.value = ''
  } catch (e) {
    createMsg.value = e instanceof ApiError ? e.message : '建群失败'
  } finally {
    creating.value = false
  }
}

// ── 我的群列表 + 群详情 ──
const loadError = ref('')
const selectedId = ref<string | null>(null)
const busy = ref(false)

const selected = computed(() => chat.groups.find((g) => g.id === selectedId.value) ?? null)
const selectedMembers = computed(() =>
  selectedId.value === null ? [] : (chat.members[selectedId.value] ?? []),
)

/** 展开群详情：拉成员（缓存优先）。 */
async function open(groupId: string): Promise<void> {
  selectedId.value = groupId
  loadError.value = ''
  try {
    await chat.loadMembers(groupId)
  } catch (e) {
    loadError.value = e instanceof ApiError ? e.message : '加载成员失败'
  }
}

// ── 群主拉人（复用按用户名精确查找——与加好友同一入口约定） ──
const searchName = ref('')
const found = ref<User | null>(null)
const searchMsg = ref('')
const searching = ref(false)
const addMsg = ref('')

async function search(): Promise<void> {
  searchMsg.value = ''
  addMsg.value = ''
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

async function addToGroup(): Promise<void> {
  if (found.value === null || selected.value === null) return
  busy.value = true
  addMsg.value = ''
  try {
    await chat.addGroupMember(selected.value.id, found.value.id)
    addMsg.value = `已把 ${found.value.display_name} 拉入群`
    found.value = null
    searchName.value = ''
  } catch (e) {
    addMsg.value = e instanceof ApiError ? e.message : '拉人失败'
  } finally {
    busy.value = false
  }
}

function toChat(groupId: string): void {
  chat.activeId = groupId
  void router.push({ name: 'chat' })
}

onMounted(async () => {
  try {
    await chat.loadContacts()
  } catch {
    loadError.value = '加载失败，请刷新重试'
  }
})
</script>

<template>
  <div class="groups-page">
    <header class="page-header">
      <button class="back" @click="router.push({ name: 'chat' })">← 返回聊天</button>
      <h2>群组</h2>
    </header>

    <p v-if="loadError" class="error">{{ loadError }}</p>

    <section class="card">
      <h3>创建群</h3>
      <div class="search-row">
        <input
          v-model="groupName"
          placeholder="输入群名"
          @keydown.enter="create"
        />
        <button class="primary" :disabled="creating || groupName.trim() === ''" @click="create">
          建群
        </button>
      </div>
      <p v-if="createMsg" class="hint">{{ createMsg }}</p>
    </section>

    <section class="card">
      <h3>我的群（{{ chat.groups.length }}）</h3>
      <p v-if="chat.groups.length === 0" class="hint">还没有群，先建一个</p>
      <div v-for="g in chat.groups" :key="g.id" class="user-card">
        <div class="avatar group-avatar">#</div>
        <div class="meta">
          <div class="name">
            {{ g.name }}
            <span class="role-tag" :class="{ owner: g.role === 'owner' }">
              {{ g.role === 'owner' ? '群主' : '成员' }}
            </span>
          </div>
          <div class="sub">{{ selectedId === g.id ? '下方查看成员' : '点击查看成员' }}</div>
        </div>
        <div class="actions">
          <button class="primary" @click="toChat(g.id)">发消息</button>
          <button @click="open(g.id)">成员</button>
        </div>
      </div>
    </section>

    <section v-if="selected !== null" class="card">
      <h3>「{{ selected.name }}」的成员（{{ selectedMembers.length }}）</h3>
      <div v-for="m in selectedMembers" :key="m.id" class="user-card">
        <div class="avatar">{{ m.display_name.charAt(0) }}</div>
        <div class="meta">
          <div class="name">
            {{ m.display_name }}
            <span v-if="m.role === 'owner'" class="role-tag owner">群主</span>
          </div>
          <div class="sub">{{ m.username }}</div>
        </div>
      </div>

      <!-- 拉人仅群主可见（后端同样校验 NotOwner——前端隐藏只是体验，不是安全边界） -->
      <div v-if="selected.role === 'owner'" class="invite">
        <h4>拉人入群</h4>
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
          <button class="primary" :disabled="busy" @click="addToGroup">拉入</button>
        </div>
        <p v-if="addMsg" class="hint">{{ addMsg }}</p>
      </div>
    </section>
  </div>
</template>

<style scoped>
.groups-page {
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

/* 群头像没有真人昵称可取首字，用 # 占位与好友头像区分 */
.group-avatar {
  font-weight: 700;
}

.meta {
  flex: 1;
  min-width: 0;
}

.name {
  font-weight: 500;
  display: flex;
  align-items: center;
  gap: 6px;
}

.sub {
  font-size: 12px;
  color: var(--text-dim);
}

.role-tag {
  font-size: 11px;
  padding: 1px 6px;
  border-radius: 4px;
  background: #eef0f4;
  color: var(--text-dim);
  font-weight: 400;
}

.role-tag.owner {
  background: #fdeeb8;
  color: #8a6d00;
}

.actions {
  display: flex;
  gap: 8px;
}

.actions button {
  padding: 6px 12px;
  font-size: 13px;
}

.invite {
  margin-top: 16px;
  padding-top: 12px;
  border-top: 1px dashed var(--border);
}

.invite h4 {
  margin: 0 0 10px;
  font-size: 13px;
}
</style>
