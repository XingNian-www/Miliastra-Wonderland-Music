<template>
  <div id="page-config">
    <div v-if="loading" class="loading-card">正在载入配置…</div>
    <div v-else-if="loadError" class="config-message err">{{ loadError }}</div>
    <template v-else>
      <div class="config-status">
        <span class="chip"><span>版本</span><em>{{ state.revision == null ? '—' : state.revision }}</em></span>
        <span class="chip"><span>配置段</span><em>{{ sections.length }}</em></span>
        <span class="chip">
          <span>未保存</span>
          <em :class="{ 'config-dirty': dirtyCount > 0 }">{{ dirtyCount }} 段</em>
        </span>
        <span class="config-field-hint" style="margin-left: auto">上次保存：{{ savedAt || '—' }}</span>
      </div>

      <div class="config-layout">
        <!-- 左侧段导航：按 domain 分组 + 搜索 -->
        <aside class="config-sidebar" aria-label="配置段导航">
          <input
            v-model="searchText"
            type="text"
            placeholder="搜索段 / 字段（名称、路径、说明）"
            style="width: 180px; flex: 0 0 auto; margin-bottom: 6px"
            aria-label="搜索配置"
          />
          <div
            v-for="domain in domains"
            :key="domain.key"
            class="config-domain"
            :class="{ open: domain.open }"
          >
            <button
              type="button"
              class="config-domain-head"
              :aria-expanded="domain.open ? 'true' : 'false'"
              @click="toggleDomain(domain.key)"
            >
              <span v-html="highlight(domain.label)"></span>
              <span style="display: inline-flex; align-items: center; gap: 4px">
                <span class="config-domain-count">{{ domain.sections.length }}</span>
                <span v-if="domain.dirty" class="dirty-dot" title="该域有未保存修改"></span>
              </span>
            </button>
            <div class="config-domain-body">
              <button
                v-for="section in domain.sections"
                :key="section.name"
                type="button"
                class="config-section-item"
                :class="{ active: section.name === activeSectionName }"
                :aria-current="section.name === activeSectionName ? 'true' : undefined"
                @click="selectSection(section.name)"
              >
                <span v-html="highlight(section.label)"></span>
                <span style="display: inline-flex; align-items: center; gap: 6px; flex: none">
                  <span class="config-domain-count" :title="`${sectionFieldCount(section)} 个字段`">
                    {{ sectionFieldCount(section) }}
                  </span>
                  <span class="section-name">{{ section.name }}</span>
                  <span v-if="state.dirty[section.name]" class="dirty-dot" title="有未保存修改"></span>
                </span>
              </button>
            </div>
          </div>
          <div v-if="!domains.length" class="config-field-hint" style="padding: 8px">
            没有匹配的配置段
          </div>
        </aside>

        <div class="config-main">
          <div class="config-toolbar">
            <button type="button" class="btn" :disabled="busy" @click="reload">重新载入</button>
            <button type="button" class="btn" :disabled="busy" @click="openHistory">历史版本</button>
            <button
              type="button"
              class="btn"
              :disabled="busy || !activeSection"
              title="按 schema 默认值重建当前段（仅填入表单，仍需保存）"
              @click="restoreSectionDefaults"
            >
              恢复本段默认
            </button>
            <span v-if="searchQuery" class="config-search-count">
              搜索命中 {{ matchedFieldCount }} 个字段
            </span>
          </div>

          <div v-if="activeSection" class="config-section-head">
            <h2>{{ activeSection.label }}</h2>
            <code>{{ activeSection.name }}</code>
            <span class="config-section-stats">
              {{ visibleFields.length }} 项
              <span
                v-for="item in effectSummary"
                :key="item.key"
                class="config-effect"
                :class="item.key"
              >
                {{ item.label }} {{ item.count }}
              </span>
            </span>
          </div>

          <form v-if="activeSection" id="configForm" class="config-form" @submit.prevent="save">
            <div v-for="group in groups" :key="group.key" class="config-group-card">
              <div v-if="group.name" class="config-group-head">{{ group.name }}</div>
              <div
                v-for="field in group.fields"
                :key="field.path"
                class="config-field"
                :class="{
                  bootstrap: field.source === 'bootstrap',
                  err: !!errorFor(field),
                }"
                :data-kind="kindClass(field)"
                :data-cfg-field="field.path"
              >
                <div class="config-field-label">
                  <b v-html="highlight(field.label)"></b>
                  <code :title="field.path">{{ field.path }}</code>
                </div>
                <div class="config-field-control">
                  <FieldControl
                    :field="field"
                    :model-value="getFieldValue(field)"
                    :disabled="isDisabled(field)"
                    @update:model-value="setFieldValue(field, $event)"
                    @invalid="setInvalid(field, $event)"
                  />
                </div>
                <div class="config-field-side">
                  <span v-if="field.source === 'bootstrap'" class="config-effect bootstrap">
                    config.yaml
                  </span>
                  <span v-else class="config-effect" :class="effectClass(field)">
                    {{ effectLabel(field) }}
                  </span>
                  <button
                    v-if="hasDefault(field)"
                    type="button"
                    class="btn small ghost"
                    :disabled="busy"
                    title="将本字段恢复为 schema 默认值（尚未保存）"
                    @click="resetField(field)"
                  >
                    恢复默认
                  </button>
                </div>
                <div class="config-field-foot">
                  <span v-if="field.hint" class="config-field-hint">{{ field.hint }}</span>
                  <span v-if="hasDefault(field)" class="config-field-default">
                    默认: {{ defaultText(field) }}
                  </span>
                  <span v-if="field.source === 'bootstrap'" class="config-readonly-note">
                    由 config.yaml 提供，修改后重启生效
                  </span>
                  <FieldError :message="errorFor(field)" />
                </div>
              </div>
            </div>
            <div v-if="!visibleFields.length" class="config-empty">没有匹配的配置字段</div>

            <!-- 表情段特判：全量表情目录 -->
            <EmojiCatalog
              v-if="activeSection.name === 'emoji'"
              :allowed="emojiAllowed"
              :can-add="!!emojiAllowedField"
              :disabled="busy"
              @update:allowed="onEmojiAllowed"
            />
          </form>

          <div v-if="activeSection" class="config-savebar">
            <span class="config-savebar-hint" :class="{ 'config-dirty': dirtyCount > 0 }">
              {{ dirtyCount ? `有未保存的修改（${dirtyCount} 段）` : '未修改' }}
            </span>
            <button type="button" class="btn" :disabled="busy || !dirtyCount" @click="discardChanges">
              放弃修改
            </button>
            <button
              type="button"
              class="btn primary"
              :disabled="busy || !dirtyCount || hasLocalErrors"
              @click="save"
            >
              {{ saving ? '保存中…' : '保存修改' }}
            </button>
          </div>

          <details
            id="configRevisionsPanel"
            ref="historyEl"
            class="config-group-card"
            style="margin-top: 16px"
            @toggle="onHistoryToggle"
          >
            <summary class="config-group-head" style="cursor: pointer">历史版本与回滚</summary>
            <div style="padding: 12px">
              <div v-if="revisionsError" class="config-message err">{{ revisionsError }}</div>
              <div v-else-if="revisionsLoading" class="config-field-hint">正在读取历史版本…</div>
              <div v-else-if="!revisions.length" class="config-field-hint">暂无历史版本</div>
              <div
                v-else
                class="history"
                style="display: grid; gap: 8px; max-height: 260px; overflow: auto"
              >
                <div v-for="item in revisions" :key="item.revision" class="history-item">
                  <div>
                    <span class="history-time">#{{ item.revision }}</span>
                    <b>{{ formatTime(item.createdAtMs) }}</b>
                    <span
                      v-if="Number(item.revision) === Number(state.revision)"
                      class="field-state ok"
                      style="margin-left: 8px"
                    >
                      当前
                    </span>
                  </div>
                  <div class="history-result">
                    <button
                      type="button"
                      class="btn small"
                      :disabled="busy || Number(item.revision) === Number(state.revision)"
                      @click="rollback(item)"
                    >
                      回滚到此版本
                    </button>
                  </div>
                </div>
              </div>
            </div>
          </details>
        </div>
      </div>
    </template>
  </div>
