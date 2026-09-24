<script setup lang="ts">
// 消息气泡：按内容 kind 分发渲染（文本/表情/图片/文件）。
// content 的归一（旧消息裸字符串 → text）收在这里，聊天窗只管布局。
import { computed } from 'vue'
import { downloadFile } from '@/api/http'
import { parseContent } from '@/types'
import type { ChatMessage } from '@/types'

const props = defineProps<{
  message: ChatMessage
}>()

/** 归一后的消息体（v-if 按 b.kind 分发，模板内类型可收窄）。 */
const b = computed(() => parseContent(props.message.content))

/** 人类可读的文件大小。 */
function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}

/** 鉴权下载（fetch + blob——a 标签直链带不了 Authorization）。 */
function download(fileId: string, filename: string): void {
  void downloadFile(`/api/files/${fileId}`, filename)
}
</script>

<template>
  <div class="bubble">
    <div v-if="b.kind === 'text'" class="text">{{ b.text }}</div>
    <div v-else-if="b.kind === 'emoji'" class="emoji">{{ b.emoji }}</div>
    <button v-else class="attachment" @click="download(b.file_id, b.filename)">
      <span class="file-icon">{{ b.kind === 'image' ? '🖼' : '📄' }}</span>
      <span class="file-meta">
        <span class="file-name">{{ b.filename }}</span>
        <span class="file-size">{{ fmtSize(b.size_bytes) }} · 点击下载</span>
      </span>
    </button>
    <div class="meta">
      <span v-if="message.status === 'sending'" class="status pending">发送中…</span>
      <span v-else-if="message.status === 'failed'" class="status failed" :title="message.failReason">
        发送失败
      </span>
      <span v-else class="status ok">已送达</span>
      <span class="time">{{ new Date(message.ts).toLocaleTimeString() }}</span>
    </div>
  </div>
</template>

<style scoped>
.bubble {
  max-width: 60%;
  background: #f0f3f7;
  border-radius: 10px;
  padding: 8px 12px;
}

.text {
  white-space: pre-wrap;
  word-break: break-word;
}

.emoji {
  font-size: 32px;
  line-height: 1.4;
}

.attachment {
  display: flex;
  align-items: center;
  gap: 10px;
  background: rgba(0, 0, 0, 0.04);
  border-radius: 8px;
  padding: 8px 12px;
  text-align: left;
}

.file-icon {
  font-size: 24px;
}

.file-meta {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.file-name {
  font-weight: 500;
  max-width: 220px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.file-size {
  font-size: 12px;
  color: var(--text-dim);
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

.status.failed {
  color: var(--danger);
}

.status.ok {
  color: #2ecc71;
}
</style>
