<template>
  <div id="page-commands">
    <h1 class="page-title">命令面板</h1>
    <p class="page-subtitle">聊天命令目录、回复覆盖与权限策略；权限与回复分别保存</p>

    <div class="command-status">
      <span class="chip" :data-state="catalogState">
        <i class="dot"></i><span>全局命令</span><em>{{ commandsEnabledLabel }}</em>
      </span>
      <span class="chip"><span>默认平台</span><em>{{ platformLabel(defaultSourceValue) || '—' }}</em></span>
      <span class="config-field-hint">{{ catalogMeta }}</span>
      <span v-if="policyDirtyCount || replyDirtyCount" class="config-dirty">
        未保存：权限 {{ policyDirtyCount }} · 回复 {{ replyDirtyCount }}
      </span>
      <button class="btn small" :disabled="catalogLoading || saving" @click="refreshCatalog">刷新目录</button>
    </div>

    <div class="row command-default-source">
      <label for="commandDefaultSongSource">@点歌默认平台</label>
      <select
        id="commandDefaultSongSource"
        :value="defaultSourceValue"
        :disabled="saving || !catalog || !(catalog.songPlatforms || []).length"
        @change="onDefaultSourceChange"
      >
        <option v-for="item in catalog?.songPlatforms || []" :key="item.value" :value="item.value">
          {{ item.label }}
        </option>
        <option v-if="unsupportedDefaultSource" :value="unsupportedDefaultSource" disabled>
          {{ platformLabel(unsupportedDefaultSource) }}（不能作为默认平台）
        </option>
      </select>
      <span class="config-field-hint">{{ defaultSourceHint }}</span>
    </div>
    <p class="config-field-hint">
      默认平台只影响 @点歌 走哪个平台的搜索，不改变各平台命令的权限；B站 请直接发 @B站点歌。
    </p>

    <div class="command-toolbar">
      <input v-model="searchText" type="text" placeholder="搜索命令名称 / 别名 / 摘要" aria-label="搜索命令" />
      <div class="tabs" role="group" aria-label="按来源过滤">
        <button
          v-for="tab in sourceTabs"
          :key="tab.value"
          class="tab"
          :class="{ active: sourceFilter === tab.value }"
          type="button"
          @click="sourceFilter = tab.value"
        >
          {{ tab.label }}
        </button>
      </div>
      <label class="switch-line">
        <input v-model="showHidden" type="checkbox" /> 显示隐藏命令
      </label>
      <span class="command-toolbar-meta">
        显示 {{ visibleCount }} / {{ totalCount }} 条<span v-if="fetchedAt"> · {{ fetchedAt }}</span>
      </span>
    </div>

    <div v-if="catalogError || policyError" class="config-message err" role="status">
      {{ catalogError || policyError }}
    </div>
    <div v-if="replyError" class="config-message err" role="status">{{ replyError }}</div>

    <div v-if="catalogLoading && !catalog" class="loading-card">正在读取命令目录…</div>
    <div v-else class="command-workspace">
      <nav class="command-reply-nav" aria-label="回复分类">
        <button
          v-for="item in categories"
          :key="item.value"
          type="button"
          class="btn small"
          :class="{ active: activeCategory === item.value }"
          @click="activeCategory = item.value"
        >
          {{ item.label }} · {{ item.count }}
        </button>
      </nav>

      <div class="command-groups">
        <div
          v-for="group in visibleGroups"
          :key="group.name"
          class="command-group"
          :class="{ collapsed: collapsed.has(group.name) }"
        >
          <button class="command-group-head" type="button" :aria-expanded="!collapsed.has(group.name)" @click="toggleGroup(group.name)">
            <span class="command-group-name">{{ group.name }}</span>
            <span class="command-group-count">{{ group.commands.length }} 条</span>
          </button>
          <div class="command-group-body">
            <div
              v-for="item in group.commands"
              :key="itemKey(item)"
              class="command-item"
              :class="{ 'hidden-command': item.hidden && showHidden, disabled: item.enabled === false, selected: isSelected(item) }"
            >
              <div class="command-head" @click="selectCommand(item)">
                <div class="command-title">
                  <button
                    type="button"
                    class="command-name command-select"
                    :class="{ active: isSelected(item) }"
                    @click="selectCommand(item)"
                  >
                    {{ item.name || '（未命名命令）' }}
                  </button>
                  <span v-for="alias in item.aliases || []" :key="alias" class="command-alias">{{ alias }}</span>
                </div>
                <div class="command-badges">
                  <span class="command-badge prefix">{{ item.prefix || '—' }}</span>
                  <span v-for="source in sourceLabels(item)" :key="source" class="command-badge source">语法来源：{{ source }}</span>
                  <span v-if="item.openEnded" class="command-badge open">会话中可自由输入</span>
                  <span v-if="item.hidden" class="command-badge hidden-mark">隐藏</span>
                  <span v-if="item.enabled === false" class="command-badge disabled-mark">已禁用</span>
                  <span class="command-badge source">最低：{{ roleLabel(item.minimumRole) }}</span>
                </div>
              </div>
              <p class="command-summary">{{ item.summary || '（暂无摘要）' }}</p>

              <div v-if="item.editable" class="row wrap" style="margin-top: 6px">
                <label class="switch-line">
                  <input
                    type="checkbox"
                    :checked="policyValue(item).enabled"
                    :disabled="saving"
                    @change="onPolicyEnabled(item, $event)"
                  />
                  <span>启用</span>
                </label>
                <label>
                  最低权限
                  <select
                    :value="policyValue(item).minimumRole"
                    :disabled="saving"
                    @change="onPolicyRole(item, $event)"
                  >
                    <option
                      v-for="role in roleOptions(item)"
                      :key="role.value"
                      :value="role.value"
                      :disabled="role.disabled"
                    >
                      {{ role.label }}
                    </option>
                  </select>
                </label>
                <span v-if="isPolicyDirty(item)" class="config-dirty">未保存</span>
                <span v-if="hasRoleFloor(item)" class="config-field-hint">
                  权限下限 {{ roleLabel(item.minimumFloor) }}，不能降低；仍可单独禁用
                </span>
                <span v-if="item.lockedReason" class="config-field-hint">{{ item.lockedReason }}</span>
                <span v-if="!item.editable" class="config-field-hint">该条目描述会话输入，请修改对应玩法命令的权限</span>
              </div>

              <div class="command-toggle">
                <button class="btn small ghost" type="button" @click="toggleDetail(item)">
                  {{ expanded.has(itemKey(item)) ? '收起' : '查看' }}用法与示例
                </button>
              </div>
              <div v-if="expanded.has(itemKey(item))" class="command-detail">
                <div v-if="commandLines(item.usage).length" class="command-detail-row">
                  <span class="command-detail-label">用法</span>
                  <div class="command-detail-lines">
                    <code v-for="line in commandLines(item.usage)" :key="'u' + line">{{ line }}</code>
                  </div>
                </div>
                <div v-if="commandLines(item.examples).length" class="command-detail-row">
                  <span class="command-detail-label">示例</span>
                  <div class="command-detail-lines">
                    <code v-for="line in commandLines(item.examples)" :key="'e' + line">{{ line }}</code>
                  </div>
                </div>
                <div v-if="!commandLines(item.usage).length && !commandLines(item.examples).length" class="command-detail-row command-detail-empty">
                  后端未提供用法与示例
                </div>
                <div v-if="item.openEnded" class="command-detail-row command-detail-open">
                  会话中可自由输入：玩法进行中接受任意输入，上列关键词只是入口或保留词。
                </div>
              </div>
            </div>
          </div>
        </div>
        <div v-if="!visibleCount" class="command-empty">
          没有匹配的命令；可清空搜索、放宽来源过滤，或勾选「显示隐藏命令」。
        </div>
      </div>

      <section class="command-reply-editor" aria-label="命令回复编辑">
        <template v-if="selectedEntry">
          <div class="row wrap" style="justify-content: space-between; align-items: baseline">
            <h3>{{ selectedEntry.name || selectedEntry.id }} 回复</h3>
            <button class="btn small ghost" type="button" :disabled="saving || replyLoading" @click="refreshReplies">刷新回复目录</button>
          </div>
          <p class="config-field-hint">
            {{ selectedEntry.editable === false ? '此命令的回复不可编辑。' : '多行文案按顺序逐条发送；留空使用内置默认；停用/静音控制是否回复。' }}
          </p>
          <template v-if="selectedEntry.editable !== false">
            <div class="command-reply-list">
              <article
                v-for="(row, index) in replyRows"
                :key="`${row.key || 'row'}:${index}`"
                class="command-reply-row"
                :class="{ inactive: row.active === false }"
              >
                <div class="row wrap command-reply-head">
                  <strong>{{ row.label || row.key || '自定义回复' }}</strong>
                  <code class="command-reply-key">{{ row.key || '自定义' }}</code>
                  <span v-if="row.active === false" class="field-state">已停用</span>
                </div>
                <div v-if="row.defaultText != null" class="config-field-hint">内置默认：{{ row.defaultText }}</div>
                <textarea
                  :ref="(el) => setReplyInput(index, el)"
                  rows="3"
                  :maxlength="maxReplyChars"
                  :value="row.text"
                  :placeholder="row.defaultText != null ? '留空则使用内置默认' : '输入回复文案；换行即分条发送'"
                  :disabled="saving"
                  :aria-label="`回复文案 ${row.key || index + 1}`"
                  @focus="lastFocusedRow = index"
                  @input="onReplyInput(index, $event)"
                ></textarea>
                <div class="command-reply-tools">
                  <button class="btn small ghost" type="button" :disabled="saving || index === 0" @click="moveRow(index, -1)">上移</button>
                  <button class="btn small ghost" type="button" :disabled="saving || index === replyRows.length - 1" @click="moveRow(index, 1)">下移</button>
                  <button v-if="row.active === false" class="btn small" type="button" :disabled="saving" @click="setRowActive(index, true)">启用</button>
                  <button v-else class="btn small ghost" type="button" :disabled="saving" @click="setRowActive(index, false)">停用</button>
                  <button v-if="row.defaultText == null" class="btn small ghost" type="button" :disabled="saving" @click="removeRow(index)">删除</button>
                </div>
                <div v-if="replyVars.length" class="command-reply-vars" aria-label="插入变量">
                  <button
                    v-for="variable in replyVars"
                    :key="variable.name"
                    type="button"
                    class="btn small ghost"
                    :title="`${variable.label || variable.name}｜插入 {${variable.name}}（如 ${variable.example || variable.name}）`"
                    :disabled="saving"
                    @click="insertVariable(variable.name)"
                  >
                    {{ '{' + variable.name + '}' }}
                  </button>
                </div>
              </article>
              <div v-if="!replyRows.length" class="command-empty">
                当前无回复；把文案留空表示使用内置默认，点“静音此命令”才会停止回复。
              </div>
            </div>
            <div class="row wrap" style="margin-top: 8px">
              <button class="btn small" type="button" :disabled="saving" @click="addRow">添加回复</button>
              <button class="btn small" type="button" :disabled="saving" @click="restoreDefaults">恢复内置回复</button>
              <button class="btn small warn" type="button" :disabled="saving" @click="muteCommand">静音此命令</button>
            </div>
            <p class="config-field-hint">{{ replyStatus }}</p>
          </template>
        </template>
        <div v-else class="command-empty">
          <template v-if="replyLoading">正在读取回复目录…</template>
          <template v-else>选择一个命令编辑回复</template>
        </div>
      </section>
    </div>

    <div class="command-savebar row wrap">
      <button class="btn primary" type="button" :disabled="saving || !policyDirtyCount" @click="savePolicy">
        {{ savingPolicy ? '保存中…' : `保存权限与平台（${policyDirtyCount}）` }}
      </button>
      <button class="btn" type="button" :disabled="saving || !policyDirtyCount" @click="discardPolicy">放弃权限草稿</button>
      <button class="btn primary" type="button" :disabled="saving || !replyDirtyCount" @click="saveReplies">
        {{ savingReplies ? '保存中…' : `保存回复（${replyDirtyCount}）` }}
      </button>
      <button class="btn" type="button" :disabled="saving || !replyDirtyCount" @click="discardReplies">放弃回复草稿</button>
      <button
        class="btn"
        type="button"
        :disabled="saving || replyLoading || !replies || !restorableReplyCount"
        title="按 GET /commands/replies 返回的 defaultReplies 逐个命令套回默认文案；只暂存草稿，点「保存回复」才写入"
        @click="restoreAllReplyDefaults"
      >
        全部恢复内置回复（{{ restorableReplyCount }}）
      </button>
      <span class="config-field-hint">{{ policyNotice || replyNotice }}</span>
    </div>
    <p class="config-field-hint">
      最低权限按普通用户 → 好友 → 管理员 → 主人逐级包含；来源与权限分开；权限与回复各有独立保存按钮。
    </p>
  </div>