</template>

<script setup>
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'
import { getJson, requestJson } from '../api.js'
import {
  clearDirty,
  handleAuth,
  isDirty,
  loadConfig,
  markDirty,
  nestedCloneSet,
  nestedGet,
  saveSections,
  state,
  toast,
} from '../store.js'
import FieldControl from '../components/FieldControl.vue'
import FieldError from '../components/FieldError.vue'
import EmojiCatalog from '../components/EmojiCatalog.vue'

// 配置中心的 IA 与旧版一致：身份映射、AI 配置（ai/ai_chat/tts）有独立页面，
// command_access 由命令面板编辑，均不在配置中心重复出现。
const HIDDEN_SECTIONS = new Set(['identity', 'ai', 'ai_chat', 'tts', 'command_access'])
const DOMAIN_ORDER = ['recognition', 'playback', 'interaction', 'entertainment', 'system']
const DOMAIN_LABELS = {
  recognition: '识别与画面',
  playback: '播放与点歌',
  interaction: 'AI 与交互',
  entertainment: '娱乐玩法',
  system: '系统',
}
const EFFECT_LABELS = {
  live: '立即生效',
  idleReload: '任务结束后重载',
  playbackIdleReload: '播放结束/暂停后重载',
  restart: '重启生效',
}
const EFFECT_CLASSES = {
  live: 'live',
  idleReload: 'idle-reload',
  playbackIdleReload: 'playback-idle-reload',
  restart: 'restart',
}
const RECT_LABELS = { x: 'X', y: 'Y', width: '宽', height: '高' }

