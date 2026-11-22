<template>
  <!-- 单个字段行：标签 / 控件 / 生效方式 / 默认值 / 恢复按钮 / 错误。
       完全由 schema 驱动，字段顺序与分组来自父级的 `groups`，这里不写死任何字段名。 -->
  <div
    class="config-field"
    :class="{ bootstrap: field.source === 'bootstrap', err: !!error }"
    :data-kind="kindClass(field)"
    :data-cfg-field="field.path"
  >
    <div class="config-field-label">
      <b v-html="highlightedLabel"></b>
      <code :title="field.path">{{ field.path }}</code>
    </div>
    <div class="config-field-control">
      <FieldControl
        :field="field"
        :model-value="value"
        :disabled="disabled"
        @update:model-value="$emit('update:value', $event)"
        @invalid="$emit('invalid', $event)"
      />
    </div>
    <div class="config-field-side">
      <span v-if="field.source === 'bootstrap'" class="config-effect bootstrap"> config.yaml </span>
      <span v-else class="config-effect" :class="effectClass(field)">{{ effectLabel(field) }}</span>
      <button
        v-if="canReset"
        type="button"
        class="btn small ghost"
        :disabled="disabled"
        :title="resetTitle"
        @click="$emit('reset')"
      >
        {{ resetLabel }}
      </button>
    </div>
    <div class="config-field-foot">
      <span v-if="field.hint" class="config-field-hint">{{ field.hint }}</span>
      <span v-if="canReset && !isSecret" class="config-field-default"> 默认: {{ defaultValueText }}</span>
      <span v-if="field.source === 'bootstrap'" class="config-readonly-note">
        由 config.yaml 提供，修改后重启生效
      </span>
      <FieldError :message="error" />
    </div>
  </div>
</template>

<script setup>
import { computed } from 'vue'
import FieldControl from '../FieldControl.vue'
import FieldError from '../FieldError.vue'
import {
  defaultText,
  effectClass,
  effectLabel,
  hasFieldDefault,
  isSecretField,
  kindClass,
} from '../../util/configSchema.js'

const props = defineProps({
  field: { type: Object, required: true },
  value: { default: null },
  error: { type: String, default: '' },
  disabled: { type: Boolean, default: false },
  highlight: { type: Function, required: true },
})
defineEmits(['update:value', 'invalid', 'reset'])

const isSecret = computed(() => isSecretField(props.field))
// secret 字段再兜一层：schema 里密钥的 default 是空串，hasFieldDefault 通常为真，
// 但「能不能清除已保存的密钥」不该取决于 default 存不存在（与 AI 页同一判据）。
const canReset = computed(() => hasFieldDefault(props.field) || isSecret.value)
const highlightedLabel = computed(() => props.highlight(props.field.label))
const defaultValueText = computed(() => defaultText(props.field))

/* 措辞统一（审计 O1）：secret 没有「默认值」可言（后端不会把密钥明文下发），
 * 「恢复默认」在这里的真实语义是**清除**已保存的密钥 —— 后端只认 {"__clear__": true}，
 * 写空串会被当成「保留旧值」，用户会以为恢复了其实没生效。文案与 AI 页保持一致。 */
const resetLabel = computed(() => (isSecret.value ? '清除' : '恢复默认'))
const resetTitle = computed(() =>
  isSecret.value
    ? '清除已保存的密钥（提交 {"__clear__": true}，尚未保存）'
    : '将本字段恢复为 schema 默认值（尚未保存）',
)
</script>
