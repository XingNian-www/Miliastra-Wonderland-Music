// 面板 API 封装：唯一出网口。
//
// 约定（改这里之前先读完）：
//   1. 面板里**不允许**出现裸 `fetch()`；所有请求都走 request / requestJson /
//      getJson / fetchBlob / fetchRaw，这样令牌注入、401 处理、错误文案只有一份。
//   2. 令牌一律由 headers() 注入 `X-Miliastra-Token`，调用方不要自己拼鉴权头。
//   3. 401 统一抛 AuthError，由 store.handleAuth() 清令牌并提示去设置页重填。
//   4. 非 2xx 统一抛 Error，文案优先取响应体里的 message / error 字段。
//   5. 假数据（`--mode mock` 构建，见 scripts/panel-visual-check.sh）也只在这一层
//      换掉 `fetch`：页面与真实请求走**同一份代码**，任何页面都不允许写
//      `if (mock)` 分支——否则截出来的就不是生产路径。
import { MOCK_ENABLED } from './mock/buildFlag.js'

const TOKEN_COOKIE = 'miliastraAccessToken'
/**
 * 旧版本把令牌双写到 localStorage 的键。只用于启动时的一次性迁移/清理，
 * **不再作为读取来源**：迁移完成后副本会被删除，之后只认 cookie / 内存。
 */
const LEGACY_TOKEN_KEY = 'miliastraAccessToken'
const TOKEN_MAX_AGE = 31536000
/** 与文档一致：令牌 cookie 使用 SameSite=Strict（旧实现误写成 Lax）。 */
const TOKEN_SAME_SITE = 'Strict'

export class AuthError extends Error {}

/** 内存态兜底：cookie 被禁用时，本次页面会话内仍能继续使用已填写的令牌。 */
let memoryToken = ''

/**
 * 令牌的实际持久化位置，由 writeToken 回读校验后更新：
 *   - `'cookie'`：cookie 确实写进去了，刷新后仍在（正常路径）；
 *   - `'memory'`：本浏览器拒绝写 cookie（禁用 / 策略拦截），令牌只在本次页面会话内有效。
 * 导出的是活绑定，设置页可以在写入后直接读它来措辞提示，不需要额外回调。
 */
export let tokenPersistence = 'cookie'

/** 诊断提示一个会话只弹一次，避免每次写令牌都刷屏。 */
let persistenceWarned = false

function decodeCookieValue(raw) {
  try {
    return decodeURIComponent(raw)
  } catch {
    // 历史 cookie 可能是未编码的原值：解码失败时按原文回退，避免整个面板不可用。
    return raw
  }
}

/** 同名 cookie 全部取值：同一名字可能因 path 不同存在多份，回读校验时必须逐个比对。 */
function allCookieTokens() {
  return String(document.cookie || '')
    .split(';')
    .map((item) => item.trim())
    .filter((item) => item.startsWith(`${TOKEN_COOKIE}=`))
    .map((item) => decodeCookieValue(item.slice(TOKEN_COOKIE.length + 1)))
}

function readCookieToken() {
  return allCookieTokens()[0] || ''
}

function purgeLegacyToken() {
  try {
    localStorage.removeItem(LEGACY_TOKEN_KEY)
  } catch {
    /* 隐私模式下 localStorage 不可用：忽略；令牌本来就只该在 cookie / 内存里 */
  }
}

/**
 * 旧版双写迁移（启动时调用一次）：
 *   - cookie 已有令牌：直接删掉 localStorage 副本，之后只读 cookie；
 *   - cookie 缺失但 localStorage 有：复制进 cookie 后删除旧副本，刷新仍可用。
 * 迁移结束后 readToken() 不再读 localStorage。
 *
 * 唯独 cookie 写不进去时不删旧副本：那是用户**唯一**的持久副本，删掉一次就再也要不回来
 * （见 writeToken 的说明）。
 */
export function migrateLegacyToken() {
  const cookieToken = readCookieToken()
  if (cookieToken) {
    purgeLegacyToken()
    return cookieToken
  }
  let legacy = ''
  try {
    legacy = localStorage.getItem(LEGACY_TOKEN_KEY) || ''
  } catch {
    return ''
  }
  if (!legacy) return ''
  writeToken(legacy)
  return legacy
}

export function readToken() {
  return readCookieToken() || memoryToken
}