// 组件卸载（切到别的页面）时缓存草稿，回来还能接着改。
let draftCache = { revision: null, draft: null }

const loading = ref(!state.config)
const loadError = ref('')
const saving = ref(false)
const searchText = ref('')
const activeSectionName = ref('')
const draft = reactive({})
const localErrors = ref({})
const domainOpen = reactive({})
const savedAt = ref('')

const historyEl = ref(null)
const revisions = ref([])
const revisionsLoading = ref(false)
const revisionsError = ref('')

const searchQuery = computed(() => searchText.value.trim().toLowerCase())
const sections = computed(() =>
  (Array.isArray(state.schema) ? state.schema : []).filter((section) => !HIDDEN_SECTIONS.has(section.name)),
)
const dirtyCount = computed(() => Object.keys(state.dirty).length)
const hasLocalErrors = computed(() => Object.keys(localErrors.value).length > 0)
const busy = computed(() => state.busy || saving.value)

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

const emojiAllowedField = computed(() => {
  const section = activeSection.value
  if (!section || section.name !== 'emoji') return null
  return (section.fields || []).find((field) => field.path === 'emoji.ai_allowed') || null
})

const emojiAllowed = computed(() => {
  const field = emojiAllowedField.value
  if (!field) return []
  const value = getFieldValue(field)
  return Array.isArray(value) ? value : []
})

watch(visibleSections, (list) => {
  if (!list.length) return
  if (!list.some((section) => section.name === activeSectionName.value)) {
    activeSectionName.value = list[0].name
  }
})

// schema 可能已在别的页面载入过：setup 阶段就先选好当前段，避免首帧空表单。
if (!activeSectionName.value && visibleSections.value.length) selectInitialSection()

/* ---------- 载入 / 草稿 ---------- */
function deepClone(value) {
  if (value === undefined) return undefined
  if (value === null) return null
  return JSON.parse(JSON.stringify(value))
}

function applyDraft(source) {
  for (const key of Object.keys(draft)) delete draft[key]
  for (const [name, value] of Object.entries(source || {})) draft[name] = deepClone(value)
  for (const section of sections.value) {
    if (!(section.name in draft)) draft[section.name] = {}
  }
}

function snapshotDraft() {
  const out = {}
  for (const [name, value] of Object.entries(draft)) out[name] = deepClone(value)
  return out
}

