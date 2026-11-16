<template>
  <h1 class="page-title">身份映射</h1>
  <p class="page-subtitle">昵称 ↔ 记忆键</p>
  <div class="loading-card" v-if="!data">加载中…</div>
  <div v-else class="panel" style="margin:16px 24px">
    <div class="panel-body">
      <textarea v-model="draft" class="config-textarea" style="min-height:320px;width:100%;font-family:monospace"></textarea>
      <p class="config-field-hint">编辑 identity.mappings（JSON）。保存只提交 identity 段。</p>
      <button class="btn primary" :disabled="busy" @click="save">{{ busy ? '保存中…' : '保存身份映射' }}</button>
      <span class="config-field-hint">{{ message }}</span>
    </div>
  </div>
</template>

<script setup>
import { onMounted, ref } from 'vue'
import { getJson } from '../api.js'
import { saveSections, toast } from '../store.js'

const data = ref(null)
const draft = ref('')
const busy = ref(false)
const message = ref('')

onMounted(async () => {
  try {
    data.value = await getJson('/config/section?name=identity')
    const section = data.value?.section ?? {}
    draft.value = JSON.stringify(section.mappings ?? section, null, 2)
  } catch (error) {
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
  busy.value = true
  const ok = await saveSections({ identity: { mappings } }, () => {
    message.value = '已保存'
    toast('身份映射已保存')
  })
  if (!ok && !message.value) message.value = '保存失败'
  busy.value = false
}
</script>
