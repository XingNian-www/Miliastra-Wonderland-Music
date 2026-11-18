<template>
  <!-- bool：复用旧页面的开关样式 -->
  <label v-if="kind === 'bool'" class="switch-line">
    <input
      type="checkbox"
      :checked="modelValue === true"
      :disabled="disabled"
      @change="onBoolChange"
    />
    <span>{{ modelValue === true ? '已启用' : '未启用' }}</span>
  </label>

  <!-- int / float -->
  <input
    v-else-if="isNumberKind"
    type="number"
    :min="minValue"
    :max="maxValue"
    :step="kind === 'float' ? 'any' : undefined"
    :value="numberText"
    :disabled="disabled"
    @input="onNumberInput"
    @change="onNumberChange"
  />

  <!-- text：多行文本（提示词等） -->
  <textarea
    v-else-if="kind === 'text'"
    class="config-prompt-text"
    :value="textText"
    :placeholder="placeholder"
    :disabled="disabled"
    @input="onTextInput"
  ></textarea>

  <!-- object：自由 JSON 对象 -->
  <div v-else-if="kind === 'object'" class="config-array" style="max-width: 100%">
    <textarea
      rows="6"
      :value="jsonText"
      :placeholder="objectPlaceholder"
      :disabled="disabled"
      style="min-height: 120px; font-family: var(--font-mono); font-size: var(--fs-meta); line-height: 1.5"
      @input="onObjectInput"
    ></textarea>
    <div v-if="objectInvalid" class="config-field-hint" style="color: var(--danger)">
      JSON 格式无效：必须是对象、数组或 null
    </div>
  </div>

  <!-- enum -->
  <select v-else-if="kind === 'enum'" :value="enumSelected" :disabled="disabled" @change="onEnumChange">
    <option v-if="field.nullable" value="__config_null__">（未设置）</option>
    <option v-for="option in enumValues" :key="option.value" :value="option.value">{{ option.label }}</option>
    <option v-if="unknownEnum != null" :value="unknownEnum">（未知值 {{ unknownEnum }}）</option>
  </select>

  <!-- 表情 AI 白名单：紧凑 chips 编辑器 -->
  <div v-else-if="isArrayKind && isAiAllowed" class="config-array" style="max-width: 100%">
    <div class="emoji-allow-chips">
      <span v-for="(item, index) in arrayValue" :key="`${item}-${index}`" class="emoji-allow-chip">
        <code>{{ item }}</code>
        <button type="button" title="移除" :disabled="disabled" @click="removeArrayAt(index)">×</button>
      </span>
      <span v-if="!arrayValue.length" class="config-field-hint">
        还没有选择表情，可在下方「全量表情目录」勾选后加入
      </span>
    </div>
    <div class="emoji-allow-add">
      <input
        v-model="chipInput"
        type="text"
        placeholder="手动添加：页签序号|表情文字，如 2|交给我吧"
        :disabled="disabled"
        @keydown.enter.prevent="addChip"
      />
      <button type="button" class="btn small" :disabled="disabled" @click="addChip">添加</button>
    </div>
  </div>

  <!-- stringArray / array：逐行编辑 -->
  <div v-else-if="isArrayKind" class="config-array">
    <div v-for="(item, index) in arrayValue" :key="index" class="config-string-row">
      <input
        type="text"
        :value="item"
        :disabled="disabled"
        @input="onArrayItemInput(index, $event)"
      />
      <button type="button" class="btn small ghost" :disabled="disabled" @click="removeArrayAt(index)">删除</button>
    </div>
    <div class="config-string-row">
      <button type="button" class="btn small" :disabled="disabled" @click="addArrayRow">添加一行</button>
    </div>
  </div>

  <!-- rect / point -->
  <div v-else-if="isRectKind" class="config-num-row" :class="{ two: rectKeys.length === 2 }">
    <label v-for="key in rectKeys" :key="key">
      {{ rectLabels[key] }}
      <input
        type="number"
        step="1"
        :value="rectPart(key)"
        :disabled="disabled"
        @input="onRectInput(key, $event)"
      />
    </label>
  </div>

  <!-- secret：空串表示不修改 -->
  <div v-else-if="kind === 'secret'" class="config-array" style="max-width: 100%">
    <div class="config-string-row">
      <input
        type="password"
        :value="secretText"
        placeholder="留空表示不修改"
        autocomplete="new-password"
        :disabled="disabled"
        @input="onSecretInput"
      />
      <button
        v-if="secretClearing"
        type="button"
        class="btn small ghost"
        :disabled="disabled"
        @click="undoSecretClear"
      >
        撤销清除
      </button>
      <button v-else type="button" class="btn small warn" :disabled="disabled" @click="markSecretClear">
        清除
      </button>
    </div>
    <div class="config-field-hint">{{ secretNote }}</div>
  </div>

  <!-- string / path / modelName / 兜底 -->
  <input
    v-else
    type="text"
    :value="textText"
    :placeholder="placeholder"
    :disabled="disabled"
    @input="onTextInput"
  />
