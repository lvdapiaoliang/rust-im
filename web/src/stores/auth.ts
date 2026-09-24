// 登录态：token 持久化 + 用户信息 + 登录/注册/恢复会话。
import { ref, computed } from 'vue'
import { defineStore } from 'pinia'
import { http, setToken, getToken } from '@/api/http'
import type { User } from '@/types'

interface LoginResp {
  token: string
  expires_in_secs: number
  user: User
}

export const useAuthStore = defineStore('auth', () => {
  const token = ref<string | null>(getToken())
  const user = ref<User | null>(null)
  const isLoggedIn = computed(() => token.value !== null)

  /** 登录：成功后持久化 token 并记录用户。 */
  async function login(username: string, password: string): Promise<void> {
    const resp = await http.post<LoginResp>('/api/login', { username, password })
    token.value = resp.token
    user.value = resp.user
    setToken(resp.token)
  }

  /** 注册：只建账号不自动登录（后端语义），成功后由调用方跳登录。 */
  async function register(username: string, password: string, displayName: string): Promise<void> {
    await http.post<User>('/api/register', {
      username,
      password,
      display_name: displayName,
    })
  }

  /** 刷新页面后按 token 恢复会话（无效则清理本地态）。 */
  async function restore(): Promise<boolean> {
    if (token.value === null) return false
    try {
      user.value = await http.get<User>('/api/me')
      return true
    } catch {
      logout()
      return false
    }
  }

  /** 登出：清本地态（服务端 token 留给过期回收；踢下线阶段 6 做）。 */
  function logout(): void {
    token.value = null
    user.value = null
    setToken(null)
  }

  return { token, user, isLoggedIn, login, register, restore, logout }
})
