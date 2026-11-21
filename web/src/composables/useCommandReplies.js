// 命令面板：回复草稿。
//
// 草稿形状是 `{ 命令 id: 回复行数组 }`；只有改过的命令才进草稿。
// 保存时过滤掉 active === false 的行（停用 = 不提交），并带上 baseRevision 做乐观并发。

import { computed, nextTick, ref } from 'vue'
import { getJson, requestJson } from '../api.js'
import { handleAuth, toast } from '../store.js'

const DEFAULT_MAX_REPLY_CHARS = 300

export function useCommandReplies({ selectedEntry, ensureSelection, hasReplyDraft }) {
  const replies = ref(null)
  const replyLoading = ref(false)
  const replyError = ref('')
  const replyNotice = ref('')
  const replyDraft = ref({})
  const replyBaseRevision = ref(null)
  const savingReplies = ref(false)
  const lastFocusedRow = ref(0)
  const replyInputs = ref([])

  const replyDirtyCount = computed(() => Object.keys(replyDraft.value).length)

  const replyRows = computed(() => {
    const entry = selectedEntry.value
    if (!entry) return []
    const draft = replyDraft.value[String(entry.id)]
    if (Array.isArray(draft)) return draft
    return Array.isArray(entry.replies) ? entry.replies : []
  })

  const maxReplyChars = computed(() => {
    const limit = Number(replies.value?.limits?.maxReplyChars)
    return Number.isFinite(limit) && limit > 0 ? limit : DEFAULT_MAX_REPLY_CHARS
  })

  /** 当前命令可插入的变量：命令自己的 allowedVariables 优先，否则用全局变量表。 */
  const replyVars = computed(() => {
    const entry = selectedEntry.value
    if (!entry) return []
    const allowed = Array.isArray(entry.allowedVariables)
      ? new Set(entry.allowedVariables.map((name) => (typeof name === 'string' ? name : name?.name)))
      : null
    return (replies.value?.variables || [])
      .map((variable) => (typeof variable === 'string' ? { name: variable } : variable))
      .filter((variable) => variable && variable.name)
      .filter((variable) => !allowed || allowed.has(variable.name))
      .map((variable) => ({
        name: String(variable.name),
        label: variable.label ? String(variable.label) : '',
        example: variable.example ? String(variable.example) : '',
      }))
  })

  const replyStatus = computed(() => {
    if (replyError.value) return replyError.value
    if (savingReplies.value) return '保存回复中…'
    if (replyNotice.value) return replyNotice.value
    if (replyDirtyCount.value) return `回复修改未保存（基线版本 ${replyBaseRevision.value ?? '—'}）`
    if (!replies.value) return '回复目录未加载'
    return `回复版本 ${replies.value.revision ?? '—'}`
  })

  /** 可批量恢复的命令数：有 defaultReplies 且可编辑（与单命令「恢复内置回复」同源）。 */
  const restorableReplyCount = computed(() => {
    const list = Array.isArray(replies.value?.commands) ? replies.value.commands : []
    return list.filter(
      (entry) => entry && entry.editable !== false && Array.isArray(entry.defaultReplies) && entry.defaultReplies.length,
    ).length
  })

  /** 取当前命令的草稿行；第一次编辑时才从服务端值复制一份，避免无谓的脏标记。 */
  function draftRowsForEdit() {
    const entry = selectedEntry.value
    if (!entry) return null
    const id = String(entry.id)
    let rows = replyDraft.value[id]
    if (!Array.isArray(rows)) {
      if (replyBaseRevision.value == null) replyBaseRevision.value = replies.value?.revision ?? 0
      rows = (Array.isArray(entry.replies) ? entry.replies : []).map((row) => ({ ...row }))
    }
    return { id, rows }
  }

  function commitRows(id, rows) {
    if (replyBaseRevision.value == null) replyBaseRevision.value = replies.value?.revision ?? 0
    replyDraft.value = { ...replyDraft.value, [id]: rows }
    replyNotice.value = ''
  }

  function setReplyInput(index, element) {
    replyInputs.value[index] = element || null
  }

  function onReplyInput(index, event) {
    const target = draftRowsForEdit()
    if (!target || savingReplies.value) return
    const rows = target.rows.slice()
    rows[index] = { ...rows[index], text: event.target.value }
    commitRows(target.id, rows)
  }

  function setRowActive(index, active) {
    const target = draftRowsForEdit()
    if (!target || savingReplies.value) return
    const rows = target.rows.slice()
    if (!rows[index]) return
    rows[index] = { ...rows[index], active }
    commitRows(target.id, rows)
  }

  function moveRow(index, delta) {
    const target = draftRowsForEdit()
    if (!target || savingReplies.value) return
    const rows = target.rows.slice()
    const to = index + delta
    if (to < 0 || to >= rows.length) return
    ;[rows[index], rows[to]] = [rows[to], rows[index]]
    commitRows(target.id, rows)
  }

  function removeRow(index) {
    const target = draftRowsForEdit()
    if (!target || savingReplies.value) return
    if (target.rows[index]?.defaultText != null) return
    const rows = target.rows.slice()
    rows.splice(index, 1)
    commitRows(target.id, rows)
  }

  function addRow() {
    const target = draftRowsForEdit()
    if (!target || savingReplies.value) return
    const rows = target.rows.slice()
    rows.push({
      key: `custom:${Date.now().toString(36)}`,
      text: '',
      defaultText: null,
      editable: true,
      active: true,
    })
    commitRows(target.id, rows)
    lastFocusedRow.value = rows.length - 1
  }

  /** 单命令恢复内置回复：只在草稿里替换，仍需点「保存回复」才写入。 */
  function restoreDefaults() {
    const entry = selectedEntry.value
    if (!entry || entry.editable === false || savingReplies.value) return
    if (!window.confirm('将此命令恢复为全部内置回复？当前自定义回复和顺序会被替换。')) return
    const rows = (Array.isArray(entry.defaultReplies) ? entry.defaultReplies : []).map((row) => ({
      ...row,
      text: row.defaultText != null ? row.defaultText : row.text,
      active: true,
    }))
    commitRows(String(entry.id), rows)
  }

  /**
   * 全部恢复内置回复：按 /commands/replies 的 defaultReplies 把所有可编辑命令
   * 套成默认草稿，仍走同一个保存接口（不新增 API）。
   */
  function restoreAllReplyDefaults() {
    if (savingReplies.value || !replies.value) return
    const list = Array.isArray(replies.value.commands) ? replies.value.commands : []
    const targets = list.filter(
      (entry) => entry && entry.editable !== false && Array.isArray(entry.defaultReplies) && entry.defaultReplies.length,
    )
    if (!targets.length) {
      toast('后端没有提供可恢复的内置默认回复', 'err')
      return
    }
    if (
      !window.confirm(
        `把 ${targets.length} 个命令的回复恢复为内置默认？\n会覆盖当前全部自定义回复；只暂存草稿，点「保存回复」后才写入。`,
      )
    ) {
      return
    }
    if (replyBaseRevision.value == null) replyBaseRevision.value = replies.value?.revision ?? 0
    const next = { ...replyDraft.value }
    for (const entry of targets) {
      next[String(entry.id)] = entry.defaultReplies.map((row) => ({
        ...row,
        text: row.defaultText != null ? row.defaultText : row.text || '',
        active: true,
      }))
    }
    replyDraft.value = next
    replyError.value = ''
    replyNotice.value = `已把 ${targets.length} 个命令恢复为内置默认（草稿，点「保存回复」写入）`
    toast(`已暂存 ${targets.length} 个命令的内置默认回复（尚未保存）`, 'ok')
  }

  /** 静音：把当前命令的回复草稿置空数组（保存后即不再回复）。 */
  function muteCommand() {
    const entry = selectedEntry.value
    if (!entry || entry.editable === false || savingReplies.value) return
    if (!window.confirm('确定静音此命令？它将不再发送任何回复。')) return
    commitRows(String(entry.id), [])
  }

  /** 在光标处插入 `{变量}`；没有焦点时插到末尾。 */
  function insertVariable(name) {
    const target = draftRowsForEdit()
    if (!target || savingReplies.value) return
    const index = Math.min(Math.max(lastFocusedRow.value, 0), Math.max(target.rows.length - 1, 0))
    const row = target.rows[index]
    if (!row) return
    const element = replyInputs.value[index]
    const text = String(row.text || '')
    const start = element && typeof element.selectionStart === 'number' ? element.selectionStart : text.length
    const end = element && typeof element.selectionEnd === 'number' ? element.selectionEnd : text.length
    const token = `{${name}}`
    const rows = target.rows.slice()
    rows[index] = { ...row, text: text.slice(0, start) + token + text.slice(end) }
    commitRows(target.id, rows)
    nextTick(() => {
      const input = replyInputs.value[index]
      if (!input) return
      input.focus()
      const position = start + token.length
      if (typeof input.setSelectionRange === 'function') input.setSelectionRange(position, position)
    })
  }

  function resetReplies() {
    replyDraft.value = {}
    replyBaseRevision.value = null
    replyError.value = ''
    replyNotice.value = ''
  }

  async function loadReplies(force = false) {
    if (replyLoading.value) return
    if (!force && hasReplyDraft.value) return
    replyLoading.value = true
    replyError.value = ''
    try {
      const data = await getJson('/commands/replies')
      if (!data || !Array.isArray(data.commands) || !Array.isArray(data.variables)) {
        throw new Error('回复目录响应缺少 commands 或 variables')
      }
      replies.value = data
      ensureSelection()
    } catch (error) {
      if (!handleAuth(error)) replyError.value = `读取回复目录失败：${error.message || error}`
    } finally {
      replyLoading.value = false
    }
  }

  function discardReplies() {
    if (!replyDirtyCount.value || savingReplies.value) return
    if (!window.confirm('放弃所有未保存回复并重新加载？')) return
    resetReplies()
    loadReplies(true)
  }

  async function saveReplies() {
    if (!replyDirtyCount.value || savingReplies.value) return false
    savingReplies.value = true
    replyError.value = ''
    replyNotice.value = ''
    try {
      const commands = Object.entries(replyDraft.value).map(([id, rows]) => ({
        id,
        replies: (Array.isArray(rows) ? rows : [])
          .filter((row) => row.active !== false)
          .map((row) => ({ key: String(row.key || ''), text: String(row.text || '') })),
      }))
      const result = await requestJson('/commands/replies', {
        baseRevision: replyBaseRevision.value ?? replies.value?.revision ?? 0,
        commands,
      })
      if (!result || result.ok !== true) throw new Error(result?.message || '回复保存失败')
      replyDraft.value = {}
      replyBaseRevision.value = null
      await loadReplies(true)
      replyNotice.value = `回复已保存，版本 ${result.revision}`
      toast(`回复已保存，版本 ${result.revision}`, 'ok')
      return true
    } catch (error) {
      if (handleAuth(error)) return false
      replyError.value = String(error.message || '').includes('回复配置已变化')
        ? '回复配置已变化；草稿已保留，请点「放弃回复草稿」重新加载后再编辑。'
        : `回复保存失败：${error.message || error}`
      return false
    } finally {
      savingReplies.value = false
    }
  }

  return {
    replies,
    replyLoading,
    replyError,
    replyNotice,
    replyDraft,
    replyBaseRevision,
    savingReplies,
    lastFocusedRow,
    replyDirtyCount,
    hasReplyDraft,
    replyRows,
    maxReplyChars,
    replyVars,
    replyStatus,
    restorableReplyCount,
    setReplyInput,
    onReplyInput,
    setRowActive,
    moveRow,
    removeRow,
    addRow,
    restoreDefaults,
    restoreAllReplyDefaults,
    muteCommand,
    insertVariable,
    loadReplies,
    discardReplies,
    saveReplies,
  }
}
