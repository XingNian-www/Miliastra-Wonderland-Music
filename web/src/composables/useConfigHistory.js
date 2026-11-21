// 配置中心：历史版本面板。
//
// `/config/revisions` 列表 + `/config/rollback` 回滚。
// 回滚会丢弃未保存草稿并产生一个新版本，所以必须先二次确认；成功后重新载入配置。

import { computed, ref } from 'vue'
import { getJson, requestJson } from '../api.js'
import { clearDirty, handleAuth, loadConfig, state, toast } from '../store.js'
import { formatNow } from '../util/configSchema.js'

/**
 * @param panelRef 指向 ConfigRevisionPanel 的模板 ref（暴露 open() / isOpen()）。
 */
export function useConfigHistory({ panelRef, busy, applyDraft, clearErrors, onRolledBack }) {
  const revisions = ref([])
  const revisionsLoading = ref(false)
  const revisionsError = ref('')

  async function loadRevisions() {
    revisionsLoading.value = true
    revisionsError.value = ''
    try {
      const data = await getJson('/config/revisions')
      revisions.value = data && Array.isArray(data.revisions) ? data.revisions : []
    } catch (error) {
      if (!handleAuth(error)) revisionsError.value = `读取历史版本失败：${error.message || error}`
    } finally {
      revisionsLoading.value = false
    }
  }

  function onHistoryToggle(event) {
    const open = event && event.target ? event.target.open : isPanelOpen()
    if (open) loadRevisions()
  }

  function isPanelOpen() {
    const panel = panelRef && panelRef.value
    return Boolean(panel && typeof panel.isOpen === 'function' && panel.isOpen())
  }

  /** 「历史版本」按钮：展开面板并滚到底部（列表随之懒加载）。 */
  function openHistory() {
    const panel = panelRef && panelRef.value
    if (!panel || typeof panel.open !== 'function') return
    panel.open()
  }

  async function rollback(item) {
    if (busy.value || !item) return
    const target = Number(item.revision)
    if (!Number.isFinite(target)) return
    if (!window.confirm(`回滚到版本 #${target}？当前未保存的修改会丢弃，回滚会记录为新版本。`)) return
    try {
      const response = await requestJson('/config/rollback', {
        revision: target,
        baseRevision: state.revision != null ? state.revision : 0,
      })
      if (response && response.ok === false) {
        toast(response.message || '回滚失败', 'err')
        return
      }
      clearDirty()
      clearErrors()
      await loadConfig(true)
      applyDraft(state.config ? state.config.sections : {})
      if (onRolledBack) onRolledBack(formatNow())
      toast(
        `已回滚到版本 #${target}，当前版本 ${response && response.revision != null ? response.revision : '—'}`,
        'ok',
      )
      await loadRevisions()
    } catch (error) {
      if (!handleAuth(error)) toast(`回滚失败：${error.message || error}`, 'err')
    }
  }

  /** 当前版本行要标「当前」并禁用回滚按钮。 */
  const currentRevision = computed(() => (state.revision == null ? null : Number(state.revision)))

  return {
    revisions,
    revisionsLoading,
    revisionsError,
    currentRevision,
    loadRevisions,
    onHistoryToggle,
    isPanelOpen,
    openHistory,
    rollback,
  }
}
