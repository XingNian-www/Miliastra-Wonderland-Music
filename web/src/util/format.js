// 面板数值/路径的“人类可读等价”工具。
//
// 配置里很多阈值是 f32，序列化成 f64 会变成 0.8999999761581421；
// 显示默认值、对比保存值时应还原成最短 f32 表示（0.9）。
// 路径在 Windows 上可能出现 `\` 与 `/`、相对与绝对两种形态。

export function f32Text(value) {
  if (value == null || value === '') return ''
  const number = Number(value)
  if (!Number.isFinite(number)) return String(value)
  const rounded = Math.fround(number)
  // 从 1 位到 9 位有效数字，找第一个回读成同一 f32 的最短表示。
  for (let precision = 1; precision <= 9; precision += 1) {
    const candidate = Number(rounded.toPrecision(precision))
    if (Math.fround(candidate) === rounded) return String(candidate)
  }
  return String(rounded)
}

export function normalizePathText(value) {
  return String(value == null ? '' : value).replace(/\\/g, '/')
}

export function sameTextOrPath(a, b) {
  if (a === b) return true
  const left = normalizePathText(a)
  const right = normalizePathText(b)
  if (left === right) return true
  if (!left.includes('/') || !right.includes('/')) return false
  const [long, short] = left.length >= right.length ? [left, right] : [right, left]
  return long.endsWith(short) && long[long.length - short.length - 1] === '/'
}

export function sameValue(a, b) {
  if (a === b) return true
  if (typeof a === 'number' || typeof b === 'number') {
    const left = Number(a)
    const right = Number(b)
    if (Number.isFinite(left) && Number.isFinite(right)) {
      const diff = Math.abs(left - right)
      return diff <= Math.max(1e-6, Math.abs(left) * 1e-6, Math.abs(right) * 1e-6)
    }
  }
  if (typeof a === 'string' && typeof b === 'string') return sameTextOrPath(a, b)
  if (a && b && typeof a === 'object' && typeof b === 'object') {
    if (Array.isArray(a) !== Array.isArray(b)) return false
    const keys = new Set([...Object.keys(a), ...Object.keys(b)])
    for (const key of keys) {
      if (!sameValue(a[key], b[key])) return false
    }
    return true
  }
  return false
}

const PLAYER_STATE_LABELS = {
  playing: '播放中',
  paused: '已暂停',
  stopped: '已停止',
  idle: '空闲',
  requested_song_playing: '点歌播放中',
  requested_song_queued: '点歌排队中',
  external_playing: '外部播放中',
  buffering: '缓冲中',
  failed: '失败',
}

export function playerStateLabel(value) {
  const raw = String(value == null ? '' : value)
  return PLAYER_STATE_LABELS[raw] || raw || '未知'
}

// 后端把 secret 字段显示成掩码；回传这些值会被当成真实密钥。
export const SECRET_MASK = '••••••'

export function stripSecretMask(value) {
  if (Array.isArray(value)) return value.map(stripSecretMask)
  if (value && typeof value === 'object') {
    const result = {}
    for (const [key, item] of Object.entries(value)) {
      if (typeof item === 'string' && item === SECRET_MASK) continue
      result[key] = stripSecretMask(item)
    }
    return result
  }
  return value
}

export function friendlyError(error) {
  const message = String(error && error.message ? error.message : error || '')
  if (!message) return '未知错误'
  if (/Failed to fetch|NetworkError|Load failed|ERR_CONNECTION|ERR_NETWORK/i.test(message)) {
    return '无法连接后端：请检查面板地址与程序是否在运行'
  }
  return message
}
