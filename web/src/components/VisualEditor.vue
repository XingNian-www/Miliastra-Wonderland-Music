<template>
  <div class="visual-editor">
    <div class="config-group-card">
      <div class="config-group-head">档案信息</div>
      <div class="visual-editor-grid">
        <label class="visual-field">
          <span>名称 <code>name</code></span>
          <input
            type="text"
            :value="draft.name"
            :disabled="disabled"
            @change="setText('name', $event, false)"
          />
        </label>
        <label class="visual-field">
          <span>结构版本 <code>schema_version</code></span>
          <input
            type="number"
            step="1"
            :value="draft.schema_version"
            :disabled="disabled"
            @change="setNumber(['schema_version'], $event)"
          />
        </label>
        <label class="visual-field">
          <span>画布宽 <code>canvas.width</code></span>
          <input
            type="number"
            step="1"
            :value="draft.canvas && draft.canvas.width"
            :disabled="disabled"
            @change="setNumber(['canvas', 'width'], $event)"
          />
        </label>
        <label class="visual-field">
          <span>画布高 <code>canvas.height</code></span>
          <input
            type="number"
            step="1"
            :value="draft.canvas && draft.canvas.height"
            :disabled="disabled"
            @change="setNumber(['canvas', 'height'], $event)"
          />
        </label>
      </div>
      <p class="config-field-hint">
        画布是分类器的基准尺寸；线上窗口与画布不一致时会缩放。恢复默认 1080P 可从页面工具栏操作。
      </p>
    </div>

    <div class="config-group-card">
      <div class="config-group-head">
        模板路径 <code>templates</code>（路径中的 / 或 \ 都会在运行时解析）
        <span class="config-domain-count">{{ templateEntries.length }}</span>
      </div>
      <p class="config-field-hint">
        路径是相对仓库/发布目录的模板图片。修改后会出现在「预览差异」里，保存需要先预览。
      </p>
      <div class="visual-editor-grid">
        <label
          v-for="entry in templateEntries"
          :key="entry.key"
          class="visual-field"
          :class="{ 'visual-field-invalid': invalid[`templates.${entry.key}`] }"
        >
          <span><code>{{ entry.key }}</code></span>
          <input
            type="text"
            :value="typeof entry.value === 'number' ? f32Text(entry.value) : entry.value"
            :disabled="disabled"
            spellcheck="false"
            @change="setText(['templates', entry.key], $event, false)"
          />
        </label>
      </div>
      <div v-if="!templateEntries.length" class="config-field-hint">当前档案没有模板字段。</div>
    </div>

    <div v-for="section in valueSections" :key="section.key" class="config-group-card">
      <div class="config-group-head">
        {{ section.label }} <code>{{ section.key }}</code>
        <span class="config-domain-count">{{ section.entries.length }}</span>
      </div>
      <p class="config-field-hint">{{ section.hint }}</p>
      <div class="visual-editor-grid">
        <div
          v-for="entry in section.entries"
          :key="entry.key"
          class="visual-field"
          :class="{ 'visual-field-invalid': invalid[`${section.key}.${entry.key}`] }"
        >
          <span><code>{{ entry.key }}</code></span>

          <!-- 矩形：x / y / width / height 四格 -->
          <div v-if="isRect(entry.value)" class="visual-rect-row">
            <label v-for="axis in rectAxes(entry.value)" :key="axis">
              <em>{{ axis }}</em>
              <input
                type="number"
                step="1"
                :value="entry.value[axis]"
                :disabled="disabled"
                @change="setRect(`${section.key}.${entry.key}`, axis, $event)"
              />
            </label>
          </div>

          <!-- 点：x / y 两格 -->
          <div v-else-if="isPoint(entry.value)" class="visual-rect-row">
            <label v-for="axis in ['x', 'y']" :key="axis">
              <em>{{ axis }}</em>
              <input
                type="number"
                step="1"
                :value="entry.value[axis]"
                :disabled="disabled"
                @change="setRect(`${section.key}.${entry.key}`, axis, $event)"
              />
            </label>
          </div>

          <!-- 数字 -->
          <input
            v-else-if="typeof entry.value === 'number'"
            type="number"
            step="any"
            :value="entry.value"
            :disabled="disabled"
            @change="setNumber([section.key, entry.key], $event)"
          />

          <!-- 字符串 / 布尔 -->
          <input
            v-else-if="typeof entry.value === 'string'"
            type="text"
            :value="entry.value"
            :disabled="disabled"
            spellcheck="false"
            @change="setText([section.key, entry.key], $event, false)"
          />
          <label v-else-if="typeof entry.value === 'boolean'" class="visual-bool">
            <input
              type="checkbox"
              :checked="entry.value"
              :disabled="disabled"
              @change="setBool([section.key, entry.key], $event)"
            />
            <span>{{ entry.value ? 'true' : 'false' }}</span>
          </label>

          <!-- 数字数组（如去重坐标） -->
          <input
            v-else-if="isNumberArray(entry.value)"
            type="text"
            :value="entry.value.join(', ')"
            :disabled="disabled"
            spellcheck="false"
            @change="setNumberArray([section.key, entry.key], $event)"
          />

          <!-- 其它复杂值：JSON 文本 -->
          <textarea
            v-else
            rows="3"
            :value="rawValue(`${section.key}.${entry.key}`, entry.value)"
            :disabled="disabled"
            spellcheck="false"
            @input="updateRaw(`${section.key}.${entry.key}`, $event)"
            @change="commitRaw(`${section.key}.${entry.key}`, [section.key, entry.key])"
          ></textarea>
        </div>
      </div>
      <div v-if="!section.entries.length" class="config-field-hint">本段为空。</div>
    </div>

    <div v-if="!valueSections.length && !templateEntries.length" class="config-field-hint">
      当前档案没有可编辑字段（请确认已成功读取 /config/visual）。
    </div>
  </div>
