// `GET /config` 的容错加载状态 → 面板展示口径。
//
// 抽成纯函数是为了能在 node:test 里直接断言，不必起 Vue 运行时。
// **缺失即降级为不显示**：这些是只读诊断字段，绝不能因为它们让配置页报错。

/** 后端加载模式 → 中文短标签。 */
const MODE_LABELS = { lenient: '容错', strict: '严格' }

/**
 * @param {object|null|undefined} config `GET /config` 响应
 * @returns {{modeLabel: string, count: number, hasQuarantine: boolean, title: string}}
 */
export function describeLoadStatus(config) {
  const payload = config && typeof config === 'object' ? config : {}
  const modeLabel = MODE_LABELS[payload.loadMode] ?? ''
  const entries = Array.isArray(payload.quarantined) ? payload.quarantined : []
  const count =
    typeof payload.quarantinedCount === 'number' ? payload.quarantinedCount : entries.length
  const hasQuarantine = count > 0
  // 面板详情要"是哪一条 + 为什么"，所以保留结构化条目而不是只拼字符串。
  const rows = entries
    .map((entry) => {
      if (!entry || typeof entry !== 'object') return null
      const ref = entry.elementRef || entry.element_ref
      if (!ref) return null
      return { ref: String(ref), reason: String(entry.reason ?? '') }
    })
    .filter(Boolean)
  const title = hasQuarantine
    ? rows.length
      ? `已被隔离的条目：\n${rows.map((row) => row.ref).join('\n')}`
      : '库里有坏条目已被隔离，程序照常启动'
    : ''
  return { modeLabel, count, hasQuarantine, title, entries: rows }
}
