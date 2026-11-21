// 配置中心：schema 驱动的纯函数层。
//
// 这里只放「不碰 Vue、不碰 DOM」的规则：字段分类、默认值展示、草稿值规整与校验。
// composables 负责状态与副作用，组件负责渲染；所有格式错误的文案集中在本文件，
// 保证「保存前的本地校验」与「字段旁的错误提示」永远同一套措辞。

import { f32Text } from './format.js'
import { nestedGet } from '../store.js'

/** 配置中心的 IA 与旧版一致：这些段各有独立页面，不在配置中心重复出现。 */
export const HIDDEN_SECTIONS = new Set(['identity', 'ai', 'ai_chat', 'tts', 'command_access'])

export const DOMAIN_ORDER = ['recognition', 'playback', 'interaction', 'entertainment', 'system']

export const DOMAIN_LABELS = {
  recognition: '识别与画面',
  playback: '播放与点歌',
  interaction: 'AI 与交互',
  entertainment: '娱乐玩法',
  system: '系统',
}

export const EFFECT_LABELS = {
  live: '立即生效',
  idleReload: '任务结束后重载',
  playbackIdleReload: '播放结束/暂停后重载',
  restart: '重启生效',
}

const EFFECT_CLASSES = {
  live: 'live',
  idleReload: 'idle-reload',
  playbackIdleReload: 'playback-idle-reload',
  restart: 'restart',
}

const RECT_LABELS = { x: 'X', y: 'Y', width: '宽', height: '高' }

const KIND_CLASSES = {
  int: 'number',
  float: 'number',
  string: 'path',
  path: 'path',
  text: 'text',
  bool: 'bool',
  enum: 'enum',
  object: 'object',
  stringArray: 'array',
  array: 'array',
  rect: 'rect',
  point: 'point',
  secret: 'secret',
  modelName: 'string',
}

export function kindOf(field) {
  return field && field.kind ? field.kind.type : 'string'
}

/** 用于样式钩子 `data-kind`，让不同控件有各自的排版（number / path / rect …）。 */
export function kindClass(field) {
  return KIND_CLASSES[kindOf(field)] || 'other'
}

export function effectLabel(field) {
  return EFFECT_LABELS[field.effect] || EFFECT_LABELS.restart
}

export function effectClass(field) {
  return EFFECT_CLASSES[field.effect] || EFFECT_CLASSES.restart
}

/** bootstrap 字段来自 config.yaml，不可在面板里改，也不算「有默认值」。 */
export function hasFieldDefault(field) {
  return field.default !== undefined && field.default !== null && field.source !== 'bootstrap'
}

export function sectionFieldCount(section) {
  return Array.isArray(section && section.fields) ? section.fields.length : 0
}

export function deepClone(value) {
  if (value === undefined) return undefined
  if (value === null) return null
  return JSON.parse(JSON.stringify(value))
}

export function setNested(target, segments, value) {
  let cursor = target
  for (const segment of segments.slice(0, -1)) {
    if (cursor[segment] == null || typeof cursor[segment] !== 'object') cursor[segment] = {}
    cursor = cursor[segment]
  }
  cursor[segments[segments.length - 1]] = value
}

export function deleteNested(target, segments) {
  if (!target || typeof target !== 'object' || !segments.length) return
  let cursor = target
  for (const segment of segments.slice(0, -1)) {
    if (!cursor || typeof cursor !== 'object') return
    cursor = cursor[segment]
  }
  if (cursor && typeof cursor === 'object') delete cursor[segments[segments.length - 1]]
}

/** 字段路径 `a.b.c` 去掉段名后的剩余段，例如 `playback.volume` → ['volume']。 */
export function fieldSegments(field) {
  return String(field.path).split('.').slice(1)
}

/**
 * 默认值的人类可读文本：f32 阈值还原成 0.9 而不是 0.8999999761581421，
 * 对象按 JSON 展示，超长截断到 48 字符。
 */
export function defaultText(field) {
  const readable = (value) => {
    if (typeof value === 'number') return Number(f32Text(value))
    if (Array.isArray(value)) return value.map(readable)
    if (value && typeof value === 'object') {
      return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, readable(item)]))
    }
    return value
  }
  let text
  if (typeof field.default === 'string') text = field.default
  else text = JSON.stringify(readable(field.default))
  if (text == null) text = String(field.default)
  return text.length > 48 ? `${text.slice(0, 48)}…` : text
}

export function formatNow() {
  return new Date().toLocaleString('zh-CN', { hour12: false })
}

export function formatTime(ms) {
  if (ms == null || ms === '') return '时间未知'
  return new Date(Number(ms)).toLocaleString('zh-CN', { hour12: false })
}

/**
 * 保存回执的人话总结：当前版本 + 各生效方式分别有多少字段命中。
 * 纯函数（版本号由调用方传入），便于单测与复用。
 *
 * @param {object|null} response /config/save 的响应
 * @param {number|string|null} revision 兜底版本号（响应里没有 revision 时用）
 */
