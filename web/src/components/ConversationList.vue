<script setup lang="ts">
// 会话列表：好友 + 群（点击切换 activeId；含 WS 状态指示）。
import { useWsStore } from '@/stores/ws'
import type { Conversation } from '@/types'

defineProps<{
  conversations: Conversation[]
  activeId: string | null
}>()

const ws = useWsStore()

const emit = defineEmits<{
  select: [id: string]
}>()

function kindLabel(kind: Conversation['kind']): string {
  return kind === 'friend' ? '好友' : '群'
}
</script>

<template>
  <nav class="conv-list">
    <header class="list-header">
      <span>会话</span>
      <span
        class="ws-dot"
        :class="ws.status"
        :title="`WebSocket：${ws.status}`"
        aria-label="连接状态"
      />
    </header>

    <ul class="list-body">
      <li
        v-for="conv in conversations"
        :key="conv.id"
        :class="{ active: conv.id === activeId }"
        @click="emit('select', conv.id)"
      >
        <div class="avatar" :class="conv.kind">
          {{ conv.name.charAt(0) }}
        </div>
        <div class="meta">
          <div class="name">{{ conv.name }}</div>
          <div class="kind">{{ kindLabel(conv.kind) }}</div>
        </div>
      </li>
    </ul>

    <p v-if="conversations.length === 0" class="empty">
      还没有会话：先用 REST 添加好友/建群（管理页阶段 6 上线）
    </p>
  </nav>
</template>

<style scoped>
.conv-list {
  background: var(--panel);
  border-right: 1px solid var(--border);
  display: flex;
  flex-direction: column;
  min-height: 0;
}

.list-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 14px 16px;
  font-weight: 600;
  border-bottom: 1px solid var(--border);
}

.ws-dot {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  background: var(--text-dim);
}

.ws-dot.open {
  background: #2ecc71;
}

.ws-dot.connecting {
  background: #f5a623;
}

.list-body {
  list-style: none;
  margin: 0;
  padding: 0;
  overflow-y: auto;
  flex: 1;
}

.list-body li {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 14px;
  cursor: pointer;
}

.list-body li:hover {
  background: #f0f3f7;
}

.list-body li.active {
  background: #e6edff;
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

.avatar.group {
  background: #e67e22;
}

.meta {
  min-width: 0;
}

.name {
  font-weight: 500;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.kind {
  font-size: 12px;
  color: var(--text-dim);
}

.empty {
  padding: 16px;
  color: var(--text-dim);
  font-size: 13px;
}
</style>
