<template>
  <div class="visual-page">
    <div v-if="loading" class="loading-card">正在读取视觉档案…</div>

    <div v-else-if="loadError" class="config-message err">
      视觉档案读取失败：{{ loadError }}
      <button type="button" class="btn" style="margin-left: 8px" @click="load()">重试</button>
    </div>

    <template v-else-if="draft">
      <div class="config-status">
        <span class="chip"><span>保存版本</span><em>{{ revision == null ? '—' : revision }}</em></span>
        <span class="chip"><span>画布</span><em>{{ canvasLabel }}</em></span>
        <span class="chip">
          <span>未保存</span>
          <em :class="{ 'config-dirty': dirty }">{{ dirty ? '是' : '否' }}</em>
        </span>
        <span class="chip">
          <span>待重载字段</span>
          <em>{{ pendingReloadFields.length }}</em>
        </span>
        <span class="chip">
          <span>与生效值差异</span>
          <em>{{ payload.profileDifferences ? payload.profileDifferences.length : 0 }}</em>
        </span>
        <span class="config-field-hint" style="margin-left: auto">
          只写视觉档案（templates / screen / thresholds / ocr 相关区域），不动其它配置段。
        </span>
      </div>

      <div class="visual-toolbar">
        <button type="button" class="btn" :disabled="busy" @click="load(true)">重新载入</button>
        <button
          type="button"
          class="btn"
          :disabled="busy || !hasDefaultProfile"
          title="把草稿恢复为内置默认 1080P 视觉档案（仍需预览后保存）"
          @click="restoreDefaults"
        >
          恢复默认 1080P
        </button>
        <button type="button" class="btn" :disabled="busy" @click="copyJson">复制 JSON</button>
        <span class="visual-toolbar-sep"></span>
        <button
          type="button"
          class="btn"
          :class="{ active: tab === 'form' }"
          @click="switchTab('form')"
        >
          表单编辑
        </button>
        <button
          type="button"
          class="btn"
          :class="{ active: tab === 'json' }"
          @click="switchTab('json')"
        >
          JSON 编辑
        </button>
        <span class="visual-toolbar-sep"></span>
        <button
          type="button"
          class="btn primary"
          :disabled="busy || importing || !canImport"
          :title="canImport ? '把预览通过的草稿写入视觉档案' : '请先预览当前草稿'"
          @click="importDraft"
        >
          {{ importing ? '保存中…' : '保存导入' }}
        </button>
      </div>

      <div v-if="message.text" class="config-message" :class="message.kind">{{ message.text }}</div>

      <div class="visual-layout">
        <section class="visual-main">
          <template v-if="tab === 'form'">
            <VisualEditor :draft="draft" :disabled="busy || importing" @edit="onDraftEdited" />
          </template>

          <template v-else>
            <div class="config-group-card">
              <div class="config-group-head">
                JSON 草稿
                <span class="config-field-hint" style="margin-left: auto; font-weight: normal">
                  编辑后点「应用到表单」；未应用前不会进入预览/保存。
                </span>
              </div>
              <textarea
                v-model="jsonText"
                class="visual-json"
                @input="jsonDirty = true"
                rows="26"
                spellcheck="false"
                :disabled="busy || importing"
                aria-label="视觉档案 JSON"
              ></textarea>
              <div class="visual-json-actions">
                <button type="button" class="btn" :disabled="busy" @click="applyJson">
                  应用到表单
                </button>
                <button type="button" class="btn" :disabled="busy" @click="formatJson">
                  格式化 JSON
                </button>
                <button type="button" class="btn" :disabled="busy" @click="syncJsonFromDraft">
                  从表单重新载入
                </button>
                <span v-if="jsonError" class="config-field-error">{{ jsonError }}</span>
              </div>
            </div>
          </template>

        </section>

        <aside class="visual-side">
          <div class="config-group-card">
            <div class="config-group-head">
              预览差异
              <span class="config-field-hint" style="margin-left: auto; font-weight: normal">
                预览只读；通过后「保存导入」会带 baseRevision 做版本校验。
              </span>
            </div>
            <div class="visual-preview-actions">
              <button type="button" class="btn primary" :disabled="busy || importing" @click="previewDraft">
                预览当前草稿
              </button>
              <span v-if="preview" class="config-field-hint">
                预览基于版本 {{ preview.revision }}，差异 {{ preview.differences.length }} 项。
              </span>
              <span v-else class="config-field-hint">尚未预览；任何修改都会让预览失效。</span>
            </div>
            <ul v-if="preview && preview.differences.length" class="visual-diff-list">
              <li v-for="item in preview.differences" :key="item.path">
                <code>{{ item.path || '/' }}</code>
                <span class="visual-diff-before">{{ short(item.before) }}</span>
                <span class="visual-diff-arrow">→</span>
                <span class="visual-diff-after">{{ short(item.after) }}</span>
              </li>
            </ul>
            <p v-else-if="preview" class="config-field-hint">草稿与当前保存值一致，没有差异。</p>
            <pre v-if="preview && preview.differences.length" class="visual-diff-json">{{
              JSON.stringify(preview.differences, null, 2)
            }}</pre>
          </div>
          <div class="config-group-card">
            <div class="config-group-head">档案状态</div>
            <dl class="visual-facts">
              <dt>保存版本</dt>
              <dd>{{ revision == null ? '—' : revision }}</dd>
              <dt>档案名称</dt>
              <dd>{{ draft.name || '（未命名）' }}</dd>
              <dt>结构版本</dt>
              <dd>{{ draft.schema_version }}</dd>
              <dt>基准画布</dt>
              <dd>{{ canvasLabel }}</dd>
              <dt>模板字段</dt>
              <dd>{{ templateCount }}</dd>
              <dt>与生效值差异</dt>
              <dd>{{ payload.profileDifferences ? payload.profileDifferences.length : 0 }} 项</dd>
            </dl>
            <p v-if="payload.statusNote" class="config-field-hint">{{ payload.statusNote }}</p>
          </div>

          <div v-if="pendingReloadFields.length" class="config-group-card">
            <div class="config-group-head">待重载字段</div>
            <p class="config-field-hint">保存已生效，但这些字段需要重启/重载进程才会真正使用：</p>
            <ul class="visual-reload-list">
              <li v-for="field in pendingReloadFields" :key="field"><code>{{ field }}</code></li>
            </ul>
          </div>

          <div v-if="payload.profileDifferences && payload.profileDifferences.length" class="config-group-card">
            <div class="config-group-head">保存值 vs 生效值</div>
            <ul class="visual-reload-list">
              <li v-for="item in payload.profileDifferences.slice(0, 20)" :key="item.path">
                <code>{{ item.path }}</code>
                <span class="visual-diff-before">{{ short(item.before) }}</span>
                →
                <span class="visual-diff-after">{{ short(item.after) }}</span>
              </li>
            </ul>
            <p v-if="payload.profileDifferences.length > 20" class="config-field-hint">
              仅显示前 20 项，共 {{ payload.profileDifferences.length }} 项。
            </p>
          </div>

          <div v-if="payload.catalog" class="config-group-card">
            <div class="config-group-head">匹配目录</div>
            <pre class="visual-catalog">{{ JSON.stringify(payload.catalog, null, 2) }}</pre>
          </div>
        </aside>
      </div>
    </template>
  </div>
