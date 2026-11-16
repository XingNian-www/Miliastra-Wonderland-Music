<template>
  <div class="ai-model-picker">
    <div class="model-picker">
      <FieldControl
        :field="field"
        :model-value="modelValue"
        @update:model-value="(value) => emit('update:modelValue', value)"
      />
      <select :disabled="loading || !models.length" @change="pickModel">
        <option value="">{{ models.length ? '从列表选择…' : '（尚未获取模型列表）' }}</option>
        <option v-for="model in models" :key="model" :value="model">{{ model }}</option>
      </select>
      <button class="btn small ghost" type="button" :disabled="loading" @click="fetchModels">
        {{ loading ? '获取中…' : '获取模型列表' }}
      </button>
    </div>
    <span v-if="error" class="config-field-hint" style="color: var(--danger)">{{ error }}</span>
    <span v-else class="config-field-hint">{{ status }}</span>
  </div>
</template>

<script setup>
import { computed, ref } from 'vue'
import { requestJson } from '../api.js'
import { toast } from '../store.js'
import FieldControl from './FieldControl.vue'

const props = defineProps({
  field: { type: Object, required: true },
  modelValue: { default: null },
  overrides: { type: Object, default: () => ({}) },
})

const emit = defineEmits(['update:modelValue'])

const models = ref([])
const loading = ref(false)
const error = ref('')

const status = computed(() => {
  if (!models.value.length) return '模型列表尚未获取，可直接手填模型名'
  return `已获取 ${models.value.length} 个模型；下拉只改草稿，保存请点页面保存按钮`
})

async function fetchModels() {
  if (loading.value) return
  loading.value = true
  error.value = ''
  try {
    const data = await requestJson('/ai/models', props.overrides || {})
    const list = Array.isArray(data && data.models) ? data.models.filter((item) => typeof item === 'string' && item) : []
    if (!list.length) throw new Error('服务端未返回模型')
    models.value = list
    toast(`已获取 ${list.length} 个模型`, 'ok')
  } catch (e) {
    models.value = []
    error.value = `获取模型列表失败：${e.message || e}（仍可手动填写）`
    toast(error.value, 'err')
  } finally {
    loading.value = false
  }
}

function pickModel(event) {
  const value = event.target.value
  if (!value) return
  emit('update:modelValue', value)
  event.target.value = ''
}
</script>

<style scoped>
.ai-model-picker { display: flex; flex-direction: column; gap: 3px; min-width: 0; width: 100%; }
.ai-model-picker .model-picker { width: 100%; }
.ai-model-picker .model-picker input { width: auto; flex: 1 1 180px; }
.ai-model-picker .model-picker select { width: auto; flex: 0 0 auto; min-width: 150px; }
.ai-model-picker .model-picker .btn { width: auto; flex: 0 0 auto; }
</style>