</template>

<script setup>
import { f32Text } from '../util/format.js'
import { computed, reactive } from 'vue'

const props = defineProps({
  draft: { type: Object, required: true },
  disabled: { type: Boolean, default: false },
})
const emit = defineEmits(['edit'])

const invalid = reactive({})
const raws = reactive({})

const SECTION_META = [
  { key: 'thresholds', label: '阈值', hint: '模板匹配 / 启动判定的分数阈值（0–1，越接近 1 越严格）。' },
  { key: 'regions', label: '搜索区域', hint: '各判据在画布上的搜索矩形（像素坐标）。' },
  { key: 'points', label: '点击点', hint: '流程点击使用的画布坐标（x / y）。' },
  { key: 'chat_geometry', label: '聊天几何', hint: 'OCR 聊天气泡的行块几何参数（像素）。' },
  { key: 'capture', label: '截图方式', hint: '截图实现选择，保持保存值原样。' },
  { key: 'matching', label: '匹配实现', hint: '模板匹配实现/后端选择。' },
]

const templateEntries = computed(() =>
  Object.entries(props.draft.templates || {})
    .map(([key, value]) => ({ key, value }))
    .sort((a, b) => a.key.localeCompare(b.key)),
)

const valueSections = computed(() =>
  SECTION_META.map((meta) => ({
    ...meta,
    entries: Object.entries(props.draft[meta.key] || {})
      .map(([key, value]) => ({ key, value }))
      .sort((a, b) => a.key.localeCompare(b.key)),
  })).filter((section) => section.entries.length > 0 || section.key !== 'points'),
)

function isRect(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false
  const keys = Object.keys(value).sort().join(',')
  return keys === 'height,width,x,y' && Object.values(value).every((item) => Number.isFinite(item))
}
function rectAxes() {
  return ['x', 'y', 'width', 'height']
}
function isPoint(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false
  const keys = Object.keys(value).sort().join(',')
  return keys === 'x,y' && Object.values(value).every((item) => Number.isFinite(item))
}
function isNumberArray(value) {
  return Array.isArray(value) && value.length > 0 && value.every((item) => typeof item === 'number')
}
function rawValue(path, value) {
  if (typeof raws[path] === 'string') return raws[path]
  return JSON.stringify(value)
}

