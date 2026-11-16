<template>
  <h1 class="page-title">身份映射</h1>
  <p class="page-subtitle">昵称 ↔ 记忆键</p>
  <div class="loading-card" v-if="!loaded">加载中…</div>
  <div v-else class="panel" style="margin:16px 24px">
    <div class="panel-body">
      <textarea v-model="draft" class="config-textarea" style="min-height:320px;width:100%;font-family:monospace"></textarea>
      <p class="config-field-hint">编辑 identity.mappings（当前 {{ mappingCount }} 条）。保存只提交 identity 段。</p>
      <p v-if="loadError" class="config-message err">{{ loadError }}</p>
      <button class="btn primary" :disabled="busy || !loaded" @click="save">{{ busy ? '保存中…' : '保存身份映射' }}</button>
      <span class="config-field-hint">{{ message }}</span>
    </div>
  </div>
</template>

<script setup>
import { computed, onMounted, ref } from 'vue'
import { getJson } from '../api.js'
import { saveSections, toast } from '../store.js'

const data = ref(null)
const draft = ref('')
const busy = ref(false)
const loaded = ref(false)
const loadError = ref('')
const message = ref('')
const mappingCount = computed(() => {
  try {
    const value = JSON.parse(draft.value)
    return value && typeof value === 'object' ? Object.keys(value).length : 0
  } catch {
    return 0
  }
})

onMounted(async () => {
  try {
    data.value = await getJson('/config/section?name=identity')
    // 接口形状：{ ok, revision, values: { mappings }, fields, ... }。
    // 读错字段会得到空对象，保存时等于清空全部映射——这里必须按 values 读。
    const values = data.value?.values ?? {}
    draft.value = JSON.stringify(values.mappings ?? {}, null, 2)
    loaded.value = true
  } catch (error) {
    loadError.value = `加载失败：${error.message || error}`
    toast(`加载身份映射失败：${error.message || error}`, 'err')
  }
})

async function save() {
  let mappings
  try {
    mappings = JSON.parse(draft.value)
  } catch (error) {
    message.value = `JSON 解析失败：${error.message}`
    return
  }
  if (!loaded.value) {
    message.value = '尚未成功加载身份映射，禁止保存'
    return
  }
  if (Object.keys(mappings || {}).length === 0 && mappingCount.value === 0
      && !window.confirm('当前身份映射为空，保存会清空全部映射，确认继续？')) {
    return
  }
  busy.value = true
  const ok = await saveSections({ identity: { mappings } }, () => {
    message.value = '已保存'
    toast('身份映射已保存')
  })
  if (!ok && !message.value) message.value = '保存失败'
  busy.value = false
}
</script>
