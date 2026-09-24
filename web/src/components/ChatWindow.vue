<script setup lang="ts">
// 聊天窗：消息流（气泡）+ 输入框。
// 消息流滚动：新消息到达/切换会话时贴底（用户上翻时不打扰）。
import { computed, nextTick, ref, watch } from 'vue'
import { useAuthStore } from '@/stores/auth'
import { useChatStore } from '@/stores/chat'

const auth = useAuthStore()
const chat = useChatStore()

const draft = ref('')
const scrollBox = ref<HTMLElement | null>(null)

const activeName = computed(() => {
  const conv = chat.conversations.find((c) => c.id === chat.activeId)
  return conv?.name ?? ''
})

const messages = computed(() => chat.activeMessages)

/** 贴底滚动（保留用户上翻的自由）。 */
async function stickToBottom(): Promise<void> {
  await nextTick()
  const el = scrollBox.value
  if (el === null) return
  const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 80
  if (nearBottom) el.scrollTop = el.scrollHeight
}

watch(
  () => messages.value.length,
  () => void stickToBottom(),
)

watch(
  () => chat.activeId,
  () => void stickToBottom(),
)

function send(): void {
  if (chat.activeId === null) return
  const text = draft.value.trim()
  if (text === '') return
  chat.sendText(chat.activeId, text)
  draft.value = ''
  void stickToBottom()
}

function isMine(from: string): boolean {
  return auth.user?.id === from
}
</script>

<template>
  <section class="chat-window">
    <header v-if="chat.activeId !== null" class="window-header">
      {{ activeName }}
    </header>

    <div v-if="chat.activeId === null" class="placeholder">
      选择左侧会话开始聊天
    </div>

    <div v-else ref="scrollBox" class="message-flow">
      <div
        v-for="m in messages"
        :key="m.client_msg_id"
        class="bubble-row"
        :class="{ mine: isMine(m.from) }"
      >
        <div class="bubble">
          <div class="text">{{ m.content }}</div>
          <div class="meta">
            <span v-if="m.status === 'sending'" class="status pending">发送中…</span>
            <span v-else class="status ok">已送达</span>
            <span class="time">{{ new Date(m.ts).toLocaleTimeString() }}</span>
          </div>
        </div>
      </div>
    </div>

    <footer v-if="chat.activeId !== null" class="composer">
      <textarea
        v-model="draft"
        placeholder="输入消息，Enter 发送（Shift+Enter 换行）"
        rows="3"
        @keydown.enter.exact.prevent="send"
      />
      <button class="primary" :disabled="draft.trim() === ''" @click="send">发送</button>
    </footer>
  </section>
</template>

<style scoped>
.chat-window {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-width: 0;
  background: var(--panel);
}

.window-header {
  padding: 14px 20px;
  font-weight: 600;
  border-bottom: 1px solid var(--border);
}

.placeholder {
  margin: auto;
  color: var(--text-dim);
}

.message-flow {
  flex: 1;
  overflow-y: auto;
  padding: 16px 20px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.bubble-row {
  display: flex;
}

.bubble-row.mine {
  justify-content: flex-end;
}

.bubble {
  max-width: 60%;
  background: #f0f3f7;
  border-radius: 10px;
  padding: 8px 12px;
}

.bubble-row.mine .bubble {
  background: #dceaff;
}

.text {
  white-space: pre-wrap;
  word-break: break-word;
}

.meta {
  display: flex;
  gap: 8px;
  justify-content: flex-end;
  font-size: 11px;
  color: var(--text-dim);
  margin-top: 4px;
}

.status.pending {
  color: #f5a623;
}

.status.ok {
  color: #2ecc71;
}

.composer {
  display: flex;
  gap: 10px;
  padding: 12px 16px;
  border-top: 1px solid var(--border);
  align-items: flex-end;
}

.composer textarea {
  flex: 1;
  font: inherit;
  padding: 8px 12px;
  border: 1px solid var(--border);
  border-radius: 6px;
  outline: none;
  resize: none;
}

.composer textarea:focus {
  border-color: var(--accent);
}
</style>
