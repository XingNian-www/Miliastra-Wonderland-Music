// 命令面板：页面级编排。
//
// 三个 composable 之间存在双向依赖：
//   - 目录需要回复目录里的别名表来归一化选中 id；
//   - 回复需要目录计算出的「当前选中命令」。
// 解法是页面持有两个共享 ref（repliesRef / hasReplyDraft），先建 ref、再注入，
// 两个方向的 watch 只做「镜像同步」，不产生额外业务判断。
//
// 页面拿到的是扁平的方法与派生值，模板不出现任何 composable 内部结构。
//
// 草稿持久化：权限/平台与回复草稿都只在内存里，切一次路由就全丢（刷新更是必丢）。
// 这里把草稿按「命令 + 版本基线」写进 sessionStorage，页面重新挂载后恢复；
// 保存成功或放弃后（脏条数归零）立刻清掉，避免本机长期留着一份过期草稿。
// 内容只有命令权限、默认平台与回复文案，没有 secret 字段；仍限制体积与时效，
// 超过上限或超过 7 天的缓存直接丢弃，不做静默长期留存。

import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { toast } from '../store.js'
import { registerDirtySource } from '../router.js'
import { ROLES, ROLE_LABELS, itemKey } from '../util/commandCatalog.js'
import { useCommandCatalog } from './useCommandCatalog.js'
import { useCommandPolicy } from './useCommandPolicy.js'
import { useCommandReplies } from './useCommandReplies.js'

/* ---------- 草稿缓存（sessionStorage） ---------- */
const DRAFT_STORAGE_KEY = 'miliastra:commandDrafts:v1'
const DRAFT_STORAGE_VERSION = 1
/** 缓存最长保留 7 天：超过视为过期，不再恢复。 */
const DRAFT_TTL_MS = 7 * 24 * 60 * 60 * 1000
/** 单份草稿的体积上限（字符数，近似字节）；超大草稿宁可不落盘。 */
const DRAFT_MAX_CHARS = 256 * 1024
/** 连续输入时合并写入，避免每个按键都同步写一次 sessionStorage。 */
const DRAFT_WRITE_DELAY_MS = 300

function canUseSessionStorage() {
  try {
    return typeof sessionStorage !== 'undefined' && !!sessionStorage
  } catch (error) {
    return false
  }
}

function clearStoredDrafts() {
  if (!canUseSessionStorage()) return
  try {
    sessionStorage.removeItem(DRAFT_STORAGE_KEY)
  } catch (error) {
    /* 隐私模式/配额异常：只失去本机恢复能力，不影响编辑与保存 */
  }
}

/** 读取本机草稿；版本不符、过期或解析失败都当作没有。 */
function readStoredDrafts() {
  if (!canUseSessionStorage()) return null
  try {
    const raw = sessionStorage.getItem(DRAFT_STORAGE_KEY)
    if (!raw) return null
    const parsed = JSON.parse(raw)
    if (!parsed || typeof parsed !== 'object' || parsed.version !== DRAFT_STORAGE_VERSION) {
      clearStoredDrafts()
      return null
    }
    if (!Number.isFinite(parsed.savedAt) || Date.now() - parsed.savedAt > DRAFT_TTL_MS) {
      clearStoredDrafts()
      return null
    }
    return parsed
  } catch (error) {
    clearStoredDrafts()
    return null
  }
}

function writeStoredDrafts(payload) {
  if (!canUseSessionStorage()) return
  try {
    const text = JSON.stringify(payload)
    if (text.length > DRAFT_MAX_CHARS) return
    sessionStorage.setItem(DRAFT_STORAGE_KEY, text)
  } catch (error) {
    /* 配额不足或隐私模式：放弃本机恢复，不打断当前编辑 */
  }
}

/** 回复草稿只保留形状合法的行：缓存被外部改坏时不能让面板直接崩。 */
function sanitizeReplyDraft(raw) {
  const out = {}
  if (!raw || typeof raw !== 'object') return out
  for (const [id, rows] of Object.entries(raw)) {
    if (!Array.isArray(rows)) continue
    out[String(id)] = rows
      .filter((row) => row && typeof row === 'object')
      .map((row) => ({
        key: String(row.key || ''),
        // label 是界面上的可读名，缓存往返不能丢（丢了只剩 key）。
        ...(row.label == null ? {} : { label: String(row.label) }),
        text: String(row.text == null ? '' : row.text),
        defaultText: row.defaultText == null ? null : String(row.defaultText),
        editable: row.editable !== false,
        active: row.active !== false,
      }))
  }
  return out
}

