<template>
  <!-- 按 schema 的 `group` 切块的字段卡片列表。
       同一组名的相邻字段放进同一张卡；搜索时只渲染命中的字段。 -->
  <form id="configForm" class="config-form" @submit.prevent="$emit('submit')">
    <div v-for="group in groups" :key="group.key" class="config-group-card">
      <div v-if="group.name" class="config-group-head">{{ group.name }}</div>
      <ConfigFieldRow
        v-for="field in group.fields"
        :key="field.path"
        :field="field"
        :value="getValue(field)"
        :error="getError(field)"
        :disabled="disabled(field)"
        :highlight="highlight"
        @update:value="$emit('update-field', field, $event)"
        @invalid="$emit('invalid-field', field, $event)"
        @reset="$emit('reset-field', field)"
      />
    </div>
    <div v-if="!visibleFieldCount" class="config-empty">没有匹配的配置字段</div>
    <slot></slot>
  </form>
</template>

<script setup>
import ConfigFieldRow from './ConfigFieldRow.vue'

const props = defineProps({
  groups: { type: Array, default: () => [] },
  visibleFieldCount: { type: Number, default: 0 },
  getValue: { type: Function, required: true },
  getError: { type: Function, required: true },
  isDisabled: { type: Function, required: true },
  highlight: { type: Function, required: true },
})
defineEmits(['submit', 'update-field', 'invalid-field', 'reset-field'])

function disabled(field) {
  return props.isDisabled(field)
}
</script>
