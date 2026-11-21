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
        title="将本字段恢复为 schema 默认值（尚未保存）"
        @click="$emit('reset')"
      >
        恢复默认
      </button>
    </div>
    <div class="config-field-foot">
      <span v-if="field.hint" class="config-field-hint">{{ field.hint }}</span>
      <span v-if="canReset" class="config-field-default"> 默认: {{ defaultValueText }}</span>
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

const canReset = computed(() => hasFieldDefault(props.field))
const highlightedLabel = computed(() => props.highlight(props.field.label))
const defaultValueText = computed(() => defaultText(props.field))
</script>
