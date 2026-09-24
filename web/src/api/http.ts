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

/** multipart 上传（字段名 file）。 */
export async function upload<T>(path: string, file: File): Promise<T> {
  // 不手动设 Content-Type：浏览器要自动附 boundary，手动设会坏掉整个 multipart 体
  const form = new FormData()
  form.append('file', file)
  const headers = new Headers()
  const token = getToken()
  if (token) headers.set('Authorization', `Bearer ${token}`)

  const resp = await fetch(path, { method: 'POST', headers, body: form })
  if (!resp.ok) {
    const body = (await resp.json().catch(() => null)) as { error?: string } | null
    throw new ApiError(resp.status, body?.error ?? `上传失败（${resp.status}）`)
  }
  return (await resp.json()) as T
}

/**
 * 鉴权下载：拿 blob 再触发保存。
 * （`Authorization` 头只能走 fetch——`<a href>` 直链带不了令牌，
 * 所以下载必须绕一道 blob；小文件场景完全够用。）
 */
export async function downloadFile(path: string, filename: string): Promise<void> {
  const headers = new Headers()
  const token = getToken()
  if (token) headers.set('Authorization', `Bearer ${token}`)

  const resp = await fetch(path, { headers })
  if (!resp.ok) throw new ApiError(resp.status, `下载失败（${resp.status}）`)
  const blob = await resp.blob()
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = filename
  a.click()
  URL.revokeObjectURL(url)
}

/** 便捷方法。 */
export const http = {
  get: <T>(path: string) => api<T>(path),
  post: <T>(path: string, body?: unknown) =>
    api<T>(path, { method: 'POST', body: body === undefined ? undefined : JSON.stringify(body) }),
  delete: <T>(path: string) => api<T>(path, { method: 'DELETE' }),
  upload,
  downloadFile,
}
