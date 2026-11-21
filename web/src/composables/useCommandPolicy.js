// 命令面板：权限与默认平台草稿。
//
// 只有「与后端原值不同」的条目才留在草稿里，因此草稿条数天然等于「未保存条数」。
// 保存走 /commands/policy，成功后清空草稿并以服务端返回的 revision 重新读取目录。

import { computed, ref } from 'vue'
import { requestJson } from '../api.js'
import { handleAuth, toast } from '../store.js'

export function useCommandPolicy({ catalog, loadCatalog }) {
  const policyDraft = ref({})
  const defaultSourceDraft = ref(null)
  const policyError = ref('')
  const policyNotice = ref('')
  const savingPolicy = ref(false)

  /** 未保存的权限条数 = 改过的命令数 + 默认平台是否改过（1 条）。 */
  const policyDirtyCount = computed(
    () => Object.keys(policyDraft.value).length + (defaultSourceDraft.value !== null ? 1 : 0),
  )

  const hasPolicyDraft = computed(() => policyDirtyCount.value > 0)

  const defaultSourceValue = computed(
    () => defaultSourceDraft.value ?? catalog.value?.defaultSongSource ?? '',
  )

  const unsupportedDefaultSource = computed(() => {
    const stored = catalog.value?.defaultSongSource
    if (!stored || defaultSourceDraft.value !== null) return ''
    const options = catalog.value?.songPlatforms || []
    return catalog.value?.defaultSongSourceSelectable === false && !options.some((item) => item.value === stored)
      ? String(stored)
      : ''
  })

  const defaultSourceHint = computed(() => {
    if (defaultSourceDraft.value !== null) return '默认平台未保存'
    if (unsupportedDefaultSource.value) {
      return `当前默认平台「${platformLabel(unsupportedDefaultSource.value)}」已不能作为 @点歌 的默认平台，请改选后保存`
    }
    return ''
  })

  function platformLabel(value) {
    const option = (catalog.value?.songPlatforms || []).find((item) => item.value === value)
    return option ? option.label : String(value || '')
  }

  function policyValue(item) {
    return policyDraft.value[String(item.id)] || {
      enabled: item.enabled !== false,
      minimumRole: item.minimumRole || 'user',
    }
  }

  function isPolicyDirty(item) {
    return Object.prototype.hasOwnProperty.call(policyDraft.value, String(item.id))
  }

  /** 写入一条权限草稿；改回原值时自动从草稿里删掉，条数才准确。 */
  function setPolicyField(item, patch) {
    if (!item || item.editable !== true || savingPolicy.value) return
    const id = String(item.id)
    const current = {
      enabled: policyValue(item).enabled,
      minimumRole: policyValue(item).minimumRole,
    }
    const next = { ...current, ...patch }
    const original = {
      enabled: item.enabled !== false,
      minimumRole: item.minimumRole || 'user',
    }
    const drafts = { ...policyDraft.value }
    if (next.enabled === original.enabled && next.minimumRole === original.minimumRole) {
      delete drafts[id]
    } else {
      drafts[id] = { id, enabled: next.enabled, minimumRole: next.minimumRole }
    }
    policyDraft.value = drafts
    policyNotice.value = ''
  }

  function onPolicyEnabled(item, event) {
    setPolicyField(item, { enabled: Boolean(event.target.checked) })
  }

  function onPolicyRole(item, event) {
    setPolicyField(item, { minimumRole: String(event.target.value) })
  }

  function onDefaultSourceChange(event) {
    const value = String(event.target.value || '')
    defaultSourceDraft.value = value === (catalog.value?.defaultSongSource ?? '') ? null : value
    policyNotice.value = ''
  }

  function resetPolicy() {
    policyDraft.value = {}
    defaultSourceDraft.value = null
    policyError.value = ''
  }

  function discardPolicy() {
    if (!policyDirtyCount.value || savingPolicy.value) return
    if (!window.confirm('放弃未保存的命令权限/平台修改并重新读取？')) return
    resetPolicy()
    policyNotice.value = ''
    loadCatalog(true)
  }

  async function savePolicy() {
    if (!policyDirtyCount.value || savingPolicy.value) return false
    savingPolicy.value = true
    policyError.value = ''
    policyNotice.value = ''
    try {
      const payload = {
        baseRevision: catalog.value?.revision ?? 0,
        items: Object.values(policyDraft.value).map((item) => ({
          id: item.id,
          enabled: item.enabled !== false,
          minimumRole: item.minimumRole,
        })),
      }
      if (defaultSourceDraft.value !== null) payload.defaultSongSource = defaultSourceDraft.value
      const result = await requestJson('/commands/policy', payload)
      if (!result || result.ok !== true) {
        throw new Error(result?.message || '命令设置保存失败')
      }
      policyDraft.value = {}
      defaultSourceDraft.value = null
      catalog.value = catalog.value ? { ...catalog.value, revision: result.revision } : catalog.value
      await loadCatalog(true)
      policyNotice.value = `命令设置已保存，版本 ${result.revision}`
      toast(`命令设置已保存，版本 ${result.revision}`, 'ok')
      return true
    } catch (error) {
      if (handleAuth(error)) return false
      policyError.value = String(error.message || '').includes('配置已变化')
        ? '配置版本已变化，草稿已保留。请放弃草稿重新读取后再编辑保存。'
        : `保存失败：${error.message || error}`
      return false
    } finally {
      savingPolicy.value = false
    }
  }

  return {
    policyDraft,
    defaultSourceDraft,
    policyError,
    policyNotice,
    savingPolicy,
    policyDirtyCount,
    hasPolicyDraft,
    defaultSourceValue,
    unsupportedDefaultSource,
    defaultSourceHint,
    platformLabel,
    policyValue,
    isPolicyDirty,
    setPolicyField,
    onPolicyEnabled,
    onPolicyRole,
    onDefaultSourceChange,
    resetPolicy,
    discardPolicy,
    savePolicy,
  }
}