</template>

<script setup>
import { computed, onBeforeUnmount, ref, watch } from 'vue'
import { f32Text } from '../util/format.js'

const props = defineProps({
  field: { type: Object, required: true },
  modelValue: { default: null },
  disabled: { type: Boolean, default: false },
})

const emit = defineEmits(['update:modelValue', 'invalid'])

const kind = computed(() => (props.field && props.field.kind ? props.field.kind.type : 'string'))
const isNumberKind = computed(() => kind.value === 'int' || kind.value === 'float')
const isArrayKind = computed(() => kind.value === 'stringArray' || kind.value === 'array')
const isRectKind = computed(() => kind.value === 'rect' || kind.value === 'point')
const isAiAllowed = computed(() => String(props.field.path || '').endsWith('.ai_allowed'))

const minValue = computed(() => (props.field.kind && props.field.kind.min != null ? props.field.kind.min : undefined))
const maxValue = computed(() => (props.field.kind && props.field.kind.max != null ? props.field.kind.max : undefined))
const placeholder = computed(() => (props.field.nullable ? '留空表示 null' : ''))

/* ---------- 文本 / 数字 ---------- */
const textText = computed(() => (props.modelValue == null ? '' : String(props.modelValue)))

function onTextInput(event) {
  const raw = event.target.value
  emit('update:modelValue', props.field.nullable && raw.trim() === '' ? null : raw)
}

// 数字输入用本地文本 + lastEmitted 防止受控 value 覆盖用户正在输入的中间态：
// 浏览器在 "1." / "-" 这类半成品上 value 为空但 badInput=true，此时不回写。
const numberText = ref('')
let lastNumberEmitted = null

watch(
  () => props.modelValue,
  (value) => {
    if (value === lastNumberEmitted) return
    numberText.value = value == null ? '' : (kind.value === 'float' ? f32Text(value) : String(value))
  },
  { immediate: true },
)

// 数字字段在草稿里必须是 number，不能是字符串：
// 后端按 f32/u32 严格反序列化，提交 "0.85" 会直接 400 invalid type。
// 输入过程中保留字符串（"1." / "-" 这类半成品），这里只在能解析时上抛数字。
function onNumberInput(event) {
  const element = event.target
  if (element.validity && element.validity.badInput) return
  const raw = element.value
  numberText.value = raw
  if (raw.trim() === '') {
    // 清空：nullable 字段上抛 null（后端只接受 null，不接受 ""）；
        // 非 nullable 仍抛空串，让父级校验报「不能为空」而不是静默存 null。
    const value = props.field.nullable ? null : ''
    lastNumberEmitted = value
    emit('update:modelValue', value)
    return
  }
  const number = Number(raw)
  if (!Number.isFinite(number)) return
  lastNumberEmitted = number
  emit('update:modelValue', number)
}

function onNumberChange(event) {
  const element = event.target
  if (element.validity && element.validity.badInput) {
    const value = props.field.nullable ? null : ''
    numberText.value = ''
    lastNumberEmitted = value
    emit('update:modelValue', value)
    return
  }
  // change 时再规整一次显示：避免 "0.9000" 这类冗余小数留在输入框里。
  const raw = element.value
  if (raw.trim() === '') return
  const number = Number(raw)
  if (!Number.isFinite(number)) return
  numberText.value = kind.value === 'float' ? f32Text(number) : String(number)
}

function onBoolChange(event) {
  emit('update:modelValue', event.target.checked)
}

/* ---------- enum ---------- */
const enumValues = computed(() =>
  props.field.kind && Array.isArray(props.field.kind.values) ? props.field.kind.values : [],
)
const enumSelected = computed(() => {
  if (props.modelValue == null) return props.field.nullable ? '__config_null__' : ''
  return String(props.modelValue)
})
const unknownEnum = computed(() => {
  if (props.modelValue == null) return null
  const text = String(props.modelValue)
  if (!text) return null
  return enumValues.value.some((option) => String(option.value) === text) ? null : text
})

function onEnumChange(event) {
  const raw = event.target.value
  emit('update:modelValue', raw === '__config_null__' ? null : raw)
}