</template>

<script setup>
import { computed, nextTick, onMounted, ref } from 'vue'
import { getJson, requestJson } from '../api.js'
import { handleAuth, toast } from '../store.js'

const ROLES = ['user', 'friend', 'admin', 'owner']
const ROLE_LABELS = { user: '普通用户', friend: '好友', admin: '管理员', owner: '主人' }
const SOURCE_LABELS = { hall: '大厅成员', friend: '好友私聊' }
const SOURCE_TABS = [
  { value: 'all', label: '全部来源' },
  { value: 'hall', label: '仅大厅' },
  { value: 'friend', label: '仅好友' },
]

const catalog = ref(null)
const replies = ref(null)
const catalogLoading = ref(false)
const replyLoading = ref(false)
const catalogError = ref('')
const replyError = ref('')
const policyError = ref('')
const policyNotice = ref('')
const replyNotice = ref('')
const fetchedAt = ref('')
const searchText = ref('')
const sourceFilter = ref('all')
const showHidden = ref(false)
const activeCategory = ref('all')
const selectedId = ref('')
const collapsed = ref(new Set())
const expanded = ref(new Set())
const policyDraft = ref({})
const replyDraft = ref({})
const defaultSourceDraft = ref(null)
const replyBaseRevision = ref(null)
const savingPolicy = ref(false)
const savingReplies = ref(false)
const lastFocusedRow = ref(0)
const replyInputs = ref([])

