<template>
  <h1 class="page-title">身份映射</h1>
  <p class="page-subtitle">游戏备注（OCR 匹配键 / 授权依据）/ 显示昵称（仅展示）↔ 记忆键；账号 UID 是授权与归属的优先依据</p>
  <div class="loading-card" v-if="!loaded">加载中…</div>
  <div v-else class="identity-page">
    <div v-if="loadError" class="config-message err">{{ loadError }}</div>

    <div class="identity-toolbar">
      <input v-model="search" placeholder="搜索游戏备注 / 显示昵称 / UID…" aria-label="搜索身份映射" />
      <span class="config-field-hint">共 {{ rows.length }} 条，当前显示 {{ filteredRows.length }} 条</span>
      <span style="flex: 1"></span>
      <button class="btn small" type="button" @click="addRow">添加映射</button>
      <button class="btn small ghost" type="button" @click="openJson">JSON 编辑</button>
    </div>

    <div class="identity-table-wrap">
      <table class="identity-table">
        <thead>
          <tr>
            <th style="width: 120px" title="游戏内好友备注：屏幕显示名 / OCR 精确匹配键 / 授权依据（JSON 键 nickname）">游戏备注</th>
            <th style="width: 120px" title="软件内显示昵称：仅用于展示，可空，不参与授权（JSON 键 note）">显示昵称</th>
            <th style="width: 120px">真实昵称</th>
            <th style="width: 100px">角色</th>
            <th>账号（game:uid，逗号分隔）</th>
            <th style="width: 60px"></th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="row in filteredRows" :key="row.id || row.key" :class="{ dirty: row.dirty }">
            <td>
              <input
                v-model="row.remark"
                placeholder="游戏内备注"
                title="写入 JSON 键 nickname：屏幕显示名 / OCR 匹配键 / 授权依据"
                @input="markDirty(row)"
              />
            </td>
            <td>
              <input
                v-model="row.displayName"
                placeholder="显示昵称（可空）"
                title="写入 JSON 键 note：软件内显示昵称，仅展示，不授权"
                @input="markDirty(row)"
              />
            </td>
            <td><input v-model="row.real_nickname" placeholder="真实昵称" @input="markDirty(row)" /></td>
            <td>
              <select v-model="row.role" @change="markDirty(row)">
                <option value="friend">好友</option>
                <option value="admin">管理员</option>
                <option value="owner">主人</option>
              </select>
            </td>
            <td><input v-model="row.accountsText" placeholder="genshin:123456" @input="markDirty(row)" /></td>
            <td><button class="btn small danger" type="button" @click="removeRow(row)">删除</button></td>
          </tr>
          <tr v-if="!filteredRows.length">
            <td colspan="6" class="config-field-hint" style="text-align:center;padding:20px">
              {{ rows.length ? '没有匹配的映射' : '还没有身份映射，点「添加映射」' }}
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <details class="identity-json" :open="jsonOpen">
      <summary>JSON 编辑（高级）</summary>
      <textarea v-model="jsonText" class="config-textarea" spellcheck="false" @input="jsonDirty = true"></textarea>
      <div class="identity-json-actions">
        <button class="btn small" type="button" @click="applyJson">应用到表格</button>
        <button class="btn small" type="button" @click="formatJson">格式化</button>
        <button class="btn small ghost" type="button" @click="syncJsonFromRows">从表格重新生成</button>
        <span v-if="jsonError" class="config-field-error">{{ jsonError }}</span>
      </div>
    </details>

    <div class="command-savebar identity-savebar">
      <button class="btn primary" type="button" :disabled="busy || !loaded || !dirtyCount" @click="save">
        {{ busy ? '保存中…' : `保存身份映射（${dirtyCount}）` }}
      </button>
      <button class="btn" type="button" :disabled="busy || !dirtyCount" @click="reload">放弃修改</button>
      <span class="config-field-hint">{{ message || (dirtyCount ? '有未保存修改' : '没有未保存修改') }}</span>
    </div>
  </div>
</template>

<script setup>
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { getJson } from '../api.js'
import { saveSections, toast } from '../store.js'
// 表格行 ↔ 后端 JSON 的换算全在 util/identityRow.js（纯函数、有单测钉死 nickname↔remark /
// note↔displayName 的方向）。这里只做状态与渲染，不要再把换算逻辑写回本文件。
import { blankRow, toMapping, toRow } from '../util/identityRow.js'

const data = ref(null)
const rows = ref([])
const search = ref('')
const busy = ref(false)
const loaded = ref(false)
const loadError = ref('')
const message = ref('')
const jsonOpen = ref(false)
const jsonText = ref('')
const jsonDirty = ref(false)
const jsonError = ref('')

const dirtyCount = computed(() => rows.value.filter((row) => row.dirty).length)
const filteredRows = computed(() => {
  const keyword = search.value.trim().toLowerCase()
  if (!keyword) return rows.value
  return rows.value.filter((row) =>
    [row.remark, row.displayName, row.real_nickname, row.accountsText].join(' ').toLowerCase().includes(keyword),
  )
})

