// 面板 API 封装：唯一出网口。
//
// 约定（改这里之前先读完）：
//   1. 面板里**不允许**出现裸 `fetch()`；所有请求都走 request / requestJson /
//      getJson / fetchBlob / fetchRaw，这样令牌注入、401 处理、错误文案只有一份。
//   2. 令牌一律由 headers() 注入 `X-Miliastra-Token`，调用方不要自己拼鉴权头。
//   3. 401 统一抛 AuthError，由 store.handleAuth() 清令牌并提示去设置页重填。
//   4. 非 2xx 统一抛 Error，文案优先取响应体里的 message / error 字段。
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

/**
 * 发一次原始请求，返回未解析的 Response。
 * 只给 request / fetchBlob / fetchRaw 用；页面不要直接调。
 */
async function send(path, method = 'GET', options = {}) {
  const response = await fetch(path, {
    method,
    headers: headers(options.headers),
    body: options.body,
    signal: options.signal,
  })
  if (response.status === 401) throw new AuthError('访问令牌无效或已过期')
  if (!response.ok) throw new Error(await errorMessage(response))
  return response
}

/** 从失败响应里取一段人能读懂的文案；响应体不是 JSON 时退回原文或 HTTP 状态。 */
async function errorMessage(response) {
  let text = ''
  try {
    text = await response.text()
  } catch {
    return `HTTP ${response.status}`
  }
  let message = text.trim() || `HTTP ${response.status}`
  try {
    const parsed = JSON.parse(text)
    message = parsed.message || parsed.error || message
  } catch {
    /* 非 JSON 错误体：直接用原文 */
  }
  return message
}

export async function request(path, method = 'GET', options = {}) {
  return (await send(path, method, options)).text()
}

/**
 * 请求二进制响应（截图、证据原图等），返回 Blob。
 * 调用方负责在替换旧图时 URL.revokeObjectURL()。
 */
export async function fetchBlob(path, options = {}) {
  return (await send(path, options.method || 'GET', options)).blob()
}

/**
 * 请求「原样文本」响应（SSE 文本流、纯文本日志等），不做 JSON 解析。
 * 与 request() 的区别只在语义：这里明确表达“我要的是原文，不是结构”。
 */
export async function fetchRaw(path, options = {}) {
  return (await send(path, options.method || 'GET', options)).text()
}

function parseJson(text, path) {
  try {
    return JSON.parse(text)
  } catch {
    throw new Error(`响应不是 JSON（${path}）：${text.slice(0, 120)}`)
  }
}

export async function requestJson(path, body) {
  const text = await request(path, 'POST', {
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body == null ? {} : body),
  })
  return parseJson(text, path)
}

export async function getJson(path) {
  return parseJson(await request(path), path)
}
