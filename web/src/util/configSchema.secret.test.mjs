// 配置中心 schema 纯函数层的回归测试。
//
// 重点盯住 B20-P1-4 / 决策 5-c 的 secret 清除协议：
//   - 清除标记是 `{"__clear__": true}`，**必须原样**穿透 normalizeSection/coerceField，
//     不能被 String(value) / deepClone 变形、也不能被丢掉；
//   - 空串 / null / 掩码仍然是「保留旧值」，不能被误判成清除；
//   - 非 secret 字段的校验行为不受这层改动影响。
//
// 为什么要在导入前装 DOM 桩：configSchema 从 store.js 取 nestedGet，而 store.js 顶层
// 就 `reactive()` + `migrateLegacyToken()`（会读 document.cookie / localStorage）。
// 这是既有分层（审计 O2 记录的可测性问题），这里用最小桩避开，不改产品代码。

import test from 'node:test'
import assert from 'node:assert/strict'

// ---- 最小浏览器桩（必须在 import configSchema 之前生效）----------------------
// 只需要“形状对”：store.js 读 document.cookie / localStorage，@vue/runtime-dom 在
// 模块初始化时调一次 document.createElement('template')。不需要真的渲染任何东西。
const cookieJar = new Map()
const storage = new Map()
const stubElement = () => ({
  content: {},
  setAttribute() {},
  appendChild() {},
  removeChild() {},
  insertBefore() {},
  addEventListener() {},
  removeEventListener() {},
  style: {},
  dataset: {},
})
globalThis.document = {
  get cookie() {
    return [...cookieJar.entries()].map(([name, value]) => `${name}=${value}`).join('; ')
  },
  set cookie(raw) {
    const [pair] = String(raw).split(';')
    const eq = pair.indexOf('=')
    if (eq < 0) return
    const name = pair.slice(0, eq)
    const value = pair.slice(eq + 1)
    if (value === '') cookieJar.delete(name)
    else cookieJar.set(name, value)
  },
  documentElement: { dataset: {}, style: {} },
  createElement: stubElement,
  createElementNS: stubElement,
  createTextNode: stubElement,
  createComment: stubElement,
  querySelector: () => null,
  querySelectorAll: () => [],
  addEventListener() {},
  removeEventListener() {},
}
globalThis.window = globalThis.window || { addEventListener() {}, removeEventListener() {} }
globalThis.localStorage = {
  getItem: (key) => (storage.has(key) ? storage.get(key) : null),
  setItem: (key, value) => storage.set(key, String(value)),
  removeItem: (key) => storage.delete(key),
}

const {
  SECRET_CLEAR_MARKER,
  coerceField,
  isSecretField,
  normalizeSection,
  secretClearValue,
} = await import('./configSchema.js')

// ---- 固件：形状对齐 /config/schema 的返回 -----------------------------------
const secretField = {
  path: 'ai.api_key',
  label: 'API Key',
  kind: { type: 'secret' },
  default: '',
  effect: 'live',
}
const intField = { path: 'ai.timeout_ms', label: '超时', kind: { type: 'int' }, default: 30, effect: 'live' }
const stringField = { path: 'ai.endpoint', label: '端点', kind: { type: 'string' }, default: '', effect: 'live' }
const section = { name: 'ai', label: 'AI', fields: [secretField, intField, stringField] }

test('secret 字段识别与清除标记形状与后端逐字一致', () => {
  assert.equal(SECRET_CLEAR_MARKER, '__clear__')
  assert.equal(isSecretField(secretField), true)
  assert.equal(isSecretField(intField), false)
  assert.deepEqual(secretClearValue(), { __clear__: true })
  // 标记必须是且只能是这一个键、这一个布尔值：后端 apply_secret_clear_markers
  // 要求 map.len()==1 且值严格等于 Value::Bool(true)。
  assert.equal(Object.keys(secretClearValue()).length, 1)
  assert.equal(Object.is(secretClearValue().__clear__, true), true)
})

test('清除标记原样穿透 normalizeSection（不能被改写成字符串或丢掉）', () => {
  const errors = {}
  const out = normalizeSection(
    section,
    {
      api_key: secretClearValue(),
      timeout_ms: ' 30 ',
      endpoint: '  http://x  ',
    },
    errors,
  )

  assert.deepEqual(errors, {})
  assert.deepEqual(out.api_key, { __clear__: true }, '标记必须保持对象形状')
  assert.equal(Object.keys(out.api_key).length, 1)
  // 非 secret 字段照常规整（int 变数字），证明这层逻辑没被带坏。
  // string 类型刻意不做 trim（路径里可能有有意义的前后空格），这里把现状钉住。
  assert.equal(out.timeout_ms, 30)
  assert.equal(out.endpoint, '  http://x  ')
})

test('空串 / null / 掩码仍表示「保留旧值」，不会被误当清除', () => {
  for (const value of ['', null, '••••••']) {
    const errors = {}
    const out = normalizeSection(section, { api_key: value }, errors)
    assert.deepEqual(errors, {})
    assert.equal(out.api_key, value, `${JSON.stringify(value)} 必须原样保留（后端语义=保留旧值）`)
    assert.notDeepEqual(out.api_key, { __clear__: true })
  }
})

test('coerceField 对 secret 是 no-op，对非 secret 仍做校验', () => {
  const out = { api_key: secretClearValue() }
  coerceField(secretField, out, ['api_key'])
  assert.deepEqual(out.api_key, { __clear__: true })

  // 非法 int：必须照常收集到 errors[field.path]，同时保留原值供用户修改
  const bad = { timeout_ms: 'abc' }
  const errors = {}
  const normalized = normalizeSection(section, bad, errors)
  assert.deepEqual(errors, { 'ai.timeout_ms': '必须是数字' })
  assert.equal(normalized.timeout_ms, 'abc')

  // 空串对非 nullable 的 int 是「不能为空」
  const emptyErrors = {}
  normalizeSection(section, { timeout_ms: '  ' }, emptyErrors)
  assert.deepEqual(emptyErrors, { 'ai.timeout_ms': '不能为空' })
})