export function saveSummary(response, revision) {
  const current =
    response && response.revision != null
      ? String(response.revision)
      : String(revision == null ? '—' : revision)
  const parts = [`已保存，当前版本 ${current}`]
  if (response && Array.isArray(response.appliedLiveFields) && response.appliedLiveFields.length) {
    parts.push(`立即生效 ${response.appliedLiveFields.length} 项`)
  }
  if (response && Array.isArray(response.runtimeReloadFields) && response.runtimeReloadFields.length) {
    parts.push(`任务结束后重载 ${response.runtimeReloadFields.length} 项`)
  }
  if (response && Array.isArray(response.playbackReloadFields) && response.playbackReloadFields.length) {
    parts.push(`播放结束/暂停后重载 ${response.playbackReloadFields.length} 项`)
  }
  if (response && response.restartRequired) parts.push('部分字段需重启生效')
  return parts.join('；')
}

/** 单个字段的草稿值 → 提交值；不合法时抛错，由 normalizeSection 收集到 errors。 */
export function coerceField(field, out, segments) {
  const kind = kindOf(field)
  const value = nestedGet(out, segments.join('.'))
  const nullable = field.nullable === true

  if (kind === 'int' || kind === 'float') {
    if (value === null) {
      if (!nullable) throw new Error('不能为空')
      return
    }
    const raw = String(value).trim()
    if (raw === '') {
      if (nullable) {
        setNested(out, segments, null)
        return
      }
      throw new Error('不能为空')
    }
    const number = Number(raw)
    if (!Number.isFinite(number)) throw new Error('必须是数字')
    if (kind === 'int' && !Number.isInteger(number)) throw new Error('必须是整数')
    if (kind === 'int' && !Number.isSafeInteger(number)) throw new Error('超出安全整数范围')
    const min = field.kind ? field.kind.min : null
    const max = field.kind ? field.kind.max : null
    if (min != null && number < min) throw new Error(`不能小于 ${min}`)
    if (max != null && number > max) throw new Error(`不能大于 ${max}`)
    setNested(out, segments, number)
    return
  }

  if (kind === 'stringArray' || kind === 'array') {
    const list = Array.isArray(value) ? value : []
    const cleaned = list.map((item) => String(item == null ? '' : item).trim()).filter((item) => item !== '')
    setNested(out, segments, cleaned)
    return
  }

  if (kind === 'rect' || kind === 'point') {
    if (value === null) {
      if (!nullable) throw new Error('坐标不能为空')
      return
    }
    if (typeof value !== 'object' || Array.isArray(value)) throw new Error('坐标必须是对象')
    const keys = kind === 'point' ? ['x', 'y'] : ['x', 'y', 'width', 'height']
    const result = {}
    const missing = []
    for (const key of keys) {
      const raw = value[key]
      if (raw === null || raw === undefined || String(raw).trim() === '') {
        missing.push(key)
        continue
      }
      const number = Number(raw)
      if (!Number.isSafeInteger(number)) throw new Error(`${RECT_LABELS[key]} 必须是整数`)
      result[key] = number
    }
    if (missing.length) {
      if (!Object.keys(result).length && nullable) {
        setNested(out, segments, null)
        return
      }
      throw new Error(`缺少 ${missing.map((key) => RECT_LABELS[key]).join('、')}`)
    }
    if (kind === 'rect' && (result.width <= 0 || result.height <= 0)) {
      throw new Error('宽高必须大于 0')
    }
    setNested(out, segments, result)
    return
  }

  if (kind === 'enum') {
    if (value === null) {
      if (!nullable) throw new Error('必须选择一个值')
      return
    }
    const text = String(value)
    if (text === '') throw new Error('必须选择一个值')
    setNested(out, segments, text)
    return
  }

  if (kind === 'bool') {
    setNested(out, segments, value === true)
    return
  }

  if (kind === 'object') {
    if (value !== null && typeof value !== 'object') {
      throw new Error('必须是 JSON 对象或 null')
    }
    return
  }

  if (kind === 'secret') return

  // string / path / text / modelName
  if (value === null) {
    if (!nullable) setNested(out, segments, '')
    return
  }
  setNested(out, segments, String(value))
}

/**
 * 把一段草稿规整成可提交的对象：
 *   - bootstrap 字段整条剔除（由 config.yaml 提供，面板不写回）；
 *   - 父级为 null 时不再深入子字段（对象整体被置空）；
 *   - 其余字段逐个 coerce，错误按字段路径收集。
 */
export function normalizeSection(section, draftSection, errors) {
  const out = draftSection && typeof draftSection === 'object' ? deepClone(draftSection) : {}
  for (const field of section.fields || []) {
    const segments = fieldSegments(field)
    if (!segments.length) continue
    if (field.source === 'bootstrap') {
      deleteNested(out, segments)
      continue
    }
    if (segments.length > 1) {
      const parentValue = nestedGet(out, segments.slice(0, -1).join('.'))
      if (parentValue === null) continue
    }
    if (nestedGet(out, segments.join('.')) === undefined) continue
    try {
      coerceField(field, out, segments)
    } catch (error) {
      errors[field.path] = error.message
    }
  }
  return out
}
