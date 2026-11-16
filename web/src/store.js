import { reactive } from 'vue'
import { AuthError, getJson, readToken, requestJson, writeToken } from './api.js'

export const state = reactive({
  token: readToken(),
  route: 'config',
  message: { text: '', kind: '' },
  busy: false,
  revision: null,
  schema: [],
  config: null,
  dirty: {},
  fieldErrors: {},
  player: null,
  health: null,
  lastRefresh: '',
  autoRefresh: true,
})

export function toast(text, kind = '') {
  state.message = { text: text || '', kind }
  if (text) {
    clearTimeout(toast._timer)
    toast._timer = setTimeout(() => {
      if (state.message.text === text) state.message = { text: '', kind: '' }
    }, 6000)
  }
}

export function markDirty(section) {
  state.dirty = { ...state.dirty, [section]: true }
}

export function isDirty() {
  return Object.keys(state.dirty).length > 0
}

export function clearDirty() {
  state.dirty = {}
}

export function setToken(token) {
  writeToken(token)
  state.token = readToken()
}

export function handleAuth(error) {
  if (error instanceof AuthError) {
    state.token = ''
    toast('访问令牌无效或已过期，请到「设置」页填入令牌', 'err')
    return true
  }
  return false
}

export async function loadConfig(force = false) {
  if (state.config && !force) return state.config
  state.busy = true
  try {
    const [schema, config] = await Promise.all([
      getJson('/config/schema'),
      getJson('/config'),
    ])
    state.schema = Array.isArray(schema) ? schema : schema.sections || []
    state.config = config
    state.revision = config && config.revision != null ? config.revision : null
    return config
  } finally {
    state.busy = false
  }
}

export async function loadStatus() {
  try {
    state.health = await getJson('/health')
  } catch (error) {
    if (!handleAuth(error)) state.health = null
  }
  try {
    state.player = await getJson('/status')
  } catch (error) {
    if (!handleAuth(error)) state.player = null
  }
  state.lastRefresh = new Date().toLocaleTimeString()
}

export async function saveSections(sections, after) {
  state.busy = true
  state.fieldErrors = {}
  try {
    const baseRevision = state.revision != null ? state.revision : 0
    const validation = await requestJson('/config/validate', { sections })
    if (Array.isArray(validation) && validation.length) {
      const errors = {}
      for (const item of validation) errors[item.field || ''] = item.message || ''
      state.fieldErrors = errors
      toast(validation.map((item) => item.message).join('；'), 'err')
      return false
    }
    if (validation && validation.errors && validation.errors.length) {
      const errors = {}
      for (const item of validation.errors) errors[item.field || ''] = item.message || ''
      state.fieldErrors = errors
      toast(validation.errors.map((item) => item.message).join('；'), 'err')
      return false
    }
    const response = await requestJson('/config/save', { baseRevision, sections })
    if (response && response.ok === false) {
      toast(response.message || '保存失败', 'err')
      return false
    }
    if (after) after(response)
    return true
  } catch (error) {
    if (!handleAuth(error)) toast(`保存失败：${error.message || error}`, 'err')
    return false
  } finally {
    state.busy = false
  }
}

export function nestedGet(object, path) {
  return String(path)
    .split('.')
    .reduce((value, key) => (value == null ? value : value[key]), object)
}

export function nestedCloneSet(target, path, value) {
  const segments = String(path).split('.')
  let cursor = target
  for (const segment of segments.slice(0, -1)) {
    if (cursor[segment] == null || typeof cursor[segment] !== 'object') cursor[segment] = {}
    cursor = cursor[segment]
  }
  cursor[segments[segments.length - 1]] = value
}
