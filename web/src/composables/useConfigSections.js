// 配置中心：段导航与搜索投影。
//
// 只做「schema → 侧边栏/表单结构」的派生：域分组、搜索命中、当前段、字段分组、
// 效果统计与高亮。不碰草稿、不碰保存——那些在 useConfigDraft / useConfigForm。

import { computed, nextTick, reactive, ref, watch } from 'vue'
import { state } from '../store.js'
import {
  DOMAIN_LABELS,
  DOMAIN_ORDER,
  HIDDEN_SECTIONS,
  EFFECT_LABELS,
  sectionFieldCount,
} from '../util/configSchema.js'

const SECTION_MEMORY_KEY = 'miliastraConfigSection'

function escapeHtml(text) {
  const map = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }
  return String(text == null ? '' : text).replace(/[&<>"']/g, (char) => map[char])
}

function escapeRegExp(text) {
  return String(text).replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
}

export function useConfigSections() {
  const searchText = ref('')
  const activeSectionName = ref('')
  const domainOpen = reactive({})

  const searchQuery = computed(() => searchText.value.trim().toLowerCase())

  /** 可见段：过滤掉有独立页面的段（identity / ai / ai_chat / tts / command_access）。 */
  const sections = computed(() =>
    (Array.isArray(state.schema) ? state.schema : []).filter((section) => !HIDDEN_SECTIONS.has(section.name)),
  )

  const visibleSections = computed(() => {
    const query = searchQuery.value
    if (!query) return sections.value
    return sections.value.filter((section) => sectionMatches(section))
  })

  const activeSection = computed(
    () => sections.value.find((section) => section.name === activeSectionName.value) || null,
  )

  const visibleFields = computed(() => {
    const section = activeSection.value
    if (!section) return []
    const fields = Array.isArray(section.fields) ? section.fields : []
    if (!searchQuery.value) return fields
    return fields.filter((field) => fieldMatches(field))
  })

  /** 按 `field.group` 连续切块：同一组名的相邻字段归入一张卡片。 */
  const groups = computed(() => {
    const result = []
    for (const field of visibleFields.value) {
      const name = field.group || ''
      let group = result[result.length - 1]
      if (!group || group.name !== name) {
        group = { key: `${name || 'default'}-${result.length}`, name, fields: [] }
        result.push(group)
      }
      group.fields.push(field)
    }
    return result
  })

  const domains = computed(() => {
    const groupsByDomain = new Map()
    for (const section of visibleSections.value) {
      const key = String(section.domain || 'system')
      if (!groupsByDomain.has(key)) groupsByDomain.set(key, [])
      groupsByDomain.get(key).push(section)
    }
    const keys = Array.from(groupsByDomain.keys()).sort((left, right) => {
      const leftRank = DOMAIN_ORDER.indexOf(left)
      const rightRank = DOMAIN_ORDER.indexOf(right)
      return (leftRank < 0 ? 99 : leftRank) - (rightRank < 0 ? 99 : rightRank)
    })
    return keys.map((key) => {
      const list = groupsByDomain.get(key).slice().sort((a, b) => (a.order || 0) - (b.order || 0))
      return {
        key,
        label: list[0].domainLabel || DOMAIN_LABELS[key] || key,
        sections: list,
        dirty: list.some((section) => state.dirty[section.name]),
        open:
          !!domainOpen[key] ||
          list.some((section) => section.name === activeSectionName.value) ||
          !!searchQuery.value,
      }
    })
  })

  const matchedFieldCount = computed(() => {
    if (!searchQuery.value) return 0
    let count = 0
    for (const section of visibleSections.value) {
      for (const field of section.fields || []) {
        if (fieldMatches(field)) count += 1
      }
    }
    return count
  })

  const effectSummary = computed(() => {
    const counts = {}
    for (const field of visibleFields.value) {
      if (field.source === 'bootstrap') continue
      const key = EFFECT_LABELS[field.effect] ? field.effect : 'restart'
      counts[key] = (counts[key] || 0) + 1
    }
    return Object.keys(EFFECT_LABELS)
      .filter((key) => counts[key])
      .map((key) => ({ key, label: EFFECT_LABELS[key], count: counts[key] }))
  })

  function fieldMatches(field) {
    const query = searchQuery.value
    if (!query) return true
    return [field.label, field.path, field.hint].join(' ').toLowerCase().includes(query)
  }

  function sectionMatches(section) {
    const query = searchQuery.value
    if (!query) return true
    if ([section.label, section.name, section.domainLabel].join(' ').toLowerCase().includes(query)) return true
    return (section.fields || []).some((field) => fieldMatches(field))
  }

  /** 搜索高亮：先转义 HTML，再按查询串包 `<mark>`（查询串来自用户输入，必须转义正则）。 */
  function highlight(text) {
    const safe = escapeHtml(text)
    const query = searchQuery.value
    if (!query) return safe
    try {
      return safe.replace(new RegExp(`(${escapeRegExp(query)})`, 'ig'), '<mark>$1</mark>')
    } catch (error) {
      return safe
    }
  }

  function selectSection(name) {
    activeSectionName.value = name
    try {
      localStorage.setItem(SECTION_MEMORY_KEY, name)
    } catch (error) {
      /* 隐私模式下存储不可用：只影响下次默认段，不影响本次编辑 */
    }
  }

  function selectInitialSection() {
    const list = visibleSections.value
    if (!list.length) return
    let remembered = ''
    try {
      remembered = localStorage.getItem(SECTION_MEMORY_KEY) || ''
    } catch (error) {
      remembered = ''
    }
    const match =
      list.find((section) => section.name === remembered) ||
      list.find((section) => section.name !== 'visual') ||
      list[0]
    activeSectionName.value = match.name
  }

  function toggleDomain(key) {
    domainOpen[key] = !domainOpen[key]
  }

  /** 跳到某个字段：先切到它所在的段，再滚到视野中间（保存失败 / 校验失败时用）。 */
  function revealField(path) {
    const sectionName = String(path).split('.')[0]
    if (sections.value.some((section) => section.name === sectionName)) {
      activeSectionName.value = sectionName
    }
    nextTick(() => {
      const target = document.querySelector(`[data-cfg-field="${path}"]`)
      if (target && typeof target.scrollIntoView === 'function') {
        target.scrollIntoView({ block: 'center' })
      }
    })
  }

  watch(visibleSections, (list) => {
    if (!list.length) return
    if (!list.some((section) => section.name === activeSectionName.value)) {
      activeSectionName.value = list[0].name
    }
  })

  // schema 可能已在别的页面载入过：setup 阶段就先选好当前段，避免首帧空表单。
  if (!activeSectionName.value && visibleSections.value.length) selectInitialSection()

  return {
    searchText,
    searchQuery,
    activeSectionName,
    sections,
    visibleSections,
    activeSection,
    visibleFields,
    groups,
    domains,
    matchedFieldCount,
    effectSummary,
    selectSection,
    selectInitialSection,
    toggleDomain,
    highlight,
    revealField,
    sectionFieldCount,
  }
}
