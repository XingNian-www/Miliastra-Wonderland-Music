<template>
  <section class="panel ai-trace-panel">
    <div class="panel-head">
      <h2>AI 请求记录</h2>
      <button class="btn small" type="button" :disabled="loading" @click="load">刷新</button>
    </div>
    <div class="panel-body">
      <div v-if="error" class="config-message err">{{ error }}</div>
      <div v-if="loading" class="loading-card">正在读取请求记录…</div>

      <template v-else>
        <p class="config-field-hint">
          容量 {{ capacity }} 条，只存内存；敏感字段由后端脱敏。
        </p>
        <p class="config-field-hint">
          最近 {{ stats.requests || 0 }} 次请求 · 输入 {{ stats.promptTokens || 0 }} token ·
          缓存命中 {{ stats.cachedTokens || 0 }} token · 总命中率 {{ ratio(stats.cacheHitRatio) }} ·
          不同稳定前缀 {{ stats.distinctPrefixes || 0 }} 个
        </p>

        <p v-if="!entries.length" class="config-field-hint">
          还没有 AI 请求。触发一次 AI 聊天后点「刷新」。
        </p>
        <div v-else class="ai-trace-list">
          <div
            v-for="entry in entries"
            :key="entry.id"
            class="ai-trace-row"
            :class="{ selected: selectedId === entry.id }"
          >
            <div class="ai-trace-row-main">
              <span class="config-field-hint">#{{ entry.id }}</span>
              <span class="config-field-hint">{{ entry.startedAt }}</span>
              <span :class="entry.ok ? 'ai-trace-ok' : 'ai-trace-fail'">{{ entry.ok ? '✓' : '✗' }}</span>
              <strong>{{ entry.model || '（未命名模型）' }}</strong>
              <span class="config-field-hint">
                {{ entry.wireApi }} · {{ entry.durationMs }} ms ·
                输入 {{ tokenText(entry.promptTokens) }} / 输出 {{ tokenText(entry.completionTokens) }} /
                命中 {{ tokenText(entry.cachedTokens) }}（{{ ratio(entry.cacheHitRatio) }}）·
                {{ entry.messageCount }} 条消息 · 稳定前缀 {{ entry.stablePrefixBytes }} B ·
                尾部约 {{ entry.tailTokens }} token · 全量 {{ entry.requestBytes }} B ·
                指纹 <code>{{ entry.prefixFingerprint }}</code>
              </span>
            </div>
            <button class="btn small" type="button" @click="view(entry.id)">查看</button>
          </div>
        </div>

        <div v-if="detailLoading" class="loading-card">正在读取报文详情…</div>
        <div v-else-if="detail" class="ai-trace-detail">
          <div class="ai-trace-detail-head">
            <strong>#{{ detail.id }} {{ detail.model }}</strong>
            <span class="config-field-hint">
              {{ detail.wireApi }}<template v-if="detail.reasoning"> · reasoning={{ detail.reasoning }}</template>
              · 耗时 {{ detail.durationMs }} ms · {{ detail.ok ? '成功' : '失败' }}
            </span>
          </div>
          <p class="config-field-hint">
            开始 {{ detail.startedAt }} · 输入 {{ tokenText(detail.promptTokens) }} token ·
            输出 {{ tokenText(detail.completionTokens) }} token ·
            缓存命中 {{ tokenText(detail.cachedTokens) }}（{{ ratio(detail.cacheHitRatio) }}）·
            稳定前缀 {{ detail.prefixFingerprint }}（{{ detail.stablePrefixBytes }} B）·
            尾部约 {{ detail.tailTokens }} token
          </p>
          <p v-if="detail.error" class="config-message err">{{ detail.error }}</p>

          <details class="ai-trace-block" open>
            <summary>请求参数（除 model/system/messages/tools）</summary>
            <pre>{{ pretty(detail.parameters) }}</pre>
          </details>
          <details v-if="detail.system" class="ai-trace-block" open>
            <summary>system prompt</summary>
            <pre>{{ detail.system }}</pre>
          </details>
          <details class="ai-trace-block" open>
            <summary>messages（{{ messageList(detail.messages).length }} 条，含工具调用/结果；不含 system）</summary>
            <pre>{{ messagesText(detail.messages) }}</pre>
          </details>
          <details class="ai-trace-block">
            <summary>tools（{{ toolCount(detail.tools) }} 个）</summary>
            <pre>{{ pretty(detail.tools) }}</pre>
          </details>
          <details v-if="detail.usage" class="ai-trace-block">
            <summary>provider 原始 usage（含缓存字段）</summary>
            <pre>{{ pretty(detail.usage) }}</pre>
          </details>
        </div>
      </template>
    </div>
  </section>