</template>

<script setup>
import { computed, onMounted, reactive, ref } from 'vue'
import { getJson, requestJson } from '../api.js'
import { toast } from '../store.js'
import VisualEditor from '../components/VisualEditor.vue'

const loading = ref(true)
const busy = ref(false)
const importing = ref(false)
const loadError = ref('')
const payload = ref({})
const revision = ref(null)
const draft = ref(null)
const savedJson = ref('')
const tab = ref('form')
const jsonText = ref('')
const jsonError = ref('')
const jsonDirty = ref(false)
const preview = ref(null)
const message = reactive({ text: '', kind: '' })

const pendingReloadFields = computed(() =>
  Array.isArray(payload.value.pendingReloadFields) ? payload.value.pendingReloadFields : [],
)
const hasDefaultProfile = computed(() => !!payload.value.defaultProfile)
const canvasLabel = computed(() => {
  const canvas = draft.value && draft.value.canvas
  return canvas && canvas.width && canvas.height ? `${canvas.width}×${canvas.height}` : '—'
})
const templateCount = computed(() =>
  draft.value && draft.value.templates ? Object.keys(draft.value.templates).length : 0,
)
const draftJson = computed(() => (draft.value ? JSON.stringify(draft.value) : ''))
const dirty = computed(() => !!draft.value && draftJson.value !== savedJson.value)
const canImport = computed(
  () =>
    !!(preview.value && preview.value.draftJson === draftJson.value && draft.value && !jsonDirty.value),
)

