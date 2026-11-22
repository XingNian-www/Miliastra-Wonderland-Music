// 配置中心：页面级编排。
//
// 把「段导航 / 草稿 / 保存 / 历史版本」四个 composable 串起来，页面只保留模板。
// 页面与子组件之间只传值 + 事件，不共享内部实现。
//
// 脏标记与草稿按页面（scope）分区：
//   - 全局 state.dirty 是共享的，AI 配置页保存后会调 clearDirty()（清空整个对象）。
//     所以配置中心只在**挂载期间**把本页脏段放进 state.dirty：进来时先把别的页面留下的
//     脏标记收进 foreignDirty、换成本页的 configDirty 快照；离开时把本页脏段收回
//     configDirty、再把 foreignDirty 原样放回。这样别的页面清脏标记时不会抹掉本页修改，
//     回来时本页的未保存状态也还在。
//   - 草稿缓存在 draftCache 里按 scope 分开存（Map），不同页面各存各的。

import { computed, ref, onBeforeUnmount, onMounted } from 'vue'
import { handleAuth, isDirty, loadConfig, state, toast } from '../store.js'
import { friendlyError } from '../util/format.js'
import { isSecretField } from '../util/configSchema.js'
import { navigateTo, registerDirtySource } from '../router.js'
import { useConfigSections } from './useConfigSections.js'
import { useConfigDraft } from './useConfigDraft.js'
import { useConfigSave } from './useConfigSave.js'
import { useConfigHistory } from './useConfigHistory.js'

/** 切到别的页面时缓存草稿：key 是页面 scope，同一 scope 只保留最后一份。 */
const draftCache = new Map()
/** 配置中心自己的脏段；离开期间从全局 state.dirty 摘出来暂存。 */
const configDirty = new Set()
/** 配置中心挂载前，state.dirty 里属于别的页面的脏标记；离开时原样放回。 */
let foreignDirty = null
/** 默认 scope：现有调用方 `useConfigForm()` 不传参，等价于旧行为。 */
const DEFAULT_SCOPE = 'config'

export function useConfigForm(scope = DEFAULT_SCOPE) {
  const pageScope = String(scope || DEFAULT_SCOPE)
  const loading = ref(!state.config)
  const loadError = ref('')

  const historyPanelRef = ref(null)

  // 站内切页守卫 / 全局 beforeunload 通过它判断「配置中心有未保存修改」。
  // blocksNavigation=false：本页草稿在切页时会被 draftCache 原样恢复，用户不会真的丢改动，
  // 再弹一次「离开后可能丢失」就是假警告；刷新/关标签仍会被全局 beforeunload 拦下。
  // 卸载时注销：离开的那次确认发生在卸载之前，能看到本页。
  const unregisterDirty = registerDirtySource(`config:${pageScope}`, {
    label: '配置中心',
    blocksNavigation: false,
    dirty: () => Object.keys(state.dirty).length,
  })

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
    // secret 没有可下发的默认值，「恢复默认」的真实语义是清除已保存的密钥
    // （草稿层写的是 {"__clear__": true}）。文案与 AI 页统一，见审计 O1。
    if (isSecretField(field)) {
      if (!window.confirm(`清除「${field.label}」已保存的密钥（尚未保存）？`)) return
      if (form.resetField(field)) toast(`已标记清除「${field.label}」，保存后生效`, 'ok')
      return
    }
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
  /** 本页 schema 里的段名：用来把「自己的脏段」和「别的页面的脏标记」分开。 */
  function ownSectionNames() {
    return new Set((nav.sections.value || []).map((section) => section.name))
  }

  /**
   * 接管全局脏标记：把别的页面留下的脏标记先收起来，换成本页离开前存的快照。
   * 必须在任何 isDirty() / markDirty() 之前执行（即 loadConfig 之前）。
   */
  function adoptDirtyScope() {
    const ownNames = ownSectionNames()
    const own = {}
    const foreign = {}
    for (const [name, flag] of Object.entries(state.dirty)) {
      if (ownNames.size && !ownNames.has(name)) foreign[name] = flag
      else own[name] = flag
    }
    foreignDirty = foreign
    for (const name of configDirty) own[name] = true
    state.dirty = own
  }

  /** 交还全局脏标记：本页脏段收回 configDirty，别的页面原样放回。 */
  function releaseDirtyScope() {
    const ownNames = ownSectionNames()
    const current = { ...state.dirty }
    configDirty.clear()
    const next = { ...(foreignDirty || {}) }
    for (const [name, flag] of Object.entries(current)) {
      // 挂载期间写入的脏标记：属于自己的段收进快照，不属于的（别的页面先挂载时写的）保留。
      if (!ownNames.size || ownNames.has(name)) configDirty.add(name)
      else next[name] = flag
    }
    state.dirty = next
    foreignDirty = null
  }

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
    adoptDirtyScope()
    window.addEventListener('keydown', onKeydown)
    window.addEventListener('beforeunload', onBeforeUnload)
    try {
      await loadConfig()
      const cached = draftCache.get(pageScope)
      // 版本号不再是「丢弃草稿」的条件：别的页面保存只说明基线变了，
      // 用户在本页没保存的修改仍然优先（回来时提示一次，让他自己决定保存还是放弃）。
      const canReuse = !!(cached && cached.draft && isDirty())
      if (canReuse) {
        form.applyDraft(cached.draft)
        if (cached.revision != null && cached.revision !== state.revision) {
          toast('其他页面已保存新的配置版本；你未保存的修改已保留，请检查后保存', '')
        }
      } else {
        // 没有对应脏段的旧草稿已被保存/放弃，丢掉避免下次误复用。
        draftCache.delete(pageScope)
        if (isDirty()) {
          // 有脏标记却没有草稿（例如热更新后模块状态丢失）：按服务端值重建，避免保存空改动。
          form.discardToConfig(state.config)
        } else {
          form.applyDraft(state.config ? state.config.sections : {})
        }
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
    unregisterDirty()
    draftCache.set(pageScope, { revision: state.revision, draft: form.snapshotDraft() })
    releaseDirtyScope()
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
