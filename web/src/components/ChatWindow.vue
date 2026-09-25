<script setup lang="ts">
// 聊天窗：消息流（气泡统一交给 MessageBubble 分类型渲染）+ 输入区（文本/表情/文件）。
// 消息流滚动：新消息到达/切换会话时贴底（用户上翻时不打扰）。
import { computed, nextTick, ref, watch } from 'vue'
import { useAuthStore } from '@/stores/auth'
import { useChatStore } from '@/stores/chat'
import { useCallStore } from '@/stores/call'
import MessageBubble from '@/components/MessageBubble.vue'

const auth = useAuthStore()
const chat = useChatStore()
const call = useCallStore()

const draft = ref('')
const scrollBox = ref<HTMLElement | null>(null)
const fileInput = ref<HTMLInputElement | null>(null)
const emojiOpen = ref(false)
const uploadMsg = ref('')

/** 常用表情集（Unicode emoji；自定义表情 = 图片消息，走文件上传）。 */
const EMOJIS = [
  '😀', '😂', '🥰', '😎', '🤔', '😴', '😭', '😡',
  '👍', '👎', '🙏', '👏', '💪', '🤝', '✌️', '🫡',
  '❤️', '💔', '🎉', '🎂', '🌹', '⭐', '🔥', '✅',
]

const activeName = computed(() => {
  const conv = chat.conversations.find((c) => c.id === chat.activeId)
  return conv?.name ?? ''
})

/** 当前会话是否为群（群消息的 from 是不同成员，需要展示名字）。 */
const isGroup = computed(() => {
  const conv = chat.conversations.find((c) => c.id === chat.activeId)
  return conv?.kind === 'group'
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
  () => {
    // 进群会话顺手拉成员（缓存优先）：发送者名字的反查表
    if (isGroup.value && chat.activeId !== null) void chat.loadMembers(chat.activeId)
    void stickToBottom()
  },
)

function send(): void {
  if (chat.activeId === null) return
  const text = draft.value.trim()
  if (text === '') return
  chat.sendText(chat.activeId, text)
  draft.value = ''
  void stickToBottom()
}

function sendEmoji(emoji: string): void {
  if (chat.activeId === null) return
  chat.sendEmoji(chat.activeId, emoji)
  emojiOpen.value = false
  void stickToBottom()
}

/** 触发文件选择框（文件与图片同一入口，kind 由 MIME 推断）。 */
function pickFile(): void {
  uploadMsg.value = ''
  fileInput.value?.click()
}

/** 上传并发送（乐观消息在上传完成后插入——上传失败就不该有气泡）。 */
async function onFileChosen(event: Event): Promise<void> {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  input.value = '' // 允许连续选同一个文件
  if (file === undefined || chat.activeId === null) return
  try {
    await chat.uploadAndSend(chat.activeId, file)
    void stickToBottom()
  } catch (e) {
    uploadMsg.value = e instanceof Error ? `发送失败：${e.message}` : '发送失败'
  }
}

function isMine(from: string): boolean {
  return auth.user?.id === from
}

/** 群消息发送者显示名（自己的消息不用标——气泡位置已区分归属）。 */
function senderName(from: string): string {
  return chat.activeId === null ? '' : chat.memberName(chat.activeId, from)
}

/** 是否展示发送者名字：群会话且非本人。 */
function showSender(from: string): boolean {
  return isGroup.value && !isMine(from)
}

/** 发起通话（仅好友会话：1对1 的门槛与服务端信令校验一致）。 */
function callPeer(kind: 'audio-video' | 'screen'): void {
  if (chat.activeId === null || isGroup.value) return
  void call.startCall(chat.activeId, kind)
}
</script>

<template>
  <section class="chat-window">
    <header v-if="chat.activeId !== null" class="window-header">
      <span class="title">{{ activeName }}</span>
      <!-- 通话入口：仅好友单聊（群通话是阶段 9 的 SFU 形态，不走 P2P） -->
      <span v-if="!isGroup" class="call-tools">
        <button class="tool" title="视频通话" @click="callPeer('audio-video')">📞</button>
        <button class="tool" title="共享屏幕给对方" @click="callPeer('screen')">🖥</button>
      </span>
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
        <div class="bubble-stack">
          <div v-if="showSender(m.from)" class="sender-name">{{ senderName(m.from) }}</div>
          <MessageBubble :message="m" />
        </div>
      </div>
    </div>

    <footer v-if="chat.activeId !== null" class="composer">
      <p v-if="uploadMsg" class="upload-msg">{{ uploadMsg }}</p>
      <div class="composer-row">
        <!-- 表情面板 -->
        <div v-if="emojiOpen" class="emoji-panel">
          <button v-for="e in EMOJIS" :key="e" class="emoji-cell" @click="sendEmoji(e)">
            {{ e }}
          </button>
        </div>
        <button class="tool" title="表情" @click="emojiOpen = !emojiOpen">😊</button>
        <button class="tool" title="发送文件（不超过 20 MB）" @click="pickFile">📎</button>
        <input ref="fileInput" type="file" class="hidden-input" @change="onFileChosen" />
        <textarea
          v-model="draft"
          placeholder="输入消息，Enter 发送（Shift+Enter 换行）"
          rows="3"
          @keydown.enter.exact.prevent="send"
        />
        <button class="primary" :disabled="draft.trim() === ''" @click="send">发送</button>
      </div>
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
  display: flex;
  align-items: center;
  gap: 12px;
}

.window-header .title {
  flex: 1;
  min-width: 0;
}

.call-tools {
  display: flex;
  gap: 4px;
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

.bubble-stack {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  max-width: 100%;
}

.bubble-row.mine .bubble-stack {
  align-items: flex-end;
}

.sender-name {
  font-size: 12px;
  color: var(--text-dim);
  margin-bottom: 2px;
  padding-left: 4px;
}

/* 气泡配色在 MessageBubble 基础上按归属重染 */
.bubble-row.mine :deep(.bubble) {
  background: #dceaff;
}

.composer {
  padding: 10px 16px 12px;
  border-top: 1px solid var(--border);
}

.upload-msg {
  margin: 0 0 8px;
  font-size: 12px;
  color: var(--danger);
}

.composer-row {
  display: flex;
  gap: 8px;
  align-items: flex-end;
  position: relative;
}

.composer-row textarea {
  flex: 1;
  font: inherit;
  padding: 8px 12px;
  border: 1px solid var(--border);
  border-radius: 6px;
  outline: none;
  resize: none;
}

.composer-row textarea:focus {
  border-color: var(--accent);
}

.tool {
  background: none;
  font-size: 20px;
  padding: 6px 8px;
}

.hidden-input {
  display: none;
}

.emoji-panel {
  position: absolute;
  bottom: 48px;
  left: 0;
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 10px;
  padding: 8px;
  display: grid;
  grid-template-columns: repeat(8, 32px);
  gap: 2px;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.12);
}

.emoji-cell {
  background: none;
  padding: 2px;
  font-size: 20px;
  border-radius: 4px;
}

.emoji-cell:hover {
  background: #f0f3f7;
}
</style>