/**
 * 写入令牌（cookie + 内存），并在写完后**回读校验**。
 *
 * 为什么必须回读：浏览器禁用 cookie / 第三方策略拦截 / 存储配额满时，
 * `document.cookie = ...` 是**静默失败**的（不抛错、也不写进去）。旧实现写完就直接
 * `purgeLegacyToken()`，于是用户的唯一持久副本被删掉、cookie 又是空的，刷新一次就
 * 永久失去令牌，且界面只会显示「需要令牌」，没有任何可诊断信号。
 *
 * 因此这里按回读结果分两种收场：
 *   - 回读等于刚写入的值：正常路径，可以安全清掉旧版 localStorage 副本；
 *   - 回读不一致：**保留旧副本**（迁移场景下它是用户唯一的持久副本；下次启动
 *     migrateLegacyToken 还能再试一次），只把令牌留在内存里保证本次会话可用，
 *     并给一次性诊断提示。
 * 注意这不是「退回 localStorage 持久化」：常规路径仍然只写 cookie，旧副本只在
 * 写入失败这条异常分支上被保留，读取路径永远不碰它。
 *
 * @returns {boolean} cookie 是否真的写成功（也同步进 tokenPersistence）
 */
export function writeToken(token) {
  const value = String(token || '').trim()
  memoryToken = value
  // 删除 cookie 也走同一条回读校验：`max-age=0` 在禁用 cookie 的浏览器里同样静默失败。
  document.cookie = value
    ? `${TOKEN_COOKIE}=${encodeURIComponent(value)}; path=/; max-age=${TOKEN_MAX_AGE}; SameSite=${TOKEN_SAME_SITE}`
    : `${TOKEN_COOKIE}=; path=/; max-age=0; SameSite=${TOKEN_SAME_SITE}`
  const persisted = value
    ? allCookieTokens().includes(value)
    : // 删除：判定标准是「同名 cookie 一个不剩」，而不是「读回空串」——
      // 浏览器给不存在的 cookie 赋值 max-age=0 也会成功，不能据此外推写能力。
      allCookieTokens().length === 0
  tokenPersistence = persisted ? 'cookie' : 'memory'
  if (persisted) {
    // 前端不再写 localStorage 副本，cookie 写成功后顺手清掉旧版本留下的那一份。
    purgeLegacyToken()
    persistenceWarned = false
  } else {
    warnPersistenceOnce(value)
  }
  return persisted
}

/**
 * 一次性诊断：console.warn 始终给（开发者可追），toast 只在面板里存在 store 时给。
 * 这里不 import store —— store 反过来 import api，静态依赖会成环；改为在写入时
 * 按需动态取一次，拿不到（例如 Node 测试环境）就只留 console 警告。
 */
function warnPersistenceOnce(value) {
  if (persistenceWarned) return
  persistenceWarned = true
  const text = value
    ? '本浏览器拒绝写入 cookie，访问令牌只在本次页面会话内有效；关闭或刷新页面后需要重新填写。请允许本站使用 cookie 以持久保存令牌。'
    : '本浏览器拒绝写入 cookie，访问令牌的清除只对本次页面会话有效；刷新后请再次清除。'
  try {
    console.warn(`[miliastra] ${text}`)
  } catch {
    /* 极端环境下 console 不可用：忽略，不影响主流程 */
  }
  notifyToast(text)
}

/**
 * 尽力弹一次面板 toast。
 *
 * 为什么是动态 import：store 反过来静态依赖本模块，静态 import store 会成环。
 * 浏览器里 store 早已被页面加载，动态 import 命中模块缓存，没有额外代价；
 * Node 测试环境没有 window 就直接跳过，只留 console.warn 这条信号。
 */
function notifyToast(text) {
  if (typeof window === 'undefined') return
  import('./store.js')
    .then(({ toast }) => toast(text, 'err'))
    .catch(() => {
      /* store 不可用（极端加载顺序）：console.warn 已经给过信号，不升级成异常 */
    })
}

export function clearToken() {
  memoryToken = ''
  document.cookie = `${TOKEN_COOKIE}=; path=/; max-age=0; SameSite=${TOKEN_SAME_SITE}`
  tokenPersistence = allCookieTokens().length ? 'cookie' : 'memory'
  purgeLegacyToken()
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
  // 假数据模式下换掉 fetch 本身，而不是在上层加分支：`send` 之后的 401 / 非 2xx /
  // 错误文案 / blob 解析全部原样生效，页面拿到的调用栈与真实请求完全一致。
  // 判定写作 `if (MOCK_ENABLED)` 而不是引入动态 import：常量在构建期就被折叠，
  // 生产产物里这段与下面的 `import()` 都会被 tree-shake 掉（见 buildFlag.js）。
  if (MOCK_ENABLED) {
    const { mockFetch } = await import('./mock/index.js')
    return sendWith(mockFetch, path, method, options)
  }
  return sendWith(fetch, path, method, options)
}

/** 真正的发送逻辑；`doFetch` 只是把 `fetch` 参数化，便于假数据替换而不复制粘贴。 */
async function sendWith(doFetch, path, method, options) {
  const response = await doFetch(path, {
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
