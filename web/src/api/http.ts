// REST 客户端：fetch 封装（token 注入、错误归一）。
// 分层纪律与后端对应：http 只管传输（URL/头/状态码），语义错误
// 用 ApiError（status + message）上抛，由调用方决定如何呈现。

const TOKEN_KEY = 'im_token'

/** 已登录令牌（localStorage 持久化——刷新页面不断会话）。 */
export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY)
}

export function setToken(token: string | null): void {
  if (token === null) {
    localStorage.removeItem(TOKEN_KEY)
  } else {
    localStorage.setItem(TOKEN_KEY, token)
  }
}

/** REST 错误：携带 HTTP 状态码与后端人话消息。 */
export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message)
  }
}

/** 发起 REST 请求并解析 JSON 响应；非 2xx 抛 [`ApiError`]。 */
export async function api<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = new Headers(init.headers)
  if (init.body !== undefined) headers.set('Content-Type', 'application/json')
  const token = getToken()
  if (token) headers.set('Authorization', `Bearer ${token}`)

  const resp = await fetch(path, { ...init, headers })
  if (!resp.ok) {
    const body = (await resp.json().catch(() => null)) as { error?: string } | null
    throw new ApiError(resp.status, body?.error ?? `请求失败（${resp.status}）`)
  }
  return (await resp.json()) as T
}

/** 便捷方法。 */
export const http = {
  get: <T>(path: string) => api<T>(path),
  post: <T>(path: string, body?: unknown) =>
    api<T>(path, { method: 'POST', body: body === undefined ? undefined : JSON.stringify(body) }),
  delete: <T>(path: string) => api<T>(path, { method: 'DELETE' }),
}
