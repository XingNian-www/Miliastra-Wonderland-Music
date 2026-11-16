<template>
  <details class="panel ai-tool-playground">
    <summary class="ai-tool-summary">AI 工具试跑（会真实调用已配置的 AI 服务，可能产生费用）</summary>
    <div class="panel-body">
      <p class="config-field-hint">
        只用于调试点歌 AI 的识别/匹配/挑选；结果直接来自 GET 接口，不会写入配置。
      </p>

      <div class="config-string-row">
        <label>
          工具
          <select v-model="mode">
            <option value="recognize">recognize（识别点歌文本）</option>
            <option value="match">match（匹配歌曲身份）</option>
            <option value="pick">pick（从候选中挑选）</option>
          </select>
        </label>
        <button class="btn primary" type="button" :disabled="loading" @click="run">
          {{ loading ? '调用中…' : '执行试跑' }}
        </button>
      </div>

      <template v-if="mode === 'recognize'">
        <label class="ai-tool-field">
          点歌文本
          <textarea v-model="recognizeText" rows="3" placeholder="例如：@点歌 晴天 周杰伦"></textarea>
        </label>
      </template>

      <template v-else-if="mode === 'match'">
        <label class="ai-tool-field">
          玩家请求
          <input v-model="matchRequest" type="text" placeholder="例如：@点歌 晴天" />
        </label>
        <div class="config-string-row">
          <label class="ai-tool-field">
            歌名
            <input v-model="matchSong" type="text" placeholder="可选" />
          </label>
          <label class="ai-tool-field">
            歌手
            <input v-model="matchSinger" type="text" placeholder="可选" />
          </label>
        </div>
      </template>

      <template v-else>
        <label class="ai-tool-field">
          玩家请求
          <input v-model="pickRequest" type="text" placeholder="例如：@点歌 晴天" />
        </label>
        <label class="ai-tool-field">
          候选列表（JSON 数组，结构同 /search/candidates）
          <textarea v-model="pickCandidates" rows="8" placeholder='[{"title":"晴天","artist":"周杰伦"}]'></textarea>
        </label>
      </template>

      <div v-if="error" class="config-message err">{{ error }}</div>
      <div v-if="result !== null" class="ai-tool-result">
        <div class="ai-tool-result-head">
          <strong>返回 JSON</strong>
          <span class="config-field-hint">{{ elapsed }} ms</span>
        </div>
        <pre>{{ pretty(result) }}</pre>
      </div>
    </div>
  </details>
</template>

<script setup>
import { ref } from 'vue'
import { request } from '../api.js'
import { toast } from '../store.js'

const mode = ref('recognize')
const recognizeText = ref('')
const matchRequest = ref('')
const matchSong = ref('')
const matchSinger = ref('')
const pickRequest = ref('')
const pickCandidates = ref('')
const loading = ref(false)
const error = ref('')
const result = ref(null)
const elapsed = ref(0)

function pretty(value) {
  try {
    return JSON.stringify(value, null, 2)
  } catch {
    return String(value)
  }
}

function query(params) {
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (value !== '') search.set(key, value)
  }
  return search.toString()
}

async function run() {
  if (loading.value) return
  error.value = ''
  result.value = null
  let path = ''
  if (mode.value === 'recognize') {
    const text = String(recognizeText.value || '').trim()
    if (!text) {
      error.value = '请填写点歌文本'
      return
    }
    path = `/ai/recognize?${query({ text })}`
  } else if (mode.value === 'match') {
    const request = String(matchRequest.value || '').trim()
    if (!request) {
      error.value = '请填写玩家请求'
      return
    }
    path = `/ai/match?${query({ request, songName: String(matchSong.value || '').trim(), songSinger: String(matchSinger.value || '').trim() })}`
  } else {
    const request = String(pickRequest.value || '').trim()
    const candidatesRaw = String(pickCandidates.value || '').trim()
    if (!request || !candidatesRaw) {
      error.value = '请填写玩家请求与候选 JSON'
      return
    }
    let candidates
    try {
      candidates = JSON.parse(candidatesRaw)
    } catch (e) {
      error.value = `候选 JSON 无效：${e.message}`
      return
    }
    if (!Array.isArray(candidates) || !candidates.length) {
      error.value = '候选 JSON 必须是非空数组'
      return
    }
    path = `/ai/pick?${query({ request, candidates: JSON.stringify(candidates) })}`
  }
  loading.value = true
  const started = performance.now()
  try {
    result.value = await request(path, 'POST')
    elapsed.value = Math.round(performance.now() - started)
  } catch (e) {
    error.value = `调用失败：${e.message || e}`
    toast(error.value, 'err')
  } finally {
    loading.value = false
  }
}
</script>

<style scoped>
.ai-tool-playground { margin-top: var(--sp-3); }
.ai-tool-summary { cursor: pointer; padding: var(--sp-3); font-weight: 600; color: var(--text-1); }
.ai-tool-field { display: flex; flex-direction: column; gap: 3px; font-size: var(--fs-meta); color: var(--text-2); min-width: 0; }
.ai-tool-field input, .ai-tool-field textarea { width: 100%; }
.ai-tool-result { margin-top: var(--sp-2); }
.ai-tool-result-head { display: flex; align-items: baseline; justify-content: space-between; gap: var(--sp-2); }
.ai-tool-result pre { white-space: pre-wrap; word-break: break-word; max-height: 420px; overflow: auto; margin: var(--sp-1) 0 0; font-family: var(--font-mono); font-size: var(--fs-tiny); line-height: var(--lh-tiny); background: var(--bg-raised); border-radius: var(--radius-m); padding: var(--sp-2); }
</style>
