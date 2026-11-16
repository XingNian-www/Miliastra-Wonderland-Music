// 面板 API 封装：token 注入、JSON 解析、401 统一处理。
const TOKEN_COOKIE = 'miliastraAccessToken'
const TOKEN_KEY = 'miliastraAccessToken'

export class AuthError extends Error {}

export function readToken() {
  const match = document.cookie.match(/(?:^|;\s*)miliastraAccessToken=([^;]*)/)
  if (match) return decodeURIComponent(match[1])
  return localStorage.getItem(TOKEN_KEY) || ''
}

export function writeToken(token) {
  const value = String(token || '').trim()
  localStorage.setItem(TOKEN_KEY, value)
  document.cookie = `${TOKEN_COOKIE}=${encodeURIComponent(value)}; path=/; max-age=31536000; SameSite=Lax`
}

export function clearToken() {
  localStorage.removeItem(TOKEN_KEY)
  document.cookie = `${TOKEN_COOKIE}=; path=/; max-age=0; SameSite=Lax`
}

function headers(extra) {
  const token = readToken()
  return { ...(token ? { 'X-Miliastra-Token': token } : {}), ...(extra || {}) }
}

export async function request(path, method = 'GET', options = {}) {
  const response = await fetch(path, {
    method,
    headers: headers(options.headers),
    body: options.body,
    signal: options.signal,
  })
  const text = await response.text()
  if (response.status === 401) throw new AuthError('访问令牌无效或已过期')
  if (!response.ok) {
    let message = text || `HTTP ${response.status}`
    try {
      const parsed = JSON.parse(text)
      message = parsed.message || parsed.error || message
    } catch {}
    throw new Error(message)
  }
  return text
}

export async function requestJson(path, body) {
  const text = await request(path, 'POST', {
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body == null ? {} : body),
  })
  try {
    return JSON.parse(text)
  } catch {
    throw new Error(`响应不是 JSON（${path}）：${text.slice(0, 120)}`)
  }
}

export async function getJson(path) {
  const text = await request(path)
  try {
    return JSON.parse(text)
  } catch {
    throw new Error(`响应不是 JSON（${path}）：${text.slice(0, 120)}`)
  }
}