function say(text, kind = '') {
  message.text = text || ''
  message.kind = kind
}
function clone(value) {
  return JSON.parse(JSON.stringify(value))
}
function setBusy(value) {
  busy.value = value
}
function invalidatePreview() {
  preview.value = null
}

async function load(force = false) {
  if (force && dirty.value && !window.confirm('重新载入会丢弃未保存的视觉档案修改，继续？')) return
  loading.value = true
  loadError.value = ''
  setBusy(true)
  try {
    const data = await getJson('/config/visual')
    if (!data || data.ok !== true) throw new Error((data && data.message) || '接口返回异常')
    if (!data.profile || typeof data.profile !== 'object') {
      throw new Error('接口未返回 profile（视觉档案为空）')
    }
    payload.value = data
    revision.value = data.revision == null ? null : Number(data.revision)
    draft.value = clone(data.profile)
    savedJson.value = JSON.stringify(draft.value)
    jsonText.value = JSON.stringify(draft.value, null, 2)
    jsonError.value = ''
    jsonDirty.value = false
    invalidatePreview()
    say(`已载入视觉档案，版本 ${revision.value == null ? '—' : revision.value}。`)
  } catch (error) {
    loadError.value = error.message || String(error)
  } finally {
    loading.value = false
    setBusy(false)
  }
}

function onDraftEdited() {
  invalidatePreview()
  if (tab.value === 'json') syncJsonFromDraft()
  say('视觉档案有未保存修改，请先预览。')
}

function switchTab(next) {
  if (next === 'json' && tab.value !== 'json') jsonText.value = JSON.stringify(draft.value, null, 2)
  tab.value = next
}

function syncJsonFromDraft() {
  jsonText.value = JSON.stringify(draft.value, null, 2)
  jsonError.value = ''
  jsonDirty.value = false
}

function normalizeProfile(parsed) {
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new Error('JSON 顶层必须是对象')
  }
  if (typeof parsed.name !== 'string') throw new Error('缺少 name（字符串）')
  if (!parsed.canvas || typeof parsed.canvas !== 'object') throw new Error('缺少 canvas 对象')
  if (!Number.isFinite(Number(parsed.canvas.width)) || !Number.isFinite(Number(parsed.canvas.height))) {
    throw new Error('canvas.width / canvas.height 必须是数字')
  }
  if (!parsed.templates || typeof parsed.templates !== 'object') throw new Error('缺少 templates 对象')
  return parsed
}

function applyJson() {
  try {
    const parsed = normalizeProfile(JSON.parse(jsonText.value))
    draft.value = parsed
    invalidatePreview()
    jsonError.value = ''
    jsonDirty.value = false
    tab.value = 'form'
    say('已把 JSON 应用到表单，请预览后保存。')
  } catch (error) {
    jsonError.value = error.message || String(error)
  }
}

function formatJson() {
  try {
    jsonText.value = JSON.stringify(JSON.parse(jsonText.value), null, 2)
    jsonError.value = ''
  } catch (error) {
    jsonError.value = error.message || String(error)
  }
}

function restoreDefaults() {
  const defaults = payload.value.defaultProfile
  if (!defaults) {
    say('接口没有返回 defaultProfile，无法恢复默认。', 'err')
    return
  }
  if (dirty.value && !window.confirm('恢复默认会覆盖当前未保存修改，继续？')) return
  draft.value = clone(defaults)
  invalidatePreview()
  syncJsonFromDraft()
  say('已填入默认 1080P 档案，请预览后保存。')
}

async function copyJson() {
  const text = JSON.stringify(draft.value, null, 2)
  try {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      await navigator.clipboard.writeText(text)
    } else {
      const area = document.createElement('textarea')
      area.value = text
      document.body.appendChild(area)
      area.select()
      document.execCommand('copy')
      document.body.removeChild(area)
    }
    say('已复制当前草稿 JSON（未保存）。')
  } catch (error) {
    say(`复制失败：${error.message || error}`, 'err')
  }
}