/** 权限草稿同理：角色必须是已知枚举，id 一律转字符串。 */
function sanitizePolicyDraft(raw) {
  const out = {}
  if (!raw || typeof raw !== 'object') return out
  for (const [id, item] of Object.entries(raw)) {
    const value = item && typeof item === 'object' ? item : {}
    const role = ROLES.includes(String(value.minimumRole)) ? String(value.minimumRole) : 'user'
    out[String(id)] = { id: String(id), enabled: value.enabled !== false, minimumRole: role }
  }
  return out
}

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

  /* ---------- 站内切页保护：命令面板自己也注册一份脏标记 ---------- */
  const unregisterDirty = registerDirtySource('commands', {
    label: '命令面板',
    dirty: () => policyApi.policyDirtyCount.value + repliesApi.replyDirtyCount.value,
  })

  /* ---------- 草稿落盘 / 恢复 ---------- */
  let persistTimer = null

  /** 立刻把当前草稿写进 sessionStorage；没有脏条数时清掉缓存。 */
  function persistDrafts() {
    if (persistTimer != null) {
      clearTimeout(persistTimer)
      persistTimer = null
    }
    const replyCount = repliesApi.replyDirtyCount.value
    const policyCount = policyApi.policyDirtyCount.value
    if (!replyCount && !policyCount) {
      clearStoredDrafts()
      return
    }
    const payload = { version: DRAFT_STORAGE_VERSION, savedAt: Date.now() }
    if (replyCount) {
      payload.replies = {
        baseRevision: repliesApi.replyBaseRevision.value ?? null,
        draft: repliesApi.replyDraft.value,
      }
    }
    if (policyCount) {
      payload.policy = {
        items: policyApi.policyDraft.value,
        defaultSource: policyApi.defaultSourceDraft.value ?? null,
      }
    }
    writeStoredDrafts(payload)
  }

  function schedulePersist() {
    if (persistTimer != null) clearTimeout(persistTimer)
    persistTimer = setTimeout(persistDrafts, DRAFT_WRITE_DELAY_MS)
  }

  watch(
    [repliesApi.replyDraft, policyApi.policyDraft, policyApi.defaultSourceDraft],
    schedulePersist,
    { deep: true },
  )

  /**
   * 已知命令 id 集合：只用来把缓存里已经不存在的命令草稿丢掉。
   * 目录 id、回复目录 id 与别名表都要算进来——回复草稿的键用的是回复目录的 id，
   * 只按目录 id 过滤会把别名命令的有效草稿误删。
   */
  function knownCommandIds() {
    const ids = new Set()
    const groups = Array.isArray(catalogApi.catalog.value?.groups) ? catalogApi.catalog.value.groups : []
    for (const group of groups) {
      for (const item of Array.isArray(group.commands) ? group.commands : []) {
        if (item && item.id != null) ids.add(String(item.id))
      }
    }
    const replyEntries = Array.isArray(repliesRef.value?.commands) ? repliesRef.value.commands : []
    for (const entry of replyEntries) {
      if (entry && entry.id != null) ids.add(String(entry.id))
    }
    const aliases = Array.isArray(repliesRef.value?.aliases) ? repliesRef.value.aliases : []
    for (const alias of aliases) {
      if (!alias) continue
      if (alias.id != null) ids.add(String(alias.id))
      if (alias.owner != null) ids.add(String(alias.owner))
    }
    return ids
  }

  /**
   * 从本机缓存恢复草稿（只在挂载后、两个目录都加载完时调用）。
   * 回复的基线版本改成服务端当前版本：否则保存一定被「回复配置已变化」拒绝；
   * 行内容仍是用户当时没保存的编辑，界面提示里会明确要求核对。
   */
  function restoreDrafts(stored) {
    if (!stored) return false
    const restored = []
    const known = knownCommandIds()
    const keepKnown = (map) =>
      known.size ? Object.fromEntries(Object.entries(map).filter(([id]) => known.has(id))) : map
    const replyDraft = keepKnown(stored.replies ? sanitizeReplyDraft(stored.replies.draft) : {})
    if (Object.keys(replyDraft).length && !repliesApi.replyDirtyCount.value) {
      repliesApi.replyDraft.value = replyDraft
      repliesApi.replyBaseRevision.value = repliesApi.replies.value?.revision ?? null
      repliesApi.replyNotice.value = '已从本机恢复未保存的回复草稿；保存或放弃后会自动清除'
      restored.push('回复')
    }
    const policyItems = keepKnown(stored.policy ? sanitizePolicyDraft(stored.policy.items) : {})
    const defaultSource = stored.policy && typeof stored.policy.defaultSource === 'string'
      ? stored.policy.defaultSource
      : null
    if ((Object.keys(policyItems).length || defaultSource !== null) && !policyApi.policyDirtyCount.value) {
      policyApi.policyDraft.value = policyItems
      policyApi.defaultSourceDraft.value = defaultSource
      policyApi.policyNotice.value = '已从本机恢复未保存的权限/平台草稿；保存或放弃后会自动清除'
      restored.push('权限/平台')
    }
    if (!restored.length) {
      clearStoredDrafts()
      return false
    }
    toast(`已恢复 ${restored.join('与')}草稿（尚未保存）`)
    return true
  }

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

  /* ---------- 保存：成功后顺手清掉本机草稿 ---------- */
  async function savePolicy() {
    const ok = await policyApi.savePolicy()
    // 失败时草稿必须留着；成功时脏条数已归零，persistDrafts 会直接移除缓存。
    if (ok) persistDrafts()
    return ok
  }

  async function saveReplies() {
    const ok = await repliesApi.saveReplies()
    if (ok) persistDrafts()
    return ok
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

  onMounted(async () => {
    const stored = readStoredDrafts()
    await Promise.all([catalogApi.loadCatalog(), repliesApi.loadReplies()])
    restoreDrafts(stored)
  })

  onBeforeUnmount(() => {
    unregisterDirty()
    // 切页/关页前兜底写一次：避免最后一次编辑还停在 300ms 的合并窗口里丢掉。
    persistDrafts()
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
    savePolicy,
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
    saveReplies,
    discardReplies: repliesApi.discardReplies,
    refreshReplies,
    // 通用
    saving,
    policyNotice,
  }
}
