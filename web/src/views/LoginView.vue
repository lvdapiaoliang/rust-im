<script setup lang="ts">
// 登录/注册页：单表单切换（骨架期不做路由区分）。
import { ref, computed } from 'vue'
import { useRouter } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { ApiError } from '@/api/http'

const auth = useAuthStore()
const router = useRouter()

const mode = ref<'login' | 'register'>('login')
const username = ref('')
const password = ref('')
const displayName = ref('')
const error = ref('')
const busy = ref(false)

const isLogin = computed(() => mode.value === 'login')

async function submit(): Promise<void> {
  error.value = ''
  busy.value = true
  try {
    if (isLogin.value) {
      await auth.login(username.value, password.value)
    } else {
      await auth.register(username.value, password.value, displayName.value)
      // 注册不自动登录（后端语义）：切到登录并预填
      mode.value = 'login'
      error.value = '注册成功，请登录'
      return
    }
    await router.push({ name: 'chat' })
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : '网络错误，请稍后重试'
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <div class="login-wrap">
    <form class="login-card" @submit.prevent="submit">
      <h1 class="title">rust-im</h1>
      <p class="subtitle">{{ isLogin ? '登录' : '注册新账号' }}</p>

      <label class="field">
        <span>用户名</span>
        <input v-model="username" autocomplete="username" placeholder="3~32 个字符" required />
      </label>

      <label v-if="!isLogin" class="field">
        <span>昵称</span>
        <input v-model="displayName" placeholder="展示给好友的名字" required />
      </label>

      <label class="field">
        <span>密码</span>
        <input
          v-model="password"
          type="password"
          autocomplete="current-password"
          placeholder="至少 6 个字符"
          required
        />
      </label>

      <p v-if="error" class="error" role="alert">{{ error }}</p>

      <button class="primary" type="submit" :disabled="busy">
        {{ busy ? '处理中…' : isLogin ? '登录' : '注册' }}
      </button>

      <button class="switch" type="button" @click="mode = isLogin ? 'register' : 'login'">
        {{ isLogin ? '没有账号？去注册' : '已有账号？去登录' }}
      </button>
    </form>
  </div>
</template>

<style scoped>
.login-wrap {
  height: 100%;
  display: flex;
  align-items: center;
  justify-content: center;
}

.login-card {
  width: 340px;
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 32px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}

.title {
  margin: 0;
  font-size: 22px;
  text-align: center;
}

.subtitle {
  margin: 0 0 8px;
  text-align: center;
  color: var(--text-dim);
}

.field {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.error {
  margin: 0;
  color: var(--danger);
  font-size: 13px;
}

.switch {
  background: none;
  color: var(--accent);
  padding: 4px;
}
</style>