/* ---------- object：本地保留文本，合法时才向上提交 ---------- */
const jsonText = ref('')
const objectInvalid = ref(false)
let lastObjectEmitted = null

function setObjectInvalid(flag) {
  if (objectInvalid.value === flag) return
  objectInvalid.value = flag
  emit('invalid', flag ? 'JSON 格式无效：必须是对象、数组或 null' : '')
}

watch(
  () => props.modelValue,
  (value) => {
    if (value === lastObjectEmitted) return
    jsonText.value = value == null ? '' : JSON.stringify(value, null, 2)
    setObjectInvalid(false)
  },
  { immediate: true },
)

const objectPlaceholder = computed(() =>
  props.field.nullable ? 'null 表示未启用；填入 JSON 对象/数组以启用' : '必须是 JSON 对象或数组',
)

function onObjectInput(event) {
  const raw = event.target.value
  jsonText.value = raw
  const text = raw.trim()
  if (text === '' || text === 'null') {
    const value = props.field.nullable ? null : {}
    lastObjectEmitted = value
    emit('update:modelValue', value)
    setObjectInvalid(false)
    return
  }
  try {
    const parsed = JSON.parse(text)
    if (parsed && typeof parsed === 'object') {
      lastObjectEmitted = parsed
      emit('update:modelValue', parsed)
      setObjectInvalid(false)
    } else {
      setObjectInvalid(true)
    }
  } catch (error) {
    setObjectInvalid(true)
  }
}

/* ---------- stringArray / array ---------- */
const arrayValue = computed(() => (Array.isArray(props.modelValue) ? props.modelValue : []))
const chipInput = ref('')

function removeArrayAt(index) {
  const next = arrayValue.value.slice()
  next.splice(index, 1)
  emit('update:modelValue', next)
}

function addArrayRow() {
  emit('update:modelValue', [...arrayValue.value, ''])
}

function onArrayItemInput(index, event) {
  const next = arrayValue.value.slice()
  next[index] = event.target.value
  emit('update:modelValue', next)
}

function addChip() {
  const text = chipInput.value.trim()
  if (!text) return
  chipInput.value = ''
  if (arrayValue.value.includes(text)) return
  emit('update:modelValue', [...arrayValue.value, text])
}

/* ---------- rect / point ---------- */
const rectLabels = { x: 'X', y: 'Y', width: '宽', height: '高' }
const rectKeys = computed(() => (kind.value === 'point' ? ['x', 'y'] : ['x', 'y', 'width', 'height']))

function rectPart(key) {
  const value = props.modelValue
  if (value == null || typeof value !== 'object') return ''
  return value[key] == null ? '' : String(value[key])
}

function onRectInput(key, event) {
  const element = event.target
  if (element.validity && element.validity.badInput) return
  const raw = String(element.value == null ? '' : element.value)
  const next = {}
  let filled = 0
  for (const part of rectKeys.value) {
    const source = part === key ? raw : rectPart(part)
    if (source === '') {
      next[part] = null
      continue
    }
    const number = Number(source)
    if (Number.isFinite(number)) {
      next[part] = number
      filled += 1
    } else {
      next[part] = null
    }
  }
  if (filled === 0) {
    emit('update:modelValue', props.field.nullable ? null : next)
    return
  }
  emit('update:modelValue', next)
}

/* ---------- secret ---------- */
const secretText = ref('')
let lastSecretEmitted = null
const secretClearing = computed(
  () =>
    props.modelValue != null &&
    typeof props.modelValue === 'object' &&
    props.modelValue.__clear__ === true,
)
const secretNote = computed(() =>
  secretClearing.value
    ? '已标记清除：保存后会清空该密钥；点「撤销清除」可改为不修改。'
    : '留空表示不修改已保存的密钥；点「清除」可移除已保存的密钥。',
)

watch(
  () => props.modelValue,
  (value) => {
    if (value === lastSecretEmitted) return
    secretText.value = ''
  },
)

function onSecretInput(event) {
  const raw = event.target.value
  secretText.value = raw
  lastSecretEmitted = raw
  emit('update:modelValue', raw)
}

function markSecretClear() {
  const marker = { __clear__: true }
  lastSecretEmitted = marker
  secretText.value = ''
  emit('update:modelValue', marker)
}

function undoSecretClear() {
  lastSecretEmitted = ''
  secretText.value = ''
  emit('update:modelValue', '')
}

// 组件被切走时清掉「JSON 无效」标记：草稿里仍留着上一次合法的值，
// 否则父级的待修错误会指向一个已经不存在的输入框，保存被永久禁用。
onBeforeUnmount(() => {
  if (objectInvalid.value) emit('invalid', '')
})
</script>
