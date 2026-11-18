<template>
  <div class="ai-config-page">
    <h1 class="page-title">AI 配置</h1>
    <p class="page-subtitle">AI 服务商、AI 聊天、识图与语音 TTS；配置页只提交改动过的段</p>

    <div class="tabs ai-main-tabs" role="tablist" aria-label="AI 配置">
      <button
        v-for="item in mainTabs"
        :key="item.key"
        class="tab"
        :class="{ active: tab === item.key }"
        type="button"
        @click="tab = item.key"
      >
        {{ item.label }}
      </button>
    </div>

    <div v-if="tab === 'config'" class="ai-config-body">
      <div class="tabs ai-section-tabs" role="tablist" aria-label="配置分组">
        <button
          v-for="item in sectionTabs"
          :key="item.name"
          class="tab"
          :class="{ active: activeSection === item.name }"
          type="button"
          @click="activeSection = item.name"
        >
          {{ item.label }}
          <span v-if="isSectionDirty(item.name)" class="dirty-dot" title="有未保存修改"></span>
        </button>
      </div>

      <div v-if="loading" class="loading-card">正在读取 AI 配置…</div>
      <div v-else-if="loadError" class="config-message err">{{ loadError }}</div>
      <div v-else-if="!currentSection" class="config-message">当前后端没有 {{ activeSection }} 配置段。</div>
      <div v-else id="configForm" class="config-form">
        <div class="config-section-head">
          <h2>{{ sectionTitle(currentSection) }}</h2>
          <code>{{ currentSection.name }}</code>
          <span class="config-section-stats">{{ currentFields.length }} 项</span>
        </div>

        <div v-for="group in groupedFields" :key="group.name" class="config-group-card">
          <div class="config-group-head">{{ group.name }}</div>
          <div
            v-for="field in group.fields"
            :key="field.path"
            class="config-field"
            :class="{ err: !!errorFor(field) }"
            :data-kind="kindOf(field)"
            :data-cfg-field="field.path"
          >
            <div class="config-field-label">
              <b>{{ field.label || field.path }}</b>
              <code :title="field.path">{{ field.path }}</code>
            </div>
            <div class="config-field-control">
              <AiModelPicker
                v-if="isChatModelField(field)"
                :field="field"
                :model-value="fieldValue(field)"
                :overrides="modelOverrides"
                @update:model-value="(value) => updateField(field, value)"
              />
              <div v-else-if="isTtsField(field)" class="ai-tts-picker">
                <div class="model-picker">
                  <FieldControl
                    :field="field"
                    :model-value="fieldValue(field)"
                    @update:model-value="(value) => updateField(field, value)"
                    @invalid="(message) => setInvalid(field, message)"
                  />
                  <select
                    :disabled="ttsCatalog.loading || !ttsOptionsFor(field).length"
                    @change="pickTtsOption(field, $event)"
                  >
                    <option value="">{{ ttsOptionsFor(field).length ? '从目录选择…' : '（尚未获取目录）' }}</option>
                    <option v-for="option in ttsOptionsFor(field)" :key="option.value" :value="option.value">{{ option.label }}</option>
                  </select>
                  <button class="btn small ghost" type="button" :disabled="ttsCatalog.loading" @click="loadTtsCatalog">
                    {{ ttsCatalog.loading ? '获取中…' : '获取 TTS 目录' }}
                  </button>
                </div>
                <span class="config-field-hint">{{ ttsStatusText(field) }}</span>
              </div>
              <FieldControl
                v-else
                :field="field"
                :model-value="fieldValue(field)"
                @update:model-value="(value) => updateField(field, value)"
                @invalid="(message) => setInvalid(field, message)"
              />
            </div>
            <div class="config-field-side">
              <span class="config-effect" :class="effectClass(field)">{{ effectText(field.effect) }}</span>
              <button
                v-if="hasDefault(field)"
                class="btn small ghost"
                type="button"
                title="恢复为 schema 默认值（尚未保存）"
                @click="resetField(field)"
              >
                恢复默认
              </button>
            </div>
            <div class="config-field-foot">
              <span v-if="field.hint" class="config-field-hint">{{ field.hint }}</span>
              <span v-if="hasDefault(field)" class="config-field-default">默认: {{ defaultText(field) }}</span>
              <FieldError :message="errorFor(field)" />
            </div>
          </div>
        </div>

        <div v-if="activeSection === 'ai_chat'" class="config-vision-note ai-vision-status">
          <div class="ai-vision-head">
            <strong>识图运行状态</strong>
            <button class="btn small ghost" type="button" :disabled="visionState.loading" @click="loadVisionStatus(true)">
              {{ visionState.loading ? '读取中…' : '刷新状态' }}
            </button>
          </div>
          <div v-if="visionState.error" class="config-field-hint" style="color: var(--danger)">{{ visionState.error }}</div>
          <div v-else-if="visionState.data" class="ai-vision-grid">
            <span>状态：{{ visionState.data.disabled ? '未启用（降级纯文本）' : (visionState.data.enabled ? '已启用' : '未启用') }}</span>
            <span>模型：<code>{{ visionState.data.model || '—' }}</code></span>
            <span>能力：{{ visionCapabilityText(visionState.data) }}</span>
            <span>来源：{{ visionState.data.source || '—' }}</span>
            <span>截图范围：{{ visionState.data.capture_scope || '—' }}</span>
            <span>整屏：{{ visionState.data.allow_full_screen ? '允许' : '不允许' }}</span>
            <span v-if="visionState.data.reason">原因：{{ visionState.data.reason }}</span>
          </div>
          <div v-else class="config-field-hint">点击「刷新状态」查询当前模型是否支持图片。</div>
        </div>

        <div class="config-savebar">
          <span class="config-savebar-hint">
            {{ dirtySections.length ? `有未保存修改：${dirtySectionLabels}` : '没有未保存修改' }}
          </span>
          <button class="btn" type="button" :disabled="saving" @click="reload">重新读取</button>
          <button class="btn primary" type="button" :disabled="saving || !dirtySections.length" @click="save">
            {{ saving ? '保存中…' : '保存改动' }}
          </button>
        </div>
      </div>

      <AiPromptPanel v-if="!loading && !loadError" :sections="sections" />
      <AiTagHelp />
    </div>

    <AiMemoryPanel v-else-if="tab === 'memory'" />
    <div v-else-if="tab === 'trace'" class="ai-trace-tab">
      <AiTracePanel />
      <AiToolPlayground />
    </div>
  </div>
