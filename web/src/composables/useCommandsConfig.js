// 命令面板：页面级编排。
//
// 三个 composable 之间存在双向依赖：
//   - 目录需要回复目录里的别名表来归一化选中 id；
//   - 回复需要目录计算出的「当前选中命令」。
// 解法是页面持有两个共享 ref（repliesRef / hasReplyDraft），先建 ref、再注入，
// 两个方向的 watch 只做「镜像同步」，不产生额外业务判断。
//
// 页面拿到的是扁平的方法与派生值，模板不出现任何 composable 内部结构。

import { computed, onMounted, ref, watch } from 'vue'
import { toast } from '../store.js'
import { ROLES, ROLE_LABELS, itemKey } from '../util/commandCatalog.js'
import { useCommandCatalog } from './useCommandCatalog.js'
import { useCommandPolicy } from './useCommandPolicy.js'
import { useCommandReplies } from './useCommandReplies.js'

export function useCommandsConfig() {
  // 目录 composable 需要读取回复目录（aliases / commands），回复 composable 才是它的所有者。
  const repliesRef = ref(null)
  // 目录加载时要判断「有回复草稿就别强制刷新」，同样先建引用。
  const replyDirtyRef = ref(false)
  // 回复加载时要判断「有权限草稿就别强制刷新」。
  const policyDirtyRef = ref(false)

  const catalogApi = useCommandCatalog({ repliesRef, hasPolicyDraft: policyDirtyRef })
  const policyApi = useCommandPolicy({ catalog: catalogApi.catalog, loadCatalog: catalogApi.loadCatalog })
  const repliesApi = useCommandReplies({
    selectedEntry: catalogApi.selectedEntry,
    ensureSelection: catalogApi.ensureSelection,
    hasReplyDraft: replyDirtyRef,
  })

  watch(
    repliesApi.replies,
    (value) => {
      repliesRef.value = value
    },
    { immediate: true },
  )
  watch(
    repliesApi.replyDirtyCount,
    (value) => {
      replyDirtyRef.value = value > 0
    },
    { immediate: true },
  )
  watch(
    policyApi.policyDirtyCount,
    (value) => {
      policyDirtyRef.value = value > 0
    },
    { immediate: true },
  )

  const saving = computed(() => policyApi.savingPolicy.value || repliesApi.savingReplies.value)
  const policyNotice = computed(() => policyApi.policyNotice.value || repliesApi.replyNotice.value)

  /* ---------- 刷新类按钮：有草稿就拒绝，避免用户无声丢改动 ---------- */
  async function refreshCatalog() {
    if (policyApi.policyDirtyCount.value > 0) {
      toast('有未保存的权限/平台草稿，请先保存或放弃', 'err')
      return
    }
    await catalogApi.loadCatalog(true)
  }

  async function refreshReplies() {
    if (repliesApi.replyDirtyCount.value > 0) {
      toast('有未保存的回复草稿，请先保存或放弃', 'err')
      return
    }
    await repliesApi.loadReplies(true)
  }

  /* ---------- 权限控件的派生：选项禁用条件依赖「保存中」状态 ---------- */
  function roleOptions(item) {
    const floorIndex = Math.max(0, ROLES.indexOf(item.minimumFloor || 'user'))
    return ROLES.map((value, index) => ({
      value,
      label: ROLE_LABELS[value],
      disabled: saving.value || index < floorIndex,
    }))
  }

  onMounted(() => {
    catalogApi.loadCatalog()
    repliesApi.loadReplies()
  })

  return {
    // 目录与筛选
    catalog: catalogApi.catalog,
    catalogLoading: catalogApi.catalogLoading,
    catalogError: catalogApi.catalogError,
    commandsEnabledLabel: catalogApi.commandsEnabledLabel,
    catalogState: catalogApi.catalogState,
    catalogMeta: catalogApi.catalogMeta,
    categories: catalogApi.categories,
    visibleGroups: catalogApi.visibleGroups,
    visibleCount: catalogApi.visibleCount,
    totalCount: catalogApi.totalCount,
    selectedEntry: catalogApi.selectedEntry,
    selectedVisible: catalogApi.selectedVisible,
    fetchedAt: catalogApi.fetchedAt,
    searchText: catalogApi.searchText,
    sourceFilter: catalogApi.sourceFilter,
    showHidden: catalogApi.showHidden,
    activeCategory: catalogApi.activeCategory,
    collapsed: catalogApi.collapsed,
    expanded: catalogApi.expanded,
    isSelected: catalogApi.isSelected,
    selectCommand: catalogApi.selectCommand,
    toggleGroup: catalogApi.toggleGroup,
    toggleDetail: catalogApi.toggleDetail,
    itemKey,
    refreshCatalog,
    // 权限与默认平台
    savingPolicy: policyApi.savingPolicy,
    policyDirtyCount: policyApi.policyDirtyCount,
    policyError: policyApi.policyError,
    defaultSourceValue: policyApi.defaultSourceValue,
    unsupportedDefaultSource: policyApi.unsupportedDefaultSource,
    defaultSourceHint: policyApi.defaultSourceHint,
    platformLabel: policyApi.platformLabel,
    policyValue: policyApi.policyValue,
    isPolicyDirty: policyApi.isPolicyDirty,
    roleOptions,
    onPolicyEnabled: policyApi.onPolicyEnabled,
    onPolicyRole: policyApi.onPolicyRole,
    onDefaultSourceChange: policyApi.onDefaultSourceChange,
    savePolicy: policyApi.savePolicy,
    discardPolicy: policyApi.discardPolicy,
    // 回复
    replies: repliesApi.replies,
    replyLoading: repliesApi.replyLoading,
    replyError: repliesApi.replyError,
    replyRows: repliesApi.replyRows,
    replyVars: repliesApi.replyVars,
    replyStatus: repliesApi.replyStatus,
    maxReplyChars: repliesApi.maxReplyChars,
    replyDirtyCount: repliesApi.replyDirtyCount,
    restorableReplyCount: repliesApi.restorableReplyCount,
    savingReplies: repliesApi.savingReplies,
    lastFocusedRow: repliesApi.lastFocusedRow,
    setReplyInput: repliesApi.setReplyInput,
    onReplyInput: repliesApi.onReplyInput,
    setRowActive: repliesApi.setRowActive,
    moveRow: repliesApi.moveRow,
    removeRow: repliesApi.removeRow,
    addRow: repliesApi.addRow,
    restoreDefaults: repliesApi.restoreDefaults,
    restoreAllReplyDefaults: repliesApi.restoreAllReplyDefaults,
    muteCommand: repliesApi.muteCommand,
    insertVariable: repliesApi.insertVariable,
    saveReplies: repliesApi.saveReplies,
    discardReplies: repliesApi.discardReplies,
    refreshReplies,
    // 通用
    saving,
    policyNotice,
  }
}