async function previewDraft() {
  if (!draft.value) return
  if (jsonDirty.value) {
    say('JSON 编辑区有未应用的修改，请先「应用到表单」再预览。', 'err')
    return
  }
  setBusy(true)
  invalidatePreview()
  const draftAtPreview = draftJson.value
  try {
    const data = await requestJson('/config/visual/preview', { profile: draft.value })
    if (!data || data.ok !== true) {
      const detail = data && data.errors ? JSON.stringify(data.errors) : (data && data.message) || '预览失败'
      throw new Error(detail)
    }
    if (draftJson.value !== draftAtPreview) {
      say('预览期间草稿又发生变化，请重新预览。', 'err')
      return
    }
    preview.value = {
      revision: Number(data.revision),
      differences: Array.isArray(data.differences) ? data.differences : [],
      draftJson: draftAtPreview,
    }
    say(
      `预览通过：基准版本 ${preview.value.revision}，${preview.value.differences.length} 项差异；尚未保存。`,
    )
  } catch (error) {
    say(`预览失败：${error.message || error}`, 'err')
  } finally {
    setBusy(false)
  }
}

async function importDraft() {
  if (!canImport.value || importing.value) return
  importing.value = true
  setBusy(true)
  const snapshot = preview.value
  invalidatePreview()
  try {
    const data = await requestJson('/config/visual/import', {
      baseRevision: snapshot.revision,
      profile: JSON.parse(snapshot.draftJson),
    })
    if (!data || data.ok !== true) {
      const detail =
        data && data.errors
          ? JSON.stringify(data.errors)
          : (data && data.message) || '导入失败（可能需要重新预览）'
      throw new Error(detail)
    }
    toast('视觉档案已保存')
    say(`视觉档案已保存，版本 ${data.revision == null ? '—' : data.revision}。`)
    await load()
  } catch (error) {
    say(`导入失败：${error.message || error}；请重新预览后再试。`, 'err')
  } finally {
    importing.value = false
    setBusy(false)
  }
}

function short(value) {
  const text = typeof value === 'string' ? `"${value}"` : JSON.stringify(value)
  if (text == null) return 'null'
  return text.length > 80 ? `${text.slice(0, 77)}…` : text
}

onMounted(() => load())
</script>

<style scoped>
.visual-page {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 0 24px 32px;
}
.visual-toolbar {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}
.visual-toolbar .btn.active {
  outline: 2px solid var(--accent, #7bd88f);
  outline-offset: 1px;
}
.visual-toolbar .btn.primary {
  border-color: var(--accent, #7bd88f);
}
.visual-toolbar-sep {
  width: 1px;
  height: 18px;
  background: var(--sep-1);
  margin: 0 4px;
}
.visual-layout {
  display: grid;
  grid-template-columns: minmax(0, 3fr) minmax(280px, 1fr);
  gap: 12px;
  align-items: start;
}
.visual-main {
  display: flex;
  flex-direction: column;
  gap: 12px;
  min-width: 0;
}
.visual-side {
  display: flex;
  flex-direction: column;
  gap: 12px;
  min-width: 0;
}
.visual-json {
  width: 100%;
  min-height: 420px;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
  line-height: 1.45;
}
.visual-json-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 8px;
  flex-wrap: wrap;
}
.visual-preview-actions {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
  margin-bottom: 8px;
}
.visual-diff-list,
.visual-reload-list {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 4px;
  max-height: 320px;
  overflow: auto;
}
.visual-diff-list li,
.visual-reload-list li {
  display: flex;
  align-items: baseline;
  gap: 6px;
  font-size: 12px;
  flex-wrap: wrap;
}
.visual-diff-before {
  color: #d9a05b;
}
.visual-diff-after {
  color: #7bd88f;
}
.visual-diff-arrow {
  opacity: 0.6;
}
.visual-diff-json,
.visual-catalog {
  max-height: 260px;
  overflow: auto;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 11px;
  background: var(--bg-2);
  border: 1px solid var(--sep-1);
  border-radius: 6px;
  padding: 8px;
}
.visual-facts {
  display: grid;
  grid-template-columns: auto 1fr;
  gap: 4px 10px;
  margin: 0;
  font-size: 12px;
}
.visual-facts dt {
  opacity: 0.7;
}
.visual-facts dd {
  margin: 0;
  font-weight: 600;
}
@media (max-width: 1100px) {
  .visual-layout {
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>