const saving = computed(() => savingPolicy.value || savingReplies.value)
const sourceTabs = SOURCE_TABS

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

const aliasOwners = computed(() => {
  const map = {}
  for (const item of replies.value?.aliases || []) {
    if (item && item.id != null && item.owner != null) map[String(item.id)] = String(item.owner)
  }
  return map
})

function canonicalId(id) {
  const key = String(id == null ? '' : id)
  return aliasOwners.value[key] || key
}

const replyEntries = computed(() => {
  const map = {}
  for (const entry of replies.value?.commands || []) {
    if (entry && entry.id != null) map[String(entry.id)] = entry
  }
  return map
})

const selectedEntry = computed(() => (selectedId.value ? replyEntries.value[selectedId.value] || null : null))

const replyRows = computed(() => {
  const entry = selectedEntry.value
  if (!entry) return []
  const draft = replyDraft.value[String(entry.id)]
  if (Array.isArray(draft)) return draft
  return Array.isArray(entry.replies) ? entry.replies : []
})

const maxReplyChars = computed(() => {
  const limit = Number(replies.value?.limits?.maxReplyChars)
  return Number.isFinite(limit) && limit > 0 ? limit : 300
})

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

const policyDirtyCount = computed(
  () => Object.keys(policyDraft.value).length + (defaultSourceDraft.value !== null ? 1 : 0),
)
const replyDirtyCount = computed(() => Object.keys(replyDraft.value).length)
// 可批量恢复的命令数：有 defaultReplies 且可编辑的条目（与单命令「恢复内置回复」同源）。
const restorableReplyCount = computed(() => {
  const list = Array.isArray(replies.value?.commands) ? replies.value.commands : []
  return list.filter(
    (entry) => entry && entry.editable !== false && Array.isArray(entry.defaultReplies) && entry.defaultReplies.length,
  ).length
})

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