function markInvalid(path, bad) {
  if (bad) invalid[path] = true
  else delete invalid[path]
}
function notify() {
  emit('edit')
}
function setNested(path, value) {
  let cursor = props.draft
  for (let i = 0; i < path.length - 1; i += 1) {
    if (cursor[path[i]] == null || typeof cursor[path[i]] !== 'object') cursor[path[i]] = {}
    cursor = cursor[path[i]]
  }
  cursor[path[path.length - 1]] = value
}
function parseNumber(event) {
  const text = String(event.target.value).trim()
  if (text === '') return NaN
  const value = Number(text)
  return Number.isFinite(value) ? value : NaN
}
function setNumber(path, event) {
  const value = parseNumber(event)
  const pathKey = path.join('.')
  if (!Number.isFinite(value)) {
    markInvalid(pathKey, true)
    event.target.value = String(path.reduce((cursor, key) => (cursor == null ? cursor : cursor[key]), props.draft))
    return
  }
  markInvalid(pathKey, false)
  setNested(path, value)
  notify()
}
function setNumberArray(path, event) {
  const text = String(event.target.value || '')
  const parts = text.split(/[,，]/).map((item) => item.trim()).filter(Boolean)
  const numbers = parts.map(Number)
  const pathKey = path.join('.')
  if (!numbers.length || numbers.some((item) => !Number.isFinite(item))) {
    markInvalid(pathKey, true)
    event.target.value = (path.reduce((cursor, key) => (cursor == null ? cursor : cursor[key]), props.draft) || []).join(', ')
    return
  }
  markInvalid(pathKey, false)
  setNested(path, numbers)
  notify()
}
function setText(path, event, nullable = true) {
  let text = String(event.target.value ?? '')
  if (nullable && text === '') text = ''
  setNested(path, text)
  markInvalid(path.join('.'), false)
  notify()
}
function setBool(path, event) {
  setNested(path, !!event.target.checked)
  notify()
}
function setRect(sectionKey, axis, event) {
  const value = parseNumber(event)
  if (!Number.isFinite(value)) {
    markInvalid(sectionKey, true)
    return
  }
  const [group, key] = sectionKey.split('.')
  if (props.draft[group] && props.draft[group][key]) props.draft[group][key][axis] = value
  markInvalid(sectionKey, false)
  notify()
}
function updateRaw(path, event) {
  raws[path] = String(event.target.value ?? '')
}
function commitRaw(path, target) {
  const text = raws[path]
  try {
    const parsed = JSON.parse(text)
    setNested(target, parsed)
    markInvalid(path, false)
    delete raws[path]
    notify()
  } catch {
    markInvalid(path, true)
  }
}
</script>

<style scoped>
.visual-editor {
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.visual-editor-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
  gap: 8px 12px;
}
.visual-field {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 6px 8px;
  border: 1px solid var(--line, #3a3a38);
  border-radius: 6px;
  background: var(--panel, #22221f);
}
.visual-field > span {
  font-size: 12px;
  opacity: 0.8;
}
.visual-field input,
.visual-field textarea {
  width: 100%;
}
.visual-field textarea {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
}
.visual-field-invalid {
  border-color: #d9534f;
}
.visual-field-invalid input,
.visual-field-invalid textarea {
  border-color: #d9534f;
}
.visual-rect-row {
  display: flex;
  gap: 6px;
  flex-wrap: wrap;
}
.visual-rect-row label {
  display: flex;
  align-items: center;
  gap: 4px;
  flex: 1 1 60px;
}
.visual-rect-row em {
  font-style: normal;
  font-size: 11px;
  opacity: 0.7;
}
.visual-bool {
  display: flex;
  align-items: center;
  gap: 6px;
}
</style>
