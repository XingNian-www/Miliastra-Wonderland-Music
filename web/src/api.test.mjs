// 令牌持久化契约（决策 5-c + 审计返工 R2）。
//
// 这里钉死的是**行为**，不是实现：
//   1. 令牌只存在 cookie / 内存，readToken() 永远不读 localStorage；
//   2. 旧版 localStorage 副本在 cookie 写成功后必须被删除（不能留下第二份明文）；
//   3. **cookie 写失败时旧副本必须保留** —— 否则浏览器禁用 cookie 的用户刷新一次就
//      永久失去唯一令牌（R2 修的正是这个）；
//   4. 编码非法时 decodeURIComponent 回退原文，不能让整个面板不可用。
//
// 每个用例都用 `?case=N` 查询串重新导入模块：ESM 的模块实例按说明符缓存，
// 换一个说明符就能得到干净的 memoryToken / tokenPersistence，模拟“刷新页面”。

import test from 'node:test'
import assert from 'node:assert/strict'
import { pathToFileURL } from 'node:url'

const API_URL = pathToFileURL(new URL('./api.js', import.meta.url).pathname).href
let caseSeq = 0

/**
 * 极简浏览器桩。cookie setter 按真实浏览器的语义解析：
 * `name=value; path=/; max-age=N; SameSite=Strict`，`max-age=0` 表示删除。
 *
 * @param {{enabled?: boolean}} options enabled=false 模拟「浏览器禁用 cookie」：
 *   setter 静默成功但什么也不写 —— 这正是 R2 要对付的失败模式。
 */
function installBrowser({ enabled = true } = {}) {
  const jar = [] // [{ name, value }]
  const writes = [] // 原始 setter 入参，用于断言 SameSite / max-age
  const storage = new Map()

  const document = {
    get cookie() {
      return jar.map((item) => `${item.name}=${item.value}`).join('; ')
    },
    set cookie(raw) {
      writes.push(raw)
      if (!enabled) return // 静默失败：不抛错、也不写
      const [pair, ...attributes] = String(raw).split(';').map((part) => part.trim())
      const eq = pair.indexOf('=')
      if (eq < 0) return
      const name = pair.slice(0, eq)
      const value = pair.slice(eq + 1)
      const maxAge = attributes.find((item) => /^max-age=/i.test(item))
      const expired = maxAge ? Number(maxAge.split('=')[1]) <= 0 : false
      // 同名 cookie 按 path 可以并存多份：删除只删匹配 name 的全部（测试里不区分 path）。
      if (expired || value === '' && maxAge) {
        for (let i = jar.length - 1; i >= 0; i -= 1) if (jar[i].name === name) jar.splice(i, 1)
        return
      }
      for (let i = jar.length - 1; i >= 0; i -= 1) if (jar[i].name === name) jar.splice(i, 1)
      jar.push({ name, value })
    },
  }

  const localStorage = {
    getItem: (key) => (storage.has(key) ? storage.get(key) : null),
    setItem: (key, value) => storage.set(key, String(value)),
    removeItem: (key) => storage.delete(key),
  }

  globalThis.document = document
  globalThis.localStorage = localStorage
  globalThis.window = { dispatchEvent() {} }
  // 注意：api.js 写失败时会动态 import('./store.js') 去弹 toast。本文件的 document 桩
  // 没有 createElement，所以那次 import 会失败——没关系，api.js 自己 catch 掉了，
  // 只剩我们要断言的 console.warn。断言只看 warn，不看 toast。

  return {
    /** 直接塞一份“旧版双写”留下的 localStorage 副本。 */
    seedLegacy(value) {
      storage.set('miliastraAccessToken', value)
    },
    /** 直接塞一份历史 cookie（可以是未编码的脏值）。 */
    seedCookie(rawValue) {
      jar.push({ name: 'miliastraAccessToken', value: rawValue })
    },
    hasLegacy() {
      return storage.has('miliastraAccessToken')
    },
    legacy() {
      return storage.get('miliastraAccessToken')
    },
    writes,
    rawCookies() {
      return document.cookie
    },
  }
}

/** 每个用例一份全新模块实例，等价于“刷新页面后重新加载”。 */
async function loadApi() {
  caseSeq += 1
  return import(`${API_URL}?case=${caseSeq}`)
}

