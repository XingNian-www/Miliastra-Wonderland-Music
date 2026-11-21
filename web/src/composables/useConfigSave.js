// 配置中心：保存 / 重新载入 / 放弃修改。
//
// 保存只提交「当前页可见且脏」的段，整段先过一遍本地规整（coerce），
// 再走 store.saveSections 的 /config/validate → /config/save 两段式流程。
// 这里只负责编排，具体网络与字段错误落库在 store.js。

import { computed, ref } from 'vue'
import {
  clearDirty,
  handleAuth,
  isDirty,
  loadConfig,
  saveSections,
  state,
  toast,
} from '../store.js'
import { formatNow, normalizeSection, saveSummary } from '../util/configSchema.js'

export function useConfigSave({ sections, draft, localErrors, hasLocalErrors, applyDraft, clearErrors, revealField }) {
  const saving = ref(false)
  const savedAt = ref('')

  const busy = computed(() => state.busy || saving.value)

  /**
   * 组装提交载荷：只为脏段生成对象，并顺带做一次本地校验。
   * 有格式错误时：记录到 localErrors、跳到第一个出错字段、返回 null。
   */
  function buildDirtySections() {
    const names = Object.keys(state.dirty).filter((name) =>
      sections.value.some((section) => section.name === name),
    )
    if (!names.length) {
      toast('当前页没有需要保存的配置段', 'err')
      return null
    }
    const errors = {}
    const payload = {}
    for (const name of names) {
      const section = sections.value.find((item) => item.name === name)
      if (!section) continue
      payload[name] = normalizeSection(section, draft[name], errors)
    }
    if (Object.keys(errors).length) {
      localErrors.value = errors
      const firstPath = Object.keys(errors)[0]
      toast(`有 ${Object.keys(errors).length} 个字段格式无效，请修正后再保存`, 'err')
      revealField(firstPath)
      return null
    }
    return payload
  }

  async function save() {
    if (saving.value || state.busy) return
    if (hasLocalErrors.value) {
      const firstPath = Object.keys(localErrors.value)[0]
      toast('有字段格式无效（如 JSON），请先修正再保存', 'err')
      revealField(firstPath)
      return
    }
    const payload = buildDirtySections()
    if (!payload) return
    saving.value = true
    let response = null
    try {
      const ok = await saveSections(payload, (result) => {
        response = result
      })
      if (!ok) {
        // 服务端校验失败：若错误字段不在当前段，自动跳到出错字段所在段。
        const firstError = Object.keys(state.fieldErrors)[0]
        if (firstError) revealField(firstError)
        return
      }
      clearDirty()
      clearErrors()
      savedAt.value = formatNow()
      toast(saveSummary(response, state.revision), 'ok')
      try {
        await loadConfig(true)
        applyDraft(state.config ? state.config.sections : {})
      } catch (error) {
        toast(`配置已保存，但重新读取失败：${error.message || error}`, 'err')
      }
      return response
    } finally {
      saving.value = false
    }
  }

  async function reload() {
    if (busy.value) return
    if (isDirty() && !window.confirm('重新载入会丢弃全部未保存的修改，继续？')) return
    try {
      await loadConfig(true)
      clearDirty()
      clearErrors()
      applyDraft(state.config ? state.config.sections : {})
      toast('已重新载入配置')
    } catch (error) {
      if (!handleAuth(error)) toast(`重新载入失败：${error.message || error}`, 'err')
    }
  }

  function discardChanges() {
    if (!Object.keys(state.dirty).length) return
    if (!window.confirm('放弃当前未保存的修改？')) return
    clearDirty()
    clearErrors()
    applyDraft(state.config ? state.config.sections : {})
    toast('已放弃未保存的修改')
  }

  return {
    saving,
    savedAt,
    busy,
    buildDirtySections,
    save,
    reload,
    discardChanges,
  }
}
