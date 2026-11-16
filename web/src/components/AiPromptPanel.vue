<template>
  <details class="panel ai-prompt-panel">
    <summary class="ai-prompt-summary">装配预览（dry-run，不写配置、不调用模型）</summary>
    <div class="panel-body">
      <p class="config-field-hint">
        用当前页面未保存的 ai / ai_chat / tts 草稿拼出一轮请求，看看会发什么。
      </p>
      <div class="ai-prompt-form">
        <label>
          提问者
          <input v-model="asker" type="text" placeholder="预览玩家" />
        </label>
        <label class="ai-prompt-question">
          问题
          <input v-model="question" type="text" placeholder="（预览问题）" />
        </label>
        <button class="btn primary" type="button" :disabled="loading" @click="generate">
          {{ loading ? '生成中…' : '生成预览' }}
        </button>
      </div>

      <div v-if="error" class="config-message err">{{ error }}</div>
      <template v-if="preview">
        <div v-if="warnings.length" class="ai-prompt-warnings">
          <strong>警告</strong>
          <ul>
            <li v-for="(warning, index) in warnings" :key="index">{{ warning }}</li>
          </ul>
        </div>

        <div class="ai-prompt-totals">
          <span>system {{ number(preview.totals?.systemChars) }} 字</span>
          <span>tail {{ number(preview.totals?.tailChars) }} 字</span>
          <span>稳定前缀 {{ number(preview.totals?.stablePrefixBytes) }} B</span>
          <span>尾部约 {{ number(preview.totals?.tailTokens) }} token</span>
          <span>前缀指纹 <code>{{ preview.totals?.prefixFingerprint || '—' }}</code></span>
          <span>工具：{{ (preview.tools || []).join('、') || '无' }}</span>
        </div>

        <div class="ai-prompt-blocks">
          <strong>提示词块</strong>
          <table>
            <thead>
              <tr><th>名称</th><th>类型</th><th>来源</th><th>字符</th><th>token</th></tr>
            </thead>
            <tbody>
              <tr v-for="(block, index) in preview.blocks || []" :key="index">
                <td>{{ block.name }}</td>
                <td>{{ block.kind }}</td>
                <td><code>{{ block.source }}</code></td>
                <td>{{ block.chars }}</td>
                <td>{{ block.tokens }}</td>
              </tr>
            </tbody>
          </table>
        </div>

        <details class="ai-prompt-block" open>
          <summary>system（{{ number(preview.totals?.systemChars) }} 字）</summary>
          <pre>{{ preview.system }}</pre>
        </details>
        <details class="ai-prompt-block">
          <summary>tail（{{ number(preview.totals?.tailChars) }} 字）</summary>
          <pre>{{ preview.tail }}</pre>
        </details>
      </template>
    </div>
  </details>
</template>

<script setup>
import { computed, ref } from 'vue'
import { requestJson } from '../api.js'
import { toast } from '../store.js'

const props = defineProps({
  sections: { type: Object, required: false, default: () => ({}) },
})

const asker = ref('预览玩家')
const question = ref('（预览问题）')
const loading = ref(false)
const error = ref('')
const preview = ref(null)
const warnings = computed(() => (Array.isArray(preview.value?.warnings) ? preview.value.warnings : []))

function clone(value) {
  return JSON.parse(JSON.stringify(value == null ? {} : value))
}

function number(value) {
  return value === null || value === undefined ? '—' : String(value)
}

async function generate() {
  loading.value = true
  error.value = ''
  try {
    const sections = clone(props.sections || {})
    for (const name of ['ai', 'ai_chat', 'tts']) {
      if (!sections[name] || typeof sections[name] !== 'object') sections[name] = {}
    }
    preview.value = await requestJson('/ai/chat/preview', {
      asker: String(asker.value || '').trim() || '预览玩家',
      question: String(question.value || '').trim() || '（预览问题）',
      sections,
    })
  } catch (e) {
    preview.value = null
    error.value = `生成预览失败：${e.message || e}`
    toast(error.value, 'err')
  } finally {
    loading.value = false
  }
}
</script>

<style scoped>
.ai-prompt-panel { margin-top: var(--sp-3); }
.ai-prompt-summary { cursor: pointer; padding: var(--sp-3); font-weight: 600; color: var(--text-1); }
.ai-prompt-form { display: flex; gap: var(--sp-2); align-items: end; flex-wrap: wrap; }
.ai-prompt-form label { display: flex; flex-direction: column; gap: 3px; font-size: var(--fs-meta); color: var(--text-2); }
.ai-prompt-form input { min-width: 180px; }
.ai-prompt-question { flex: 1; min-width: 220px; }
.ai-prompt-question input { width: 100%; }
.ai-prompt-warnings { border: 1px solid var(--warn); border-radius: var(--radius-m); padding: var(--sp-2) var(--sp-3); background: var(--warn-soft); }
.ai-prompt-warnings ul { margin: var(--sp-1) 0 0; padding-left: 18px; }
.ai-prompt-totals { display: flex; gap: var(--sp-2) var(--sp-3); flex-wrap: wrap; margin-top: var(--sp-2); font-size: var(--fs-meta); color: var(--text-2); }
.ai-prompt-blocks { margin-top: var(--sp-2); }
.ai-prompt-blocks table { width: 100%; border-collapse: collapse; margin-top: var(--sp-1); font-size: var(--fs-tiny); }
.ai-prompt-blocks th, .ai-prompt-blocks td { text-align: left; padding: 4px 6px; border-bottom: 1px solid var(--sep-2); }
.ai-prompt-blocks th { color: var(--text-3); font-weight: 600; }
.ai-prompt-block { margin-top: var(--sp-2); border: 1px solid var(--sep-2); border-radius: var(--radius-m); padding: var(--sp-2) var(--sp-3); }
.ai-prompt-block summary { cursor: pointer; font-weight: 600; }
.ai-prompt-block pre { white-space: pre-wrap; word-break: break-word; max-height: 360px; overflow: auto; margin: var(--sp-1) 0 0; font-family: var(--font-mono); font-size: var(--fs-tiny); line-height: var(--lh-tiny); }
</style>