function selectInitialSection() {
  const list = visibleSections.value
  if (!list.length) return
  let remembered = ''
  try {
    remembered = localStorage.getItem('miliastraConfigSection') || ''
  } catch (error) {
    remembered = ''
  }
  const match =
    list.find((section) => section.name === remembered) ||
    list.find((section) => section.name !== 'visual') ||
    list[0]
  activeSectionName.value = match.name
}

function selectSection(name) {
  activeSectionName.value = name
  try {
    localStorage.setItem('miliastraConfigSection', name)
  } catch (error) {
    /* 忽略隐私模式下的存储失败 */
  }
}

function toggleDomain(key) {
  domainOpen[key] = !domainOpen[key]
}

/* ---------- 字段值读写 ---------- */
function getFieldValue(field) {
  const section = activeSection.value
  if (!section) return null
  const segments = field.path.split('.').slice(1)
  const value = nestedGet(draft[section.name], segments.join('.'))
  if (value !== undefined) return value
  if (field.default !== undefined) return field.default
  return null
}

function setFieldValue(field, value) {
  const section = activeSection.value
  if (!section) return
  if (!draft[section.name] || typeof draft[section.name] !== 'object') draft[section.name] = {}
  const segments = field.path.split('.').slice(1)
  nestedCloneSet(draft[section.name], segments.join('.'), value)
  markDirty(section.name)
  clearLocalError(field.path)
  if (state.fieldErrors[field.path]) {
    const next = { ...state.fieldErrors }
    delete next[field.path]
    state.fieldErrors = next
  }
}

function setInvalid(field, message) {
  const next = { ...localErrors.value }
  if (message) next[field.path] = message
  else delete next[field.path]
  localErrors.value = next
}

function clearLocalError(path) {
  if (!localErrors.value[path]) return
  const next = { ...localErrors.value }
  delete next[path]
  localErrors.value = next
}

function errorFor(field) {
  return state.fieldErrors[field.path] || localErrors.value[field.path] || ''
}

function isDisabled(field) {
  return busy.value || field.source === 'bootstrap'
}

function hasDefault(field) {
  return field.default !== undefined && field.default !== null && field.source !== 'bootstrap'
}

/* ---------- 展示辅助 ---------- */
function sectionFieldCount(section) {
  return Array.isArray(section.fields) ? section.fields.length : 0
}

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

function escapeHtml(text) {
  return String(text == null ? '' : text).replace(/[&<>"']/g, (char) => {
    const map = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }
    return map[char]
  })
}

function escapeRegExp(text) {
  return String(text).replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
}

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

function kindClass(field) {
  const type = field.kind ? field.kind.type : 'string'
  const map = {
    int: 'number',
    float: 'number',
    string: 'path',
    path: 'path',
    text: 'text',
    bool: 'bool',
    enum: 'enum',
    object: 'object',
    stringArray: 'array',
    array: 'array',
    rect: 'rect',
    point: 'point',
    secret: 'secret',
    modelName: 'string',
  }
  return map[type] || 'other'
}

function effectLabel(field) {
  return EFFECT_LABELS[field.effect] || EFFECT_LABELS.restart
}

function effectClass(field) {
  return EFFECT_CLASSES[field.effect] || EFFECT_CLASSES.restart
}

function defaultText(field) {
  let text
  if (typeof field.default === 'string') text = field.default
  else text = JSON.stringify(field.default)
  if (text == null) text = String(field.default)
  return text.length > 48 ? `${text.slice(0, 48)}…` : text
}

function formatNow() {
  return new Date().toLocaleString('zh-CN', { hour12: false })
}

function formatTime(ms) {
  if (ms == null || ms === '') return '时间未知'
  return new Date(Number(ms)).toLocaleString('zh-CN', { hour12: false })
}

/* ---------- 重新载入 / 放弃 ---------- */
async function reload() {
  if (busy.value) return
  if (isDirty() && !window.confirm('重新载入会丢弃全部未保存的修改，继续？')) return
  try {
    await loadConfig(true)
    clearDirty()
    state.fieldErrors = {}
    localErrors.value = {}
    applyDraft(state.config ? state.config.sections : {})
    toast('已重新载入配置')
  } catch (error) {
    if (!handleAuth(error)) toast(`重新载入失败：${error.message || error}`, 'err')
  }
}