</template>

<script setup>
import { onMounted, ref } from 'vue'
import { getJson } from '../api.js'
import { toast } from '../store.js'

const loading = ref(false)
const detailLoading = ref(false)
const error = ref('')
const entries = ref([])
const stats = ref({})
const capacity = ref(0)
const selectedId = ref(null)
const detail = ref(null)

function ratio(value) {
  if (value === null || value === undefined || !Number.isFinite(Number(value))) return '—'
  return `${Math.round(Number(value) * 1000) / 10}%`
}

function tokenText(value) {
  return value === null || value === undefined ? '—' : String(value)
}

function pretty(value) {
  try {
    return JSON.stringify(value, null, 2)
  } catch {
    return String(value)
  }
}

function messageList(messages) {
  return Array.isArray(messages) ? messages : []
}

function toolCount(tools) {
  return Array.isArray(tools) ? tools.length : 0
}

function contentText(content) {
  if (content === null || content === undefined) return ''
  if (typeof content === 'string') return content
  if (!Array.isArray(content)) return pretty(content)
  return content
    .map((part) => {
      if (part && typeof part.text === 'string') return part.text
      if (part && (part.type === 'image_url' || part.type === 'input_image')) return '[图片已省略]'
      return pretty(part)
    })
    .join('\n')
}

function messageText(message) {
  if (message === null || message === undefined) return ''
  if (typeof message === 'string') return message
  const content = message.content
  if (typeof content === 'string') return content
  if (Array.isArray(content)) return contentText(content)
  const rest = { ...message }
  delete rest.role
  delete rest.content
  if (message.tool_calls) return pretty(message.tool_calls)
  return Object.keys(rest).length ? pretty(rest) : ''
}

function messagesText(messages) {
  return messageList(messages)
    .map((message) => {
      const role = (message && (message.role || message.type)) || 'item'
      return `【${role}】\n${messageText(message)}`
    })
    .join('\n\n')
}

async function load() {
  loading.value = true
  error.value = ''
  try {
    const data = await getJson('/ai/trace')
    entries.value = Array.isArray(data.entries) ? data.entries : []
    stats.value = data.stats || {}
    capacity.value = data.capacity || 0
    detail.value = null
    selectedId.value = null
    if (entries.value.length) await view(entries.value[0].id)
  } catch (e) {
    error.value = `读取请求记录失败：${e.message || e}`
    toast(error.value, 'err')
    entries.value = []
    detail.value = null
  } finally {
    loading.value = false
  }
}

async function view(id) {
  selectedId.value = id
  detailLoading.value = true
  detail.value = null
  try {
    detail.value = await getJson(`/ai/trace/entry?id=${encodeURIComponent(id)}`)
  } catch (e) {
    toast(`读取报文失败：${e.message || e}`, 'err')
    selectedId.value = null
  } finally {
    detailLoading.value = false
  }
}

onMounted(load)
</script>

<style scoped>
.ai-trace-panel { margin-top: var(--sp-3); }
.ai-trace-list { display: flex; flex-direction: column; gap: var(--sp-1); }
.ai-trace-row { display: flex; gap: var(--sp-2); align-items: center; justify-content: space-between; padding: var(--sp-2); border: 1px solid var(--sep-2); border-radius: var(--radius-m); background: var(--bg-card); }
.ai-trace-row.selected { border-color: var(--accent); }
.ai-trace-row-main { display: flex; gap: var(--sp-2); align-items: baseline; flex-wrap: wrap; min-width: 0; }
.ai-trace-ok { color: var(--ok); font-weight: 600; }
.ai-trace-fail { color: var(--danger); font-weight: 600; }
.ai-trace-detail { margin-top: var(--sp-3); }
.ai-trace-detail-head { display: flex; gap: var(--sp-2); align-items: baseline; flex-wrap: wrap; }
.ai-trace-block { border: 1px solid var(--sep-2); border-radius: var(--radius-m); padding: var(--sp-2) var(--sp-3); margin-top: var(--sp-2); }
.ai-trace-block summary { cursor: pointer; font-weight: 600; color: var(--text-1); }
.ai-trace-block pre { white-space: pre-wrap; word-break: break-word; max-height: 360px; overflow: auto; margin: var(--sp-1) 0 0; font-family: var(--font-mono); font-size: var(--fs-tiny); line-height: var(--lh-tiny); }
</style>