test('fresh：没有令牌时 migrate 返回空、readToken 读不到', async () => {
  installBrowser()
  const api = await loadApi()
  assert.equal(api.migrateLegacyToken(), '')
  assert.equal(api.readToken(), '')
})

test('迁移：旧副本进 cookie、副本被删除、readToken 走 cookie', async () => {
  const browser = installBrowser()
  browser.seedLegacy('legacy-token')
  const api = await loadApi()

  assert.equal(api.migrateLegacyToken(), 'legacy-token')
  assert.equal(api.readToken(), 'legacy-token')
  assert.equal(browser.hasLegacy(), false, 'cookie 写成功后必须删掉 localStorage 副本')
  assert.equal(api.tokenPersistence, 'cookie')
})

test('cookie 优先：cookie 有值时直接清掉旧副本', async () => {
  const browser = installBrowser()
  browser.seedCookie('cookie-token')
  browser.seedLegacy('legacy-token')
  const api = await loadApi()

  assert.equal(api.migrateLegacyToken(), 'cookie-token')
  assert.equal(api.readToken(), 'cookie-token')
  assert.equal(browser.hasLegacy(), false)
})

test('write/read 往返：需要编码的字符原样读回，且 cookie 带 SameSite=Strict', async () => {
  const browser = installBrowser()
  const api = await loadApi()

  assert.equal(api.writeToken('  a b%c;d  '), true, 'trim 后写入应成功')
  assert.equal(api.readToken(), 'a b%c;d')
  const last = browser.writes.at(-1)
  assert.match(last, /^miliastraAccessToken=a%20b%25c%3Bd;/)
  assert.match(last, /SameSite=Strict/)
  assert.match(last, /path=\//)
})

test('历史未编码 cookie 解码失败时回退原文，不让面板整体不可用', async () => {
  const browser = installBrowser()
  browser.seedCookie('bad%zz')
  const api = await loadApi()

  assert.equal(api.readToken(), 'bad%zz')
})

test('clearToken：删 cookie、清内存、清旧副本', async () => {
  const browser = installBrowser()
  browser.seedLegacy('legacy-token')
  const api = await loadApi()
  api.writeToken('t-1')
  browser.seedLegacy('legacy-token')

  api.clearToken()
  assert.equal(api.readToken(), '')
  assert.equal(browser.hasLegacy(), false)
  assert.ok(
    browser.writes.some((raw) => raw.startsWith('miliastraAccessToken=;') && /max-age=0/.test(raw)),
    '删除必须显式 max-age=0',
  )
})

test('R2：cookie 写失败时不删旧副本，刷新后仍能迁移回来', async () => {
  const browser = installBrowser({ enabled: false })
  browser.seedLegacy('only-copy-token')
  const api = await loadApi()

  const warnings = []
  const originalWarn = console.warn
  console.warn = (text) => warnings.push(String(text))
  let token
  try {
    token = api.migrateLegacyToken()
  } finally {
    console.warn = originalWarn
  }

  // 本次会话可用（内存兜底）
  assert.equal(token, 'only-copy-token')
  assert.equal(api.readToken(), 'only-copy-token')
  assert.equal(api.tokenPersistence, 'memory', '必须把“只在内存里”暴露出来，供 UI 措辞')
  // 唯一的持久副本不能被删掉
  assert.equal(browser.hasLegacy(), true, 'cookie 写失败时 localStorage 旧副本必须保留')
  assert.equal(browser.legacy(), 'only-copy-token')
  // 一次性诊断
  assert.equal(warnings.length, 1, '应当只警告一次')
  assert.match(warnings[0], /cookie/)

  // “刷新”一次：旧副本还在，所以还能恢复
  const refreshed = await loadApi()
  assert.equal(refreshed.migrateLegacyToken(), 'only-copy-token')
  assert.equal(refreshed.readToken(), 'only-copy-token')
})

test('R2：cookie 写成功后不再残留 localStorage 副本（常规路径不受影响）', async () => {
  const browser = installBrowser()
  browser.seedLegacy('legacy-token')
  const api = await loadApi()
  api.migrateLegacyToken()

  api.writeToken('newer-token')
  assert.equal(api.readToken(), 'newer-token')
  assert.equal(browser.hasLegacy(), false, '写入成功后不应把旧副本留着当第二份明文')
  assert.equal(api.tokenPersistence, 'cookie')
})