function discardChanges() {
  if (!dirtyCount.value) return
  if (!window.confirm('放弃当前未保存的修改？')) return
  clearDirty()
  state.fieldErrors = {}
  localErrors.value = {}
  applyDraft(state.config ? state.config.sections : {})
  toast('已放弃未保存的修改')
}

function resetField(field) {
  const section = activeSection.value
  if (!section || !hasDefault(field)) return
  if (!window.confirm(`将「${field.label}」恢复为默认值（尚未保存）？`)) return
  const segments = field.path.split('.').slice(1)
  if (!draft[section.name] || typeof draft[section.name] !== 'object') draft[section.name] = {}
  nestedCloneSet(draft[section.name], segments.join('.'), deepClone(field.default))
  markDirty(section.name)
  clearLocalError(field.path)
  toast(`已将「${field.label}」恢复为默认值，记得保存`, 'ok')
}

function restoreSectionDefaults() {
  const section = activeSection.value
  if (!section) return
  if (!window.confirm(`将「${section.label}」整段按 schema 默认值重建（尚未保存）？`)) return
  const rebuilt = {}
  for (const field of section.fields || []) {
    if (field.source === 'bootstrap') continue
    const type = field.kind ? field.kind.type : 'string'
    // 密钥字段恢复默认不清空已保存的密钥：不写入该路径，保存时后端会保留当前值。
    if (type === 'secret') continue
    const segments = field.path.split('.').slice(1)
    if (segments.length > 1) {
      const parentPath = `${section.name}.${segments.slice(0, -1).join('.')}`
      const parentField = (section.fields || []).find((item) => item.path === parentPath)
      if (parentField && parentField.default == null) continue
      const parentValue = nestedGet(rebuilt, segments.slice(0, -1).join('.'))
      if (parentValue === null) continue
    }
    setNested(rebuilt, segments, field.default == null ? null : deepClone(field.default))
  }
  draft[section.name] = rebuilt
  markDirty(section.name)
  localErrors.value = {}
  toast(`已按默认值重建「${section.label}」，请检查后保存`, 'ok')
}

function setNested(target, segments, value) {
  let cursor = target
  for (const segment of segments.slice(0, -1)) {
    if (cursor[segment] == null || typeof cursor[segment] !== 'object') cursor[segment] = {}
    cursor = cursor[segment]
  }
  cursor[segments[segments.length - 1]] = value
}

function deleteNested(target, segments) {
  if (!target || typeof target !== 'object' || !segments.length) return
  let cursor = target
  for (const segment of segments.slice(0, -1)) {
    if (!cursor || typeof cursor !== 'object') return
    cursor = cursor[segment]
  }
  if (cursor && typeof cursor === 'object') delete cursor[segments[segments.length - 1]]
}

/* ---------- 保存：只提交 dirty 段 ---------- */
function buildDirtySections() {
  const names = Object.keys(state.dirty).filter((name) =>
    sections.value.some((section) => section.name === name),
  )
  if (!names.length) {
    toast('当前页没有需要保存的配置段', 'err')
    return null
  }
  const errors = {}
  const payload = {}
  for (const name of names) {
    const section = sections.value.find((item) => item.name === name)
    if (!section) continue
    const normalized = normalizeSection(section, errors)
    payload[name] = normalized
  }
  if (Object.keys(errors).length) {
    localErrors.value = errors
    const firstPath = Object.keys(errors)[0]
    toast(`有 ${Object.keys(errors).length} 个字段格式无效，请修正后再保存`, 'err')
    focusInvalidField(firstPath)
    return null
  }
  return payload
}

