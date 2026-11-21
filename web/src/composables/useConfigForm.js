// 配置中心：页面级编排。
//
// 把「段导航 / 草稿 / 保存 / 历史版本」四个 composable 串起来，页面只保留模板。
// 页面与子组件之间只传值 + 事件，不共享内部实现。

import { computed, ref, onBeforeUnmount, onMounted } from 'vue'
import { handleAuth, isDirty, loadConfig, state, toast } from '../store.js'
import { friendlyError } from '../util/format.js'
import { navigateTo } from '../router.js'
import { useConfigSections } from './useConfigSections.js'
import { useConfigDraft } from './useConfigDraft.js'
import { useConfigSave } from './useConfigSave.js'
import { useConfigHistory } from './useConfigHistory.js'

// 切到别的页面时缓存草稿：只有版本号一致、且确实有脏标记时才复用。
// 模块级变量即可——同一时刻只有一个配置中心实例。
let draftCache = { revision: null, draft: null }

export function useConfigForm() {
  const loading = ref(!state.config)
  const loadError = ref('')

  const historyPanelRef = ref(null)

  const nav = useConfigSections()
  const form = useConfigDraft(nav.sections, nav.activeSection)
  const saver = useConfigSave({
    sections: nav.sections,
    draft: form.draft,
    localErrors: form.localErrors,
    hasLocalErrors: form.hasLocalErrors,
    applyDraft: form.applyDraft,
    clearErrors: form.clearErrors,
    revealField: nav.revealField,
  })
  const history = useConfigHistory({
    panelRef: historyPanelRef,
    busy: saver.busy,
    applyDraft: form.applyDraft,
    clearErrors: form.clearErrors,
    onRolledBack: (stamp) => {
      saver.savedAt.value = stamp
    },
  })

  /* ---------- 当前段的派生展示 ---------- */
  /**
   * 视觉档案提示条是否显示。
   *
   * 注意：旧页面写的是 `v-if="activeSection === 'visual'"`，而 activeSection 是
   * 对象（computed 返回 section 实例），与字符串比较**恒为 false**——这条提示条
   * 在旧版本里从来没显示过。这里原样保留该结果（行为等价优先），
   * 如需真正显示请改成 `activeSection.value?.name === 'visual'` 并同步调整回归用例。
   */
  const showVisualHint = computed(() => nav.activeSection.value === 'visual')

  const isEmojiSection = computed(() => nav.activeSection.value?.name === 'emoji')

  const emojiAllowedField = computed(() => {
    const section = nav.activeSection.value
    if (!section || section.name !== 'emoji') return null
    return (section.fields || []).find((field) => field.path === 'emoji.ai_allowed') || null
  })

  const emojiAllowed = computed(() => {
    const field = emojiAllowedField.value
    if (!field) return []
    const value = form.getFieldValue(field)
    return Array.isArray(value) ? value : []
  })

  function onEmojiAllowed(list) {
    const field = emojiAllowedField.value
    if (!field) return
    form.setFieldValue(field, list)
  }

  /* ---------- 字段状态 ---------- */
  function isDisabled(field) {
    return saver.busy.value || field.source === 'bootstrap'
  }

  /* ---------- 事件包装：确认框与 toast 留在这一层，子组件保持纯展示 ---------- */
  function resetField(field) {
    if (!window.confirm(`将「${field.label}」恢复为默认值（尚未保存）？`)) return
    if (form.resetField(field)) toast(`已将「${field.label}」恢复为默认值，记得保存`, 'ok')
  }

  function restoreSectionDefaults() {
    const section = nav.activeSection.value
    if (!section) return
    if (!window.confirm(`将「${section.label}」整段按 schema 默认值重建（尚未保存）？`)) return
    form.restoreSectionDefaults(section.fields)
    toast(`已按默认值重建「${section.label}」，请检查后保存`, 'ok')
  }

  async function save() {
    const response = await saver.save()
    // 保存成功后如果历史面板正开着，顺带刷新列表（版本号已变）。
    if (response && history.isPanelOpen()) await history.loadRevisions()
  }

  function openVisual() {
    navigateTo('visual')
  }

  /* ---------- 生命周期 ---------- */
  function onKeydown(event) {
    if ((event.ctrlKey || event.metaKey) && String(event.key).toLowerCase() === 's') {
      event.preventDefault()
      save()
    }
  }

  function onBeforeUnload(event) {
    if (!isDirty()) return
    event.preventDefault()
    event.returnValue = ''
  }

  onMounted(async () => {
    window.addEventListener('keydown', onKeydown)
    window.addEventListener('beforeunload', onBeforeUnload)
    try {
      await loadConfig()
      const cached = draftCache
      const canReuse = !!(cached && cached.draft && isDirty() && cached.revision === state.revision)
      if (canReuse) {
        form.applyDraft(cached.draft)
      } else {
        // 其他页面已保存过新版本：旧草稿失效，同步清掉脏标记避免误报「未保存」。
        if (cached && cached.revision != null && cached.revision !== state.revision) form.discardToConfig(state.config)
        form.applyDraft(state.config ? state.config.sections : {})
      }
      if (
        !nav.activeSectionName.value ||
        !nav.visibleSections.value.some((section) => section.name === nav.activeSectionName.value)
      ) {
        nav.selectInitialSection()
      }
    } catch (error) {
      if (!handleAuth(error)) loadError.value = `载入配置失败：${friendlyError(error)}`
    } finally {
      loading.value = false
    }
  })

  onBeforeUnmount(() => {
    draftCache = { revision: state.revision, draft: form.snapshotDraft() }
    window.removeEventListener('keydown', onKeydown)
    window.removeEventListener('beforeunload', onBeforeUnload)
  })

  return {
    loading,
    loadError,
    historyPanelRef,
    // 导航
    searchText: nav.searchText,
    searchQuery: nav.searchQuery,
    activeSectionName: nav.activeSectionName,
    sections: nav.sections,
    activeSection: nav.activeSection,
    visibleFields: nav.visibleFields,
    groups: nav.groups,
    domains: nav.domains,
    matchedFieldCount: nav.matchedFieldCount,
    effectSummary: nav.effectSummary,
    selectSection: nav.selectSection,
    toggleDomain: nav.toggleDomain,
    highlight: nav.highlight,
    // 草稿
    dirtyCount: form.dirtyCount,
    hasLocalErrors: form.hasLocalErrors,
    getFieldValue: form.getFieldValue,
    setFieldValue: form.setFieldValue,
    setInvalid: form.setInvalid,
    errorFor: form.errorFor,
    isDisabled,
    resetField,
    restoreSectionDefaults,
    // 保存
    saving: saver.saving,
    savedAt: saver.savedAt,
    busy: saver.busy,
    save,
    reload: saver.reload,
    discardChanges: saver.discardChanges,
    // 历史版本
    revisions: history.revisions,
    revisionsLoading: history.revisionsLoading,
    revisionsError: history.revisionsError,
    currentRevision: history.currentRevision,
    onHistoryToggle: history.onHistoryToggle,
    openHistory: history.openHistory,
    rollback: history.rollback,
    // 当前段的派生展示
    showVisualHint,
    isEmojiSection,
    emojiAllowed,
    emojiAllowedField,
    onEmojiAllowed,
    // 其它
    openVisual,
  }
}