</template>

<script setup>
import { computed, onMounted, reactive, ref, watch } from 'vue'
import { requestJson } from '../api.js'
import { clearDirty, nestedCloneSet, nestedGet, saveSections, state, loadConfig, toast } from '../store.js'
import { stripSecretMask } from '../util/format.js'
import FieldControl from '../components/FieldControl.vue'
import FieldError from '../components/FieldError.vue'
import AiMemoryPanel from '../components/AiMemoryPanel.vue'
import AiTracePanel from '../components/AiTracePanel.vue'
import AiPromptPanel from '../components/AiPromptPanel.vue'
import AiModelPicker from '../components/AiModelPicker.vue'
import AiToolPlayground from '../components/AiToolPlayground.vue'
import AiTagHelp from '../components/AiTagHelp.vue'

const mainTabs = [
  { key: 'config', label: '配置' },
  { key: 'memory', label: '记忆' },
  { key: 'trace', label: '请求记录' },
]

const sectionTabs = [
  { name: 'ai', label: 'AI 服务商' },
  { name: 'ai_chat', label: 'AI 聊天' },
  { name: 'tts', label: 'TTS' },
]

const tab = ref('config')
const activeSection = ref('ai')
const loading = ref(true)
const saving = ref(false)
const loadError = ref('')
const sections = reactive({ ai: {}, ai_chat: {}, tts: {} })
const localErrors = ref({})
const ttsCatalog = reactive({ models: [], voices: [], source: '', modelSource: '', loaded: false, loading: false, error: '' })
const visionState = reactive({ loading: false, error: '', data: null, fetchedAt: 0 })
let baseline = { ai: {}, ai_chat: {}, tts: {} }

function clone(value) {
  return JSON.parse(JSON.stringify(value == null ? {} : value))
}

function schemaSection(name) {
  return (state.schema || []).find((section) => section.name === name) || null
}

const currentSection = computed(() => schemaSection(activeSection.value))
const currentFields = computed(() => (currentSection.value && currentSection.value.fields) || [])

