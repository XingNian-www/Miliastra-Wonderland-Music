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
        <div class="ai-trace-stats">
          <span>容量 {{ capacity }} 条</span>
          <span>最近 {{ stats.requests || 0 }} 次</span>
          <span>输入 {{ tokenText(stats.promptTokens) }} token</span>
          <span>缓存命中 {{ tokenText(stats.cachedTokens) }}（{{ ratio(stats.cacheHitRatio) }}）</span>
          <span>稳定前缀 {{ stats.distinctPrefixes || 0 }} 个</span>
        </div>

        <p v-if="!entries.length" class="config-field-hint">
          还没有 AI 请求。触发一次 AI 聊天后点「刷新」。
        </p>

        <div v-else class="ai-trace-layout">
          <aside class="ai-trace-list-wrap">
            <input v-model="search" placeholder="搜索 #id / 模型 / 指纹…" aria-label="搜索请求" />
            <div class="ai-trace-filter">
              <button class="tab" :class="{ active: statusFilter === 'all' }" type="button" @click="statusFilter = 'all'">全部</button>
              <button class="tab" :class="{ active: statusFilter === 'ok' }" type="button" @click="statusFilter = 'ok'">成功</button>
              <button class="tab" :class="{ active: statusFilter === 'fail' }" type="button" @click="statusFilter = 'fail'">失败</button>
            </div>
            <div class="ai-trace-list">
              <div
                v-for="entry in filteredEntries"
                :key="entry.id"
                class="ai-trace-row"
                :class="{ selected: selectedId === entry.id }"
                role="button"
                tabindex="0"
                @click="view(entry.id)"
                @keydown.enter.prevent="view(entry.id)"
                @keydown.space.prevent="view(entry.id)"
              >
                <div class="ai-trace-row-top">
                  <span :class="entry.ok ? 'ai-trace-ok' : 'ai-trace-fail'">{{ entry.ok ? '✓' : '✗' }}</span>
                  <strong>#{{ entry.id }}</strong>
                  <span class="config-field-hint">{{ entry.startedAt }}</span>
                </div>
                <div class="ai-trace-row-model">{{ entry.model || '（未命名模型）' }}</div>
                <div class="config-field-hint">
                  {{ entry.durationMs }} ms · {{ entry.messageCount }} 条消息 ·
                  输入 {{ tokenText(entry.promptTokens) }} / 命中 {{ ratio(entry.cacheHitRatio) }}
                </div>
              </div>
              <p v-if="!filteredEntries.length" class="config-field-hint">没有匹配的请求</p>
            </div>
          </aside>

          <div ref="detailBox" class="ai-trace-detail-wrap">
            <div v-if="!detail && !detailLoading" class="empty-hint">从左侧选择一条请求查看报文</div>
            <div v-else-if="detailLoading" class="loading-card">正在读取报文详情…</div>
            <template v-else-if="detail">
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
            </template>
          </div>
        </div>
      </template>
    </div>
  </section>
</template>

<script setup>
import { computed, nextTick, onMounted, ref } from 'vue'
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
const search = ref('')
const statusFilter = ref('all')
const detailBox = ref(null)

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

const filteredEntries = computed(() => {
  const keyword = search.value.trim().toLowerCase()
  return entries.value.filter((entry) => {
    if (statusFilter.value === 'ok' && !entry.ok) return false
    if (statusFilter.value === 'fail' && entry.ok) return false
    if (!keyword) return true
    return `${entry.id} ${entry.model || ''} ${entry.prefixFingerprint || ''} ${entry.startedAt || ''}`.toLowerCase().includes(keyword)
  })
})

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
  if (window.innerWidth < 900) {
    await nextTick()
    detailBox.value?.scrollIntoView({ block: 'start', behavior: 'smooth' })
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
<style scoped>
.ai-trace-stats { display: flex; flex-wrap: wrap; gap: 6px 16px; color: var(--text-2); font-size: 11.5px; margin-bottom: 10px; }
.ai-trace-layout { display: grid; grid-template-columns: minmax(240px, 320px) minmax(0, 1fr); gap: 12px; align-items: start; }
.ai-trace-list-wrap { display: flex; flex-direction: column; gap: 6px; position: sticky; top: 76px; }
.ai-trace-list-wrap > input { width: 100%; }
.ai-trace-filter { display: flex; gap: 4px; }
.ai-trace-filter .tab { padding: 4px 10px; font-size: 11px; }
.ai-trace-list { display: flex; flex-direction: column; gap: 4px; max-height: calc(100vh - 260px); overflow: auto; }
.ai-trace-row { padding: 7px 9px; border: 1px solid transparent; border-radius: 9px; background: rgba(255, 255, 255, 0.02); cursor: pointer; }
.ai-trace-row:hover { background: rgba(255, 255, 255, 0.05); }
.ai-trace-row.selected { border-color: rgba(91, 140, 255, 0.5); background: var(--accent-soft); }
.ai-trace-row-top { display: flex; align-items: center; gap: 6px; font-size: 12px; }
.ai-trace-row-top strong { font-size: 12.5px; }
.ai-trace-row-model { font-size: 11.5px; color: var(--text-2); margin: 2px 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ai-trace-detail-wrap { min-height: 240px; border: 1px solid var(--sep-2); border-radius: 10px; padding: 12px; background: rgba(255, 255, 255, 0.01); }
.ai-trace-detail-head { display: flex; align-items: baseline; gap: 10px; flex-wrap: wrap; margin-bottom: 6px; }
.ai-trace-block pre { max-height: 420px; overflow: auto; }
@media (max-width: 900px) {
  .ai-trace-layout { grid-template-columns: 1fr; }
  .ai-trace-list-wrap { position: static; }
  .ai-trace-list { max-height: 280px; }
  .ai-trace-detail-wrap { min-height: 0; }
}
</style>
