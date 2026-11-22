// 配置生效级别文案的回归钉子（W2「文案在撒谎」修复）。
//
// 真实语义：`Effect::IdleReload` **不是**「任务结束后重载某个组件」，而是保存后
// 等运行态空闲，然后**整个子进程**以退出码 75 退出，由 `src/main.rs` 的看门狗
// 重新拉起（`src/lib.rs` 的 `CONFIG_RELOAD_EXIT_CODE`）。用户会据此决定
// 「现在保存会不会打断当前任务」，所以文案必须出现「重启 / 整程序」，
// 并且不能再出现「任务结束后重载」这类把整进程重启说成组件热重载的措辞。
//
// 为什么要在导入前装 DOM 桩：configSchema 从 store.js 取 nestedGet，而 store.js
// 顶层就 `reactive()` + `migrateLegacyToken()`（会读 document.cookie / localStorage）。
// 这里沿用 configSchema.secret.test.mjs 的最小桩，与产品代码解耦。

import test from 'node:test'
import assert from 'node:assert/strict'

// ---- 最小浏览器桩（必须在 import configSchema 之前生效）----------------------
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

const { EFFECT_LABELS, effectLabel, saveSummary } = await import('./configSchema.js')

test('idle_reload_label_matches_real_semantics', () => {
  const label = EFFECT_LABELS.idleReload

  // 必须说明「会重启」且是「整程序」，而不是模糊的「重载」。
  assert.match(label, /重启/, `IdleReload 文案必须出现「重启」，实际：${label}`)
  assert.match(label, /整程序|整个程序/, `IdleReload 文案必须说明「整程序」，实际：${label}`)

  // 旧文案的原话，以及同义的误导句式，都不能再出现。
  assert.doesNotMatch(label, /任务结束后重载/, `IdleReload 文案仍含旧措辞：${label}`)
  assert.doesNotMatch(label, /任务结束后/, `IdleReload 文案不得把语义说成「任务结束后」：${label}`)
  assert.doesNotMatch(label, /^重载$/, 'IdleReload 文案不能只写「重载」')
  assert.notEqual(label, EFFECT_LABELS.restart, 'IdleReload 与「重启生效」是两种级别，不能复用同一文案')

  // 字段旁的标签走同一条映射，不能被 fallback 成 restart。
  assert.equal(effectLabel({ effect: 'idleReload' }), label)
  assert.equal(effectLabel({ effect: 'unknown-effect' }), EFFECT_LABELS.restart)

  // 整张表都不该再有「任务结束后重载」这种把整进程重启说成组件重载的说法。
  for (const [effect, text] of Object.entries(EFFECT_LABELS)) {
    assert.doesNotMatch(text, /任务结束后重载/, `${effect} 文案仍含旧措辞：${text}`)
  }
})

test('save_summary_uses_the_same_wording', () => {
  const summary = saveSummary(
    { revision: 7, runtimeReloadFields: ['tts.endpoint', 'window.capture_method'] },
    null,
  )
  assert.match(summary, /空闲后整程序重启 2 项/)
  assert.doesNotMatch(summary, /任务结束后重载/)
})