const groupedFields = computed(() => {
  const groups = new Map()
  for (const field of currentFields.value) {
    const name = String(field.group || '').trim() || '其他'
    if (!groups.has(name)) groups.set(name, [])
    groups.get(name).push(field)
  }
  return [...groups.entries()].map(([name, fields]) => ({ name, fields }))
})

const dirtySections = computed(() =>
  sectionTabs
    .map((item) => item.name)
    .filter((name) => JSON.stringify(sections[name]) !== JSON.stringify(baseline[name]))
)

const dirtySectionLabels = computed(() =>
  dirtySections.value
    .map((name) => (sectionTabs.find((item) => item.name === name) || { label: name }).label)
    .join(' / ')
)

function resetDraft() {
  const source = state.config && state.config.sections ? state.config.sections : {}
  for (const item of sectionTabs) {
    const value = source[item.name] && typeof source[item.name] === 'object' ? source[item.name] : {}
    sections[item.name] = clone(value)
    baseline[item.name] = clone(value)
  }
  syncStoreDirty()
}

// state.dirty 是共享 store；只维护本页三段，避免把已改回原值的段留成脏段。
function syncStoreDirty() {
  const next = { ...state.dirty }
  for (const item of sectionTabs) {
    if (dirtySections.value.includes(item.name)) next[item.name] = true
    else delete next[item.name]
  }
  state.dirty = next
}

async function load(force = true) {
  loading.value = true
  loadError.value = ''
  try {
    await loadConfig(force)
    localErrors.value = {}
    state.fieldErrors = {}
    resetDraft()
    if (activeSection.value === 'ai_chat') loadVisionStatus(true)
  } catch (error) {
    loadError.value = `读取配置失败：${error.message || error}`
    toast(loadError.value, 'err')
  } finally {
    loading.value = false
  }
}

function isSectionDirty(name) {
  return dirtySections.value.includes(name)
}

function sectionTitle(section) {
  const item = sectionTabs.find((entry) => entry.name === section.name)
  return item ? item.label : section.label || section.name
}

function pathSegments(field) {
  return String(field.path).split('.')
}

function sectionNameFor(field) {
  return pathSegments(field)[0]
}

function fieldValue(field) {
  const section = sections[sectionNameFor(field)]
  const rest = pathSegments(field).slice(1).join('.')
  const value = rest ? nestedGet(section, rest) : section
  // 与 ConfigPage 一致：缺失值先按 schema 默认展示，但不写入草稿；
  // 只有用户真正改动该字段时才会被提交。
  if (value !== undefined) return value
  if (field.default !== undefined) return field.default
  return null
}