function focusInvalidField(path) {
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

function normalizeSection(section, errors) {
  const source = draft[section.name]
  const out = source && typeof source === 'object' ? deepClone(source) : {}
  for (const field of section.fields || []) {
    const segments = field.path.split('.').slice(1)
    if (!segments.length) continue
    if (field.source === 'bootstrap') {
      deleteNested(out, segments)
      continue
    }
    if (segments.length > 1) {
      const parentValue = nestedGet(out, segments.slice(0, -1).join('.'))
      if (parentValue === null) continue
    }
    if (nestedGet(out, segments.join('.')) === undefined) continue
    try {
      coerceField(field, out, segments)
    } catch (error) {
      errors[field.path] = error.message
    }
  }
  return out
}

function coerceField(field, out, segments) {
  const kind = field.kind ? field.kind.type : 'string'
  const value = nestedGet(out, segments.join('.'))
  const nullable = field.nullable === true

  if (kind === 'int' || kind === 'float') {
    if (value === null) {
      if (!nullable) throw new Error('不能为空')
      return
    }
    const raw = String(value).trim()
    if (raw === '') {
      if (nullable) {
        setNested(out, segments, null)
        return
      }
      throw new Error('不能为空')
    }
    const number = Number(raw)
    if (!Number.isFinite(number)) throw new Error('必须是数字')
    if (kind === 'int' && !Number.isInteger(number)) throw new Error('必须是整数')
    if (kind === 'int' && !Number.isSafeInteger(number)) throw new Error('超出安全整数范围')
    const min = field.kind ? field.kind.min : null
    const max = field.kind ? field.kind.max : null
    if (min != null && number < min) throw new Error(`不能小于 ${min}`)
    if (max != null && number > max) throw new Error(`不能大于 ${max}`)
    setNested(out, segments, number)
    return
  }

  if (kind === 'stringArray' || kind === 'array') {
    const list = Array.isArray(value) ? value : []
    const cleaned = list.map((item) => String(item == null ? '' : item).trim()).filter((item) => item !== '')
    setNested(out, segments, cleaned)
    return
  }

  if (kind === 'rect' || kind === 'point') {
    if (value === null) {
      if (!nullable) throw new Error('坐标不能为空')
      return
    }
    if (typeof value !== 'object' || Array.isArray(value)) throw new Error('坐标必须是对象')
    const keys = kind === 'point' ? ['x', 'y'] : ['x', 'y', 'width', 'height']
    const result = {}
    const missing = []
    for (const key of keys) {
      const raw = value[key]
      if (raw === null || raw === undefined || String(raw).trim() === '') {
        missing.push(key)
        continue
      }
      const number = Number(raw)
      if (!Number.isSafeInteger(number)) throw new Error(`${RECT_LABELS[key]} 必须是整数`)
      result[key] = number
    }
    if (missing.length) {
      if (!Object.keys(result).length && nullable) {
        setNested(out, segments, null)
        return
      }
      throw new Error(`缺少 ${missing.map((key) => RECT_LABELS[key]).join('、')}`)
    }
    if (kind === 'rect' && (result.width <= 0 || result.height <= 0)) {
      throw new Error('宽高必须大于 0')
    }
    setNested(out, segments, result)
    return
  }

  if (kind === 'enum') {
    if (value === null) {
      if (!nullable) throw new Error('必须选择一个值')
      return
    }
    const text = String(value)
    if (text === '') throw new Error('必须选择一个值')
    setNested(out, segments, text)
    return
  }

  if (kind === 'bool') {
    setNested(out, segments, value === true)
    return
  }

  if (kind === 'object') {
    if (value !== null && typeof value !== 'object') {
      throw new Error('必须是 JSON 对象或 null')
    }
    return
  }

  if (kind === 'secret') return

  // string / path / text / modelName
  if (value === null) {
    if (!nullable) setNested(out, segments, '')
    return
  }
  setNested(out, segments, String(value))
}

function saveMessage(response) {
  const revision =
    response && response.revision != null ? String(response.revision) : String(state.revision == null ? '—' : state.revision)
  const parts = [`已保存，当前版本 ${revision}`]
  if (response && Array.isArray(response.appliedLiveFields) && response.appliedLiveFields.length) {
    parts.push(`立即生效 ${response.appliedLiveFields.length} 项`)
  }
  if (response && Array.isArray(response.runtimeReloadFields) && response.runtimeReloadFields.length) {
    parts.push(`任务结束后重载 ${response.runtimeReloadFields.length} 项`)
  }
  if (response && Array.isArray(response.playbackReloadFields) && response.playbackReloadFields.length) {
    parts.push(`播放结束/暂停后重载 ${response.playbackReloadFields.length} 项`)
  }
  if (response && response.restartRequired) parts.push('部分字段需重启生效')
  return parts.join('；')
}

async function save() {
  if (saving.value || state.busy) return
  if (hasLocalErrors.value) {
    const firstPath = Object.keys(localErrors.value)[0]
    toast('有字段格式无效（如 JSON），请先修正再保存', 'err')
    focusInvalidField(firstPath)
    return
  }
  const payload = buildDirtySections()
  if (!payload) return
  saving.value = true
  let response = null
  try {
    const ok = await saveSections(payload, (result) => {
      response = result
    })
    if (!ok) {
      // 服务端校验失败：若错误字段不在当前段，自动跳到出错字段所在段。
      const firstError = Object.keys(state.fieldErrors)[0]
      if (firstError) focusInvalidField(firstError)
      return
    }
    clearDirty()
    state.fieldErrors = {}
    localErrors.value = {}
    savedAt.value = formatNow()
    toast(saveMessage(response), 'ok')
    try {
      await loadConfig(true)
      applyDraft(state.config ? state.config.sections : {})
    } catch (error) {
      toast(`配置已保存，但重新读取失败：${error.message || error}`, 'err')
    }
    if (historyEl.value && historyEl.value.open) await loadRevisions()
  } finally {
    saving.value = false
  }
}

/* ---------- 表情白名单 ---------- */
function onEmojiAllowed(list) {
  const field = emojiAllowedField.value
  if (!field) return
  setFieldValue(field, list)
}

/* ---------- 历史版本 ---------- */
function onHistoryToggle(event) {
  const open = event && event.target ? event.target.open : historyEl.value && historyEl.value.open
  if (open) loadRevisions()
}

function openHistory() {
  if (!historyEl.value) return
  historyEl.value.open = true
  nextTick(() => {
    if (historyEl.value && typeof historyEl.value.scrollIntoView === 'function') {
      historyEl.value.scrollIntoView({ block: 'end' })
    }
  })
}

async function loadRevisions() {
  revisionsLoading.value = true
  revisionsError.value = ''
  try {
    const data = await getJson('/config/revisions')
    const list = data && Array.isArray(data.revisions) ? data.revisions : []
    revisions.value = list
  } catch (error) {
    if (!handleAuth(error)) revisionsError.value = `读取历史版本失败：${error.message || error}`
  } finally {
    revisionsLoading.value = false
  }
}

async function rollback(item) {
  if (busy.value || !item) return
  const target = Number(item.revision)
  if (!Number.isFinite(target)) return
  if (!window.confirm(`回滚到版本 #${target}？当前未保存的修改会丢弃，回滚会记录为新版本。`)) return
  saving.value = true
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
    state.fieldErrors = {}
    localErrors.value = {}
    await loadConfig(true)
    applyDraft(state.config ? state.config.sections : {})
    savedAt.value = formatNow()
    toast(
      `已回滚到版本 #${target}，当前版本 ${response && response.revision != null ? response.revision : '—'}`,
      'ok',
    )
    await loadRevisions()
  } catch (error) {
    if (!handleAuth(error)) toast(`回滚失败：${error.message || error}`, 'err')
  } finally {
    saving.value = false
  }
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
      applyDraft(cached.draft)
    } else {
      // 其他页面已保存过新版本：旧草稿失效，同步清掉脏标记避免误报“未保存”。
      if (cached && cached.revision != null && cached.revision !== state.revision) clearDirty()
      applyDraft(state.config ? state.config.sections : {})
    }
    if (
      !activeSectionName.value ||
      !visibleSections.value.some((section) => section.name === activeSectionName.value)
    ) {
      selectInitialSection()
    }
  } catch (error) {
    if (!handleAuth(error)) loadError.value = `载入配置失败：${error.message || error}`
  } finally {
    loading.value = false
  }
})

onBeforeUnmount(() => {
  draftCache = { revision: state.revision, draft: snapshotDraft() }
  window.removeEventListener('keydown', onKeydown)
  window.removeEventListener('beforeunload', onBeforeUnload)
})
</script>