function markDirty(row) {
  row.dirty = true
  message.value = ''
}

function buildMappings() {
  return rows.value.map(toMapping)
}

async function reload() {
  loading(true)
  try {
    data.value = await getJson('/config/section?name=identity')
    const values = data.value?.values ?? {}
    const mappings = Array.isArray(values.mappings) ? values.mappings : []
    rows.value = mappings.map(toRow)
    loaded.value = true
    loadError.value = ''
    message.value = ''
    jsonDirty.value = false
    syncJsonFromRows()
  } catch (error) {
    loadError.value = `加载失败：${error.message || error}`
    toast(`加载身份映射失败：${error.message || error}`, 'err')
  } finally {
    busy.value = false
  }
}

function loading(value) {
  busy.value = value
}

function syncJsonFromRows() {
  jsonText.value = JSON.stringify(buildMappings(), null, 2)
  jsonError.value = ''
}

function openJson() {
  jsonOpen.value = true
  syncJsonFromRows()
}

function formatJson() {
  try {
    jsonText.value = JSON.stringify(JSON.parse(jsonText.value), null, 2)
    jsonError.value = ''
  } catch (error) {
    jsonError.value = formatJsonError(error)
  }
}

function formatJsonError(error) {
  const match = String(error.message || error).match(/position\s+(\d+)/i)
  if (!match) return `JSON 解析失败：${error.message || error}`
  const position = Number(match[1])
  const before = jsonText.value.slice(0, position)
  const line = before.split('\n').length
  const column = position - before.lastIndexOf('\n')
  return `JSON 解析失败：第 ${line} 行第 ${column} 列`
}

function applyJson() {
  try {
    const parsed = JSON.parse(jsonText.value)
    if (!Array.isArray(parsed)) throw new Error('顶层必须是数组')
    rows.value = parsed.map((item, index) => {
      if (!item || typeof item !== 'object') throw new Error(`第 ${index + 1} 条不是对象`)
      const row = toRow(item, index)
      row.dirty = true
      return row
    })
    jsonError.value = ''
    message.value = '已从 JSON 应用到表格，保存后才会写入'
    toast('已应用到表格')
  } catch (error) {
    jsonError.value = formatJsonError(error)
  }
}

function addRow() {
  rows.value.unshift(blankRow())
}

function removeRow(row) {
  const label = row.displayName || row.real_nickname || row.remark || row.id
  if (!window.confirm(`删除「${label || '未命名'}」这条映射？保存后生效。`)) return
  rows.value = rows.value.filter((item) => item !== row)
}

async function save() {
  if (!loaded.value) {
    message.value = '尚未成功加载身份映射，禁止保存'
    return
  }
  const mappings = buildMappings()
  busy.value = true
  message.value = ''
  const ok = await saveSections({ identity: { mappings } }, () => {
    message.value = '已保存'
    rows.value.forEach((row) => { row.dirty = false })
    toast('身份映射已保存')
    syncJsonFromRows()
  })
  if (!ok && !message.value) message.value = '保存失败'
  busy.value = false
}

function beforeUnload(event) {
  if (!dirtyCount.value) return
  event.preventDefault()
  event.returnValue = ''
}

onMounted(() => {
  reload()
  window.addEventListener('beforeunload', beforeUnload)
})
onBeforeUnmount(() => window.removeEventListener('beforeunload', beforeUnload))
</script>

<style scoped>
.identity-page { padding: 0 24px 24px; max-width: 1480px; margin: 0 auto; }
.identity-toolbar { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; margin: 12px 0; }
.identity-toolbar input { flex: 1 1 220px; max-width: 360px; }
.identity-table-wrap { overflow-x: auto; border: 1px solid var(--sep-1); border-radius: 12px; }
.identity-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.identity-table th { text-align: left; padding: 8px 10px; color: var(--text-3); font-weight: 600; font-size: 11px; border-bottom: 1px solid var(--sep-1); white-space: nowrap; }
.identity-table td { padding: 6px 8px; border-bottom: 1px solid var(--sep-2); vertical-align: middle; }
.identity-table tr.dirty td { background: rgba(245, 176, 76, 0.06); }
.identity-table input, .identity-table select { width: 100%; min-width: 0; height: 28px; }
.identity-json { margin-top: 12px; border: 1px solid var(--sep-2); border-radius: 10px; padding: 8px 12px; }
.identity-json summary { cursor: pointer; font-size: 12.5px; color: var(--text-2); }
.identity-json textarea { width: 100%; min-height: 220px; margin-top: 8px; font-family: var(--font-mono); }
.identity-json-actions { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; margin-top: 6px; }
.identity-savebar { margin-top: 12px; }
@media (max-width: 720px) {
  .identity-page { padding: 0 16px 20px; }
}
</style>
