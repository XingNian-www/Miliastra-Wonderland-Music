// 命令面板：目录加载 / 筛选 / 选中。
//
// `/commands/catalog` 只读一次，之后所有筛选（来源、搜索、隐藏、分类）与选中态
// 都在本地派生。回复目录由外部传入（`repliesRef`），因为别名表与命令目录谁先到
// 并不固定——两边都要能触发选中归一化。

import { computed, ref } from 'vue'
import { getJson } from '../api.js'
import { handleAuth } from '../store.js'
import { itemKey } from '../util/commandCatalog.js'

export function useCommandCatalog({ repliesRef, hasPolicyDraft }) {
  const catalog = ref(null)
  const catalogLoading = ref(false)
  const catalogError = ref('')
  const fetchedAt = ref('')
  const searchText = ref('')
  const sourceFilter = ref('all')
  const showHidden = ref(false)
  const activeCategory = ref('all')
  const selectedId = ref('')
  const collapsed = ref(new Set())
  const expanded = ref(new Set())

  const commandsEnabledLabel = computed(() => {
    if (!catalog.value) return catalogError.value ? '读取失败' : '读取中…'
    if (catalog.value.commandsEnabled === false) return '已禁用'
    if (catalog.value.commandsEnabled === true) return '已启用'
    return '状态未知'
  })

  const catalogState = computed(() => {
    if (!catalog.value) return catalogError.value ? 'err' : ''
    if (catalog.value.commandsEnabled === false) return 'warn'
    if (catalog.value.commandsEnabled === true) return 'ok'
    return ''
  })

  const catalogMeta = computed(() => {
    if (!catalog.value) return catalogError.value ? '命令目录读取失败' : '正在读取命令目录…'
    const parts = [`版本 ${catalog.value.revision ?? '—'}`]
    if (fetchedAt.value) parts.push(`最近读取 ${fetchedAt.value}`)
    return parts.join(' · ')
  })

  /** 别名 id → 规范 id：后端允许同一命令有多个 id，选中态必须归一化。 */
  const aliasOwners = computed(() => {
    const map = {}
    for (const item of repliesRef.value?.aliases || []) {
      if (item && item.id != null && item.owner != null) map[String(item.id)] = String(item.owner)
    }
    return map
  })

  function canonicalId(id) {
    const key = String(id == null ? '' : id)
    return aliasOwners.value[key] || key
  }

  /** 回复目录里按 id 索引的命令，选中项由它解析。 */
  const replyEntries = computed(() => {
    const map = {}
    for (const entry of repliesRef.value?.commands || []) {
      if (entry && entry.id != null) map[String(entry.id)] = entry
    }
    return map
  })

  const selectedEntry = computed(() =>
    selectedId.value ? replyEntries.value[selectedId.value] || null : null,
  )

  const categories = computed(() => {
    const groups = catalog.value?.groups || []
    const total = groups.reduce((sum, group) => sum + (Array.isArray(group.commands) ? group.commands.length : 0), 0)
    return [
      { value: 'all', label: '全部', count: total },
      ...groups.map((group) => ({
        value: group.name,
        label: group.name || '未分组',
        count: Array.isArray(group.commands) ? group.commands.length : 0,
      })),
    ]
  })

  function matches(item) {
    if (!item || typeof item !== 'object') return false
    if (item.hidden && !showHidden.value) return false
    if (sourceFilter.value !== 'all') {
      const authorities = Array.isArray(item.authorities)
        ? item.authorities.map((value) => String(value).toLowerCase())
        : []
      if (sourceFilter.value === 'hall' && !authorities.includes('hall')) return false
      if (sourceFilter.value === 'friend' && !authorities.includes('friend')) return false
    }
    const query = searchText.value.trim().toLowerCase()
    if (!query) return true
    const haystack = [item.name, item.summary, item.id, ...(Array.isArray(item.aliases) ? item.aliases : [])]
      .filter((value) => value != null)
      .join('\n')
      .toLowerCase()
    return haystack.includes(query)
  }

  const visibleGroups = computed(() =>
    (catalog.value?.groups || [])
      .filter((group) => activeCategory.value === 'all' || group.name === activeCategory.value)
      .map((group) => ({
        name: group.name,
        commands: (Array.isArray(group.commands) ? group.commands : []).filter((item) => matches(item)),
      }))
      .filter((group) => group.commands.length),
  )

  const visibleCount = computed(() =>
    visibleGroups.value.reduce((sum, group) => sum + group.commands.length, 0),
  )

  const totalCount = computed(() =>
    (catalog.value?.groups || []).reduce(
      (sum, group) => sum + (Array.isArray(group.commands) ? group.commands.length : 0),
      0,
    ),
  )

  const selectedVisible = computed(() =>
    visibleGroups.value.some((group) => group.commands.some((item) => isSelected(item))),
  )

  function isSelected(item) {
    return Boolean(item && item.id != null && canonicalId(item.id) === selectedId.value)
  }

  function selectCommand(itemOrId) {
    const id = typeof itemOrId === 'object' && itemOrId ? itemOrId.id : itemOrId
    if (id == null) return
    selectedId.value = canonicalId(id)
  }

  /**
   * 选中态归一化：目录与回复目录谁先到都调用一次。
   * 先尝试把已选 id 归一到当前别名表；失败则退回第一条有 id 的命令。
   */
  function ensureSelection() {
    if (selectedId.value) {
      const canonical = canonicalId(selectedId.value)
      if (replyEntries.value[canonical]) {
        selectedId.value = canonical
        return
      }
      // 回复目录尚未加载：先保留当前选中，等别名表到达后再归一化。
      if (!repliesRef.value) return
    }
    const groups = catalog.value?.groups || []
    for (const group of groups) {
      for (const command of group.commands || []) {
        if (command && command.id != null) {
          selectCommand(command)
          return
        }
      }
    }
    if (!selectedId.value && repliesRef.value?.commands?.length) {
      selectCommand(repliesRef.value.commands[0].id)
    }
  }

  function toggleGroup(name) {
    const next = new Set(collapsed.value)
    if (next.has(name)) next.delete(name)
    else next.add(name)
    collapsed.value = next
  }

  function toggleDetail(item) {
    const key = itemKey(item)
    const next = new Set(expanded.value)
    if (next.has(key)) next.delete(key)
    else next.add(key)
    expanded.value = next
  }

  async function loadCatalog(force = false) {
    if (catalogLoading.value) return
    if (!force && hasPolicyDraft.value) return
    catalogLoading.value = true
    catalogError.value = ''
    try {
      const data = await getJson('/commands/catalog')
      if (!data || typeof data !== 'object' || Array.isArray(data) || !Array.isArray(data.groups)) {
        throw new Error('命令目录响应格式不正确')
      }
      catalog.value = {
        commandsEnabled: data.commandsEnabled,
        groups: data.groups,
        revision: data.revision,
        defaultSongSource: data.defaultSongSource,
        defaultSongSourceSelectable: data.defaultSongSourceSelectable,
        songPlatforms: data.songPlatforms,
      }
      fetchedAt.value = new Date().toLocaleTimeString('zh-CN', { hour12: false })
      ensureSelection()
    } catch (error) {
      if (!handleAuth(error)) catalogError.value = `读取命令目录失败：${error.message || error}`
    } finally {
      catalogLoading.value = false
    }
  }

  return {
    catalog,
    catalogLoading,
    catalogError,
    fetchedAt,
    searchText,
    sourceFilter,
    showHidden,
    activeCategory,
    selectedId,
    collapsed,
    expanded,
    commandsEnabledLabel,
    catalogState,
    catalogMeta,
    selectedEntry,
    categories,
    visibleGroups,
    visibleCount,
    totalCount,
    selectedVisible,
    canonicalId,
    isSelected,
    selectCommand,
    ensureSelection,
    toggleGroup,
    toggleDetail,
    loadCatalog,
  }
}