function updateField(field, value) {
  const sectionName = sectionNameFor(field)
  if (!sections[sectionName] || typeof sections[sectionName] !== 'object') sections[sectionName] = {}
  const rest = pathSegments(field).slice(1).join('.')
  if (!rest) {
    sections[sectionName] = value
    syncStoreDirty()
    return
  }
  nestedCloneSet(sections[sectionName], rest, value)
  syncStoreDirty()
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

function hasDefault(field) {
  return field.default !== undefined && field.default !== null && field.source !== 'bootstrap'
}

function resetField(field) {
  if (!hasDefault(field)) return
  if (!window.confirm(`将「${field.label || field.path}」恢复为 schema 默认值（尚未保存）？`)) return
  updateField(field, clone(field.default))
}

function kindOf(field) {
  return (field.kind && field.kind.type) || 'string'
}

function defaultText(field) {
  if (field.default === undefined) return '—'
  if (field.default === null) return 'null'
  if (typeof field.default === 'string') return field.default || '（空）'
  if (typeof field.default === 'object') {
    try {
      const text = JSON.stringify(field.default)
      return text.length > 64 ? `${text.slice(0, 64)}…` : text
    } catch {
      return '—'
    }
  }
  return String(field.default)
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

function effectText(effect) {
  return EFFECT_LABELS[effect] || '重启生效'
}

function effectClass(field) {
  return EFFECT_CLASSES[field.effect] || 'restart'
}

/* ---------- AI 模型下拉（POST /ai/models） ---------- */
const CHAT_MODEL_PATHS = new Set(['ai.model', 'ai_chat.model'])
const TTS_FIELD_PATHS = new Set(['tts.model', 'tts.voice', 'tts.reference_id'])

const modelOverrides = computed(() => {
  const ai = sections.ai || {}
  const out = {}
  for (const key of ['provider', 'endpoint', 'responses_endpoint', 'http_proxy', 'wire_api']) {
    if (ai[key] === undefined || ai[key] === null) continue
    out[key] = ai[key]
  }
  if (typeof ai.api_key === 'string' && ai.api_key) out.api_key = ai.api_key
  return out
})

function isChatModelField(field) {
  return CHAT_MODEL_PATHS.has(field.path)
}

/* ---------- TTS 模型/音色目录（POST /tts/models） ---------- */
function isTtsField(field) {
  return TTS_FIELD_PATHS.has(field.path)
}

function ttsOptionsFor(field) {
  if (field.path === 'tts.model') {
    return (ttsCatalog.models || []).map((model) => ({ value: model, label: model }))
  }
  return (ttsCatalog.voices || [])
    .filter((voice) => voice && voice.id)
    .map((voice) => ({ value: voice.id, label: `${voice.title || voice.id} · ${voice.id}` }))
}

function pickTtsOption(field, event) {
  const value = event.target.value
  if (!value) return
  updateField(field, value)
  event.target.value = ''
}

function ttsStatusText(field) {
  if (ttsCatalog.error) return `目录获取失败：${ttsCatalog.error}（仍可手填）`
  if (!ttsCatalog.loaded) return '目录尚未获取，可手填或点「获取 TTS 目录」'
  if (field.path === 'tts.model') {
    return `模型 ${ttsCatalog.models.length} 个 · ${ttsCatalog.modelSource === 'api' ? 'API 获取' : '内置列表'}`
  }
  return `音色 ${ttsCatalog.voices.length} 个 · ${ttsCatalog.source === 'api' ? '来自 fish API' : 'fallback/未获取'}`
}

function ttsOverrides() {
  const tts = sections.tts || {}
  const saved = baseline.tts || {}
  const out = {}
  // 与旧页一致：只有端点确实变化才提交 endpoint；api_key 只提交用户新填的非空值。
  if (String(tts.endpoint ?? '') !== String(saved.endpoint ?? '')) out.endpoint = tts.endpoint ?? ''
  if (typeof tts.api_key === 'string' && tts.api_key) out.api_key = tts.api_key
  return out
}

async function loadTtsCatalog() {
  if (ttsCatalog.loading) return
  ttsCatalog.loading = true
  ttsCatalog.error = ''
  try {
    const data = await requestJson('/tts/models', stripSecretMask(ttsOverrides()))
    if (!data || typeof data !== 'object') throw new Error('返回格式无效')
    ttsCatalog.models = Array.isArray(data.models) ? data.models.filter((item) => typeof item === 'string' && item) : []
    ttsCatalog.voices = Array.isArray(data.voices) ? data.voices : []
    ttsCatalog.source = data.source === 'api' ? 'api' : 'fallback'
    ttsCatalog.modelSource = data.modelSource === 'api' ? 'api' : 'fallback'
    ttsCatalog.loaded = true
    toast(`已获取 TTS 目录：模型 ${ttsCatalog.models.length} 个，音色 ${ttsCatalog.voices.length} 个`, 'ok')
  } catch (e) {
    ttsCatalog.error = e.message || String(e)
    toast(`获取 TTS 目录失败：${ttsCatalog.error}；可继续手填模型/音色。`, 'err')
  } finally {
    ttsCatalog.loading = false
  }
}

/* ---------- 识图运行状态（POST /ai/vision/status） ---------- */
function visionOverrides() {
  const chat = sections.ai_chat || {}
  const ai = sections.ai || {}
  const vision = chat.vision || {}
  const model = String(vision.model || chat.model || '').trim()
  const out = {}
  if (model) out.model = model
  if (ai.provider) out.provider = ai.provider
  if (ai.endpoint) out.endpoint = ai.endpoint
  return out
}

function visionCapabilityText(data) {
  if (!data) return '—'
  if (data.disabled) return '识图未启用（降级纯文本）'
  if (data.capability === 'supported' || data.supports_image === true) return '支持图片'
  if (data.capability === 'unsupported') return '明确不支持图片（降级纯文本）'
  return '未知（按降级纯文本处理）'
}

async function loadVisionStatus(force = false) {
  if (visionState.loading) return
  if (!force && Date.now() - visionState.fetchedAt < 5000) return
  visionState.loading = true
  visionState.error = ''
  try {
    visionState.data = await requestJson('/ai/vision/status', visionOverrides())
  } catch (e) {
    visionState.data = null
    visionState.error = `模型能力暂不可用：${e.message || e}（已降级纯文本；可手填视觉模型并保存）`
  } finally {
    visionState.fetchedAt = Date.now()
    visionState.loading = false
  }
}

watch(activeSection, (name) => {
  if (name === 'ai_chat') loadVisionStatus(false)
})

async function reload() {
  if (dirtySections.value.length && !window.confirm('有未保存修改，重新读取会丢弃它们，继续？')) return
  await load(true)
}

// 按 schema 把草稿里的数字字段规整成 number。
// 后端对 int/float 严格反序列化，字符串会直接 400 invalid type；
// 配置中心有 coerceField，这里此前没有，导致 AI 页数值改不动/存不回。
function coerceNumberValue(field, value) {
  const kind = field.kind ? field.kind.type : 'string'
  if (kind !== 'int' && kind !== 'float') return value
  if (value === null || value === undefined) return value
  if (typeof value === 'number') return value
  const text = String(value).trim()
  if (text === '') return field.nullable === true ? null : value
  const number = Number(text)
  return Number.isFinite(number) ? number : value
}

function normalizedSection(name) {
  const section = schemaSection(name)
  const source = clone(sections[name])
  if (!section) return source
  for (const field of section.fields || []) {
    const segments = String(field.path).split('.').slice(1)
    if (!segments.length) continue
    let cursor = source
    let ok = true
    for (const segment of segments.slice(0, -1)) {
      if (cursor == null || typeof cursor !== 'object') { ok = false; break }
      cursor = cursor[segment]
    }
    if (!ok || cursor == null || typeof cursor !== 'object') continue
    const key = segments[segments.length - 1]
    if (!(key in cursor)) continue
    cursor[key] = coerceNumberValue(field, cursor[key])
  }
  return source
}

async function save() {
  if (saving.value || !dirtySections.value.length) return
  const firstError = Object.keys(localErrors.value)[0]
  if (firstError) {
    toast(`有 ${Object.keys(localErrors.value).length} 个字段格式无效，请修正后再保存`, 'err')
    document.querySelector(`[data-cfg-field="${firstError}"]`)?.scrollIntoView({ block: 'center' })
    return
  }
  const payload = {}
  for (const name of dirtySections.value) payload[name] = normalizedSection(name)
  saving.value = true
  try {
    let response = null
    const ok = await saveSections(payload, (result) => { response = result })
    if (!ok) return
    clearDirty()
    state.fieldErrors = {}
    localErrors.value = {}
    await loadConfig(true)
    resetDraft()
    const revision = response && response.revision != null ? response.revision : state.revision
    if (revision != null) toast(`已保存，当前版本 ${revision}`, 'ok')
    else toast('已保存', 'ok')
  } finally {
    saving.value = false
  }
}

onMounted(() => load(true))
</script>

<style scoped>
.ai-config-page { display: flex; flex-direction: column; gap: var(--sp-3); }
.ai-main-tabs { align-self: flex-start; }
.ai-section-tabs {
  position: sticky;
  top: 0;
  z-index: 3;
  align-self: flex-start;
  background: var(--bg-0, var(--bg-card));
  padding: 4px 0;
}
.ai-config-body { display: flex; flex-direction: column; gap: var(--sp-3); }
.config-section-stats { color: var(--text-3); font-size: var(--fs-meta); }
.dirty-dot {
  display: inline-block;
  width: 6px;
  height: 6px;
  margin-left: 6px;
  border-radius: 50%;
  background: var(--warn);
}
.panel-body > * + * { margin-top: var(--sp-3); }
.ai-tts-picker { display: flex; flex-direction: column; gap: 3px; min-width: 0; width: 100%; }
.ai-tts-picker .model-picker { width: 100%; }
.ai-tts-picker .model-picker input { width: auto; flex: 1 1 180px; }
.ai-tts-picker .model-picker select { width: auto; flex: 0 0 auto; min-width: 150px; }
.ai-tts-picker .model-picker .btn { width: auto; flex: 0 0 auto; }
.ai-vision-head { display: flex; align-items: baseline; justify-content: space-between; gap: var(--sp-2); }
.ai-vision-grid { display: flex; gap: 4px var(--sp-3); flex-wrap: wrap; margin-top: var(--sp-2); }
</style>
