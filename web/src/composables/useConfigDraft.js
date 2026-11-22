// 配置中心：草稿状态与字段读写。
//
// 草稿是一个 reactive 的 `{ 段名: 段对象 }`，只保存「用户改过的值」；
// 脏标记写进 state.dirty（全局面板状态），由 useConfigForm 负责「只放本页脏段」，
// 这样切页面回来还能续着改，别的页面保存时也不会把本页的脏标记一起清掉。
// 组件不直接改草稿，一律走这里的 setFieldValue / resetField / restoreSectionDefaults。

import { computed, reactive, ref } from 'vue'
import { clearDirty, markDirty, nestedGet, state } from '../store.js'
import {
  deepClone,
  fieldSegments,
  hasFieldDefault,
  isSecretField,
  secretClearValue,
  setNested,
} from '../util/configSchema.js'

export function useConfigDraft(sections, activeSection) {
  const draft = reactive({})
  const localErrors = ref({})

  /** 未保存的段数：侧边栏和保存栏都显示它。 */
  const dirtyCount = computed(() => Object.keys(state.dirty).length)

  /** 本表单内的格式错误（JSON 无效等）；与后端返回的 state.fieldErrors 分开存。 */
  const hasLocalErrors = computed(() => Object.keys(localErrors.value).length > 0)

  function applyDraft(source) {
    for (const key of Object.keys(draft)) delete draft[key]
    for (const [name, value] of Object.entries(source || {})) draft[name] = deepClone(value)
    for (const section of sections.value) {
      if (!(section.name in draft)) draft[section.name] = {}
    }
  }

  /** 离开页面时缓存草稿：只有版本号一致、且确实有脏标记时才复用。 */
  function snapshotDraft() {
    const out = {}
    for (const [name, value] of Object.entries(draft)) out[name] = deepClone(value)
    return out
  }

  function ensureSectionObject(name) {
    if (!draft[name] || typeof draft[name] !== 'object') draft[name] = {}
    return draft[name]
  }

  /**
   * 读字段当前值：草稿优先，其次 schema 默认值，最后 null。
   * 组件里的输入框永远读这里，保证「未改过的字段」也显示后端默认值。
   */
  function getFieldValue(field) {
    const section = activeSection.value
    if (!section) return null
    const value = fieldSegments(field).length
      ? nestedGet(draft[section.name], fieldSegments(field).join('.'))
      : undefined
    if (value !== undefined) return value
    if (field.default !== undefined) return field.default
    return null
  }

  function setFieldValue(field, value) {
    const section = activeSection.value
    if (!section) return
    const target = ensureSectionObject(section.name)
    setNested(target, fieldSegments(field), value)
    markDirty(section.name)
    clearLocalError(field.path)
    // 后端上一次返回的字段错误已过期：用户一改就撤掉，避免错误提示一直挂着。
    if (state.fieldErrors[field.path]) {
      const next = { ...state.fieldErrors }
      delete next[field.path]
      state.fieldErrors = next
    }
  }

  function setInvalid(field, message) {
    const next = { ...localErrors.value }
    if (message) next[field.path] = message
    else delete next[field.path]
    localErrors.value = next
  }

  function clearLocalError(path) {
    if (!localErrors.value[path]) return
    const next = { ...localErrors.value }
    delete next[path]
    localErrors.value = next
  }

  function errorFor(field) {
    return state.fieldErrors[field.path] || localErrors.value[field.path] || ''
  }

  function clearErrors() {
    state.fieldErrors = {}
    localErrors.value = {}
  }

  /**
   * 单字段恢复默认：只写草稿并标脏，必须再点保存才生效。
   *
   * secret 字段没有可下发的默认值（后端不会把密钥明文放进 schema），「恢复默认」的
   * 真实语义是清除已保存的密钥：后端只认 `{"__clear__": true}`，写空串 / null
   * 都会被当成「保留旧值」，看起来恢复了其实没生效。标记常量只从 configSchema 取，
   * 不要在这里自创。
   */
  function resetField(field) {
    const section = activeSection.value
    if (!section) return false
    const secret = isSecretField(field)
    if (!secret && !hasFieldDefault(field)) return false
    const target = ensureSectionObject(section.name)
    setNested(target, fieldSegments(field), secret ? secretClearValue() : deepClone(field.default))
    markDirty(section.name)
    clearLocalError(field.path)
    return true
  }

  /**
   * 整段按 schema 默认值重建（仍只写草稿）。
   * 跳过三类字段：bootstrap（config.yaml 提供）、secret（整段重建不能顺手清空已保存密钥，
   * 清密钥只能走单字段的「清除 / 恢复默认」）、父级对象本身是 null 的子字段
   * （父对象整体关闭时不重建子项）。
   */
  function restoreSectionDefaults(fields) {
    const section = activeSection.value
    if (!section) return
    const rebuilt = {}
    for (const field of fields || section.fields || []) {
      if (field.source === 'bootstrap') continue
      if (fieldSegments(field).length === 0) continue
      const type = field.kind ? field.kind.type : 'string'
      if (type === 'secret') continue
      const segments = fieldSegments(field)
      if (segments.length > 1) {
        const parentPath = `${section.name}.${segments.slice(0, -1).join('.')}`
        const parentField = (section.fields || []).find((item) => item.path === parentPath)
        if (parentField && parentField.default == null) continue
        const parentValue = nestedGet(rebuilt, segments.slice(0, -1).join('.'))
        if (parentValue === null) continue
      }
      setNested(rebuilt, segments, field.default == null ? null : deepClone(field.default))
    }
    draft[section.name] = rebuilt
    markDirty(section.name)
    localErrors.value = {}
  }

  /**
   * 放弃全部修改：脏标记、两类错误、草稿一起回到服务端最新值。
   * 注意 clearDirty() 清的是整个 state.dirty；配置中心挂载期间 state.dirty 只包含
   * 本页的脏段（见 useConfigForm 的 adoptDirtyScope / releaseDirtyScope），所以这里
   * 不会误伤别的页面。
   */
  function discardToConfig(config) {
    clearDirty()
    clearErrors()
    applyDraft(config ? config.sections : {})
  }

  return {
    draft,
    localErrors,
    dirtyCount,
    hasLocalErrors,
    applyDraft,
    snapshotDraft,
    getFieldValue,
    setFieldValue,
    setInvalid,
    clearLocalError,
    errorFor,
    clearErrors,
    resetField,
    restoreSectionDefaults,
    discardToConfig,
  }
}