const totalCount = computed(() =>
  (catalog.value?.groups || []).reduce(
    (sum, group) => sum + (Array.isArray(group.commands) ? group.commands.length : 0),
    0,
  ),
)

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

function roleLabel(value) {
  return ROLE_LABELS[value] || value || '普通用户'
}

function sourceLabels(item) {
  const authorities = Array.isArray(item.authorities)
    ? item.authorities.map((value) => String(value).toLowerCase()).filter(Boolean)
    : []
  if (!authorities.length) return ['大厅成员 / 好友私聊']
  return authorities.map((value) => SOURCE_LABELS[value] || value)
}

function itemKey(item) {
  return String(item?.id || `${item?.prefix || ''}${item?.name || ''}`)
}

function isSelected(item) {
  return Boolean(item && item.id != null && canonicalId(item.id) === selectedId.value)
}

function hasRoleFloor(item) {
  return Boolean(item.minimumFloor) && item.minimumFloor !== 'user'
}

function roleOptions(item) {
  const floorIndex = Math.max(0, ROLES.indexOf(item.minimumFloor || 'user'))
  return ROLES.map((value, index) => ({
    value,
    label: ROLE_LABELS[value],
    disabled: saving.value || index < floorIndex,
  }))
}

function commandLines(values) {
  const list = Array.isArray(values) ? values : values ? [values] : []
  return list.map((value) => String(value == null ? '' : value).trim()).filter(Boolean)
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

/* ---------- 载入 ---------- */
function selectCommand(itemOrId) {
  const id = typeof itemOrId === 'object' && itemOrId ? itemOrId.id : itemOrId
  if (id == null) return
  selectedId.value = canonicalId(id)
  replyNotice.value = ''
}

function ensureSelection() {
  if (selectedId.value) {
    const canonical = canonicalId(selectedId.value)
    if (replyEntries.value[canonical]) {
      selectedId.value = canonical
      return
    }
    // 回复目录尚未加载：先保留当前选中，等别名表到达后再归一化。
    if (!replies.value) return
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
  if (!selectedId.value && replies.value?.commands?.length) {
    selectCommand(replies.value.commands[0].id)
  }
}

async function loadCatalog(force = false) {
  if (catalogLoading.value) return
  if (!force && policyDirtyCount.value > 0) return
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

async function loadReplies(force = false) {
  if (replyLoading.value) return
  if (!force && replyDirtyCount.value > 0) return
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

async function refreshCatalog() {
  if (policyDirtyCount.value > 0) {
    toast('有未保存的权限/平台草稿，请先保存或放弃', 'err')
    return
  }
  await loadCatalog(true)
}

async function refreshReplies() {
  if (replyDirtyCount.value > 0) {
    toast('有未保存的回复草稿，请先保存或放弃', 'err')
    return
  }
  await loadReplies(true)
}

/* ---------- 权限草稿 ---------- */
function policyValue(item) {
  return policyDraft.value[String(item.id)] || {
    enabled: item.enabled !== false,
    minimumRole: item.minimumRole || 'user',
  }
}

function isPolicyDirty(item) {
  return Object.prototype.hasOwnProperty.call(policyDraft.value, String(item.id))
}

function setPolicyField(item, patch) {
  if (!item || item.editable !== true || saving.value) return
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

function discardPolicy() {
  if (!policyDirtyCount.value || saving.value) return
  if (!window.confirm('放弃未保存的命令权限/平台修改并重新读取？')) return
  policyDraft.value = {}
  defaultSourceDraft.value = null
  policyError.value = ''
  policyNotice.value = ''
  loadCatalog(true)
}

async function savePolicy() {
  if (!policyDirtyCount.value || saving.value) return
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
  } catch (error) {
    if (handleAuth(error)) return
    policyError.value = String(error.message || '').includes('配置已变化')
      ? '配置版本已变化，草稿已保留。请放弃草稿重新读取后再编辑保存。'
      : `保存失败：${error.message || error}`
  } finally {
    savingPolicy.value = false
  }
}

/* ---------- 回复草稿 ---------- */
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

function onReplyInput(index, event) {
  const target = draftRowsForEdit()
  if (!target || saving.value) return
  const rows = target.rows.slice()
  rows[index] = { ...rows[index], text: event.target.value }
  commitRows(target.id, rows)
}

function setRowActive(index, active) {
  const target = draftRowsForEdit()
  if (!target || saving.value) return
  const rows = target.rows.slice()
  if (!rows[index]) return
  rows[index] = { ...rows[index], active }
  commitRows(target.id, rows)
}

function moveRow(index, delta) {
  const target = draftRowsForEdit()
  if (!target || saving.value) return
  const rows = target.rows.slice()
  const to = index + delta
  if (to < 0 || to >= rows.length) return
  ;[rows[index], rows[to]] = [rows[to], rows[index]]
  commitRows(target.id, rows)
}

function removeRow(index) {
  const target = draftRowsForEdit()
  if (!target || saving.value) return
  if (target.rows[index]?.defaultText != null) return
  const rows = target.rows.slice()
  rows.splice(index, 1)
  commitRows(target.id, rows)
}

function addRow() {
  const target = draftRowsForEdit()
  if (!target || saving.value) return
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

function restoreDefaults() {
  const entry = selectedEntry.value
  if (!entry || entry.editable === false || saving.value) return
  if (!window.confirm('将此命令恢复为全部内置回复？当前自定义回复和顺序会被替换。')) return
  const rows = (Array.isArray(entry.defaultReplies) ? entry.defaultReplies : []).map((row) => ({
    ...row,
    text: row.defaultText != null ? row.defaultText : row.text,
    active: true,
  }))
  commitRows(String(entry.id), rows)
}

// 「全部恢复内置回复」：旧页只有单命令恢复；这里按 /commands/replies 的
// defaultReplies 把所有可编辑命令套成默认草稿，仍走同一个保存接口（不新增 API）。
function restoreAllReplyDefaults() {
  if (saving.value || !replies.value) return
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

function muteCommand() {
  const entry = selectedEntry.value
  if (!entry || entry.editable === false || saving.value) return
  if (!window.confirm('确定静音此命令？它将不再发送任何回复。')) return
  commitRows(String(entry.id), [])
}

function setReplyInput(index, element) {
  replyInputs.value[index] = element || null
}

function insertVariable(name) {
  const target = draftRowsForEdit()
  if (!target || saving.value) return
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

function discardReplies() {
  if (!replyDirtyCount.value || saving.value) return
  if (!window.confirm('放弃所有未保存回复并重新加载？')) return
  replyDraft.value = {}
  replyBaseRevision.value = null
  replyError.value = ''
  replyNotice.value = ''
  loadReplies(true)
}

async function saveReplies() {
  if (!replyDirtyCount.value || saving.value) return
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
  } catch (error) {
    if (handleAuth(error)) return
    replyError.value = String(error.message || '').includes('回复配置已变化')
      ? '回复配置已变化；草稿已保留，请点「放弃回复草稿」重新加载后再编辑。'
      : `回复保存失败：${error.message || error}`
  } finally {
    savingReplies.value = false
  }
}

onMounted(() => {
  loadCatalog()
  loadReplies()
})
</script>
