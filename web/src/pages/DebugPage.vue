<template>
  <h1 class="page-title">调试沙盒</h1>
  <p class="page-subtitle">只读诊断、截图、主动读取与调试命令；写操作都会二次确认</p>

  <div class="debug-layout">
    <aside class="panel debug-nav">
      <div class="panel-body">
        <div class="debug-tabs">
          <button v-for="tab in tabs" :key="tab.key" class="btn small" :class="{ primary: tabKey === tab.key }" @click="tabKey = tab.key">{{ tab.label }}</button>
        </div>
        <p class="config-field-hint">{{ currentTab.hint }}</p>
        <div class="debug-endpoints">
          <button
            v-for="item in currentTab.items"
            :key="item.path + item.method"
            type="button"
            class="debug-endpoint"
            :class="{ active: activeItem && itemKey(activeItem) === itemKey(item) }"
            @click="activeKey = itemKey(item)"
          >
            <span class="debug-endpoint-label">{{ item.label }}</span>
            <span class="config-field-hint">{{ item.method }} {{ item.path }}</span>
          </button>
        </div>
      </div>
    </aside>

    <section v-if="activeItem" class="panel debug-runner">
      <div class="panel-head">
        <h2>{{ activeItem.label }}</h2>
        <span class="config-effect">{{ activeItem.method }} {{ activeItem.path }}</span>
      </div>
      <div class="panel-body">
        <div v-if="activeItem.query && activeItem.query.length" class="debug-params">
          <label v-for="param in activeItem.query" :key="param.key" class="debug-param">
            <span>{{ param.key }}</span>
            <input v-model="param.value" :placeholder="param.placeholder || ''" />
          </label>
        </div>
        <textarea
          v-if="activeItem.method === 'POST'"
          v-model="activeItem.body"
          class="config-textarea debug-body"
          placeholder='请求体 JSON，例如 {"command":"click","x":10,"y":20}'
        ></textarea>
        <div class="debug-run-actions">
          <button class="btn small" :disabled="busy === activeItem" @click="run(activeItem)">{{ busy === activeItem ? '执行中…' : '执行' }}</button>
          <span v-if="resultOf[itemKey(activeItem)]?.status" class="config-field-hint">{{ resultOf[itemKey(activeItem)].status }}</span>
          <span style="flex:1"></span>
          <button v-if="resultOf[itemKey(activeItem)]?.text" class="btn small ghost" type="button" @click="clearResult(activeItem)">清空结果</button>
        </div>
        <div v-if="images[itemKey(activeItem)]" class="debug-image">
          <img :src="images[itemKey(activeItem)]" alt="debug screenshot" />
        </div>
        <pre v-if="resultOf[itemKey(activeItem)]?.text" class="debug-result">{{ resultOf[itemKey(activeItem)].text }}</pre>
        <p v-else class="config-field-hint">还没有响应；点「执行」后结果固定显示在这里。</p>
      </div>
    </section>
  </div>
</template>

<script setup>
import { computed, reactive, ref, watch } from 'vue'
import { readToken } from '../api.js'
import { getJson, request, requestJson } from '../api.js'
import { handleAuth, state, toast } from '../store.js'

const tabKey = ref('readonly')
const WRITE_PATHS = new Set([
  '/debug/run','/debug/run/all','/debug/run/step','/debug/run/cancel','/debug/run/status','/debug/run/replay','/debug/mode','/decisions/submit',
])
const busy = ref(null)
const activeKey = ref('')
const resultOf = reactive({})
const images = reactive({})

function q(key, placeholder) { return { key, value: '', placeholder } }
function item(label, method, path, options = {}) {
  return { label, method, path, query: options.query || [], body: options.body || '{}' }
}

const tabs = [
  {
    key: 'readonly',
    label: '只读诊断',
    hint: 'GET 请求，不触发游戏输入；不需要调试模式。',
    items: [
      item('运行状态', 'GET', '/debug/state'),
      item('界面判定', 'GET', '/debug/ui-state', { query: [q('image', '可选：图片路径')] }),
      item('窗口信息', 'GET', '/debug/window'),
      item('任务队列', 'GET', '/debug/tasks'),
      item('运行记录列表', 'GET', '/debug/runs'),
      item('界面目录', 'GET', '/debug/ui-catalogue', { query: [q('image', '可选：图片路径')] }),
      item('只读观察摘要', 'GET', '/debug/read-only/summary'),
      item('只读观察条目', 'GET', '/debug/read-only/entries', { query: [q('since', '游标'), q('limit', '1..50'), q('taskId', '可选')] }),
      item('只读观察模式', 'GET', '/debug/read-only/mode'),
    ],
  },
  {
    key: 'capture',
    label: '截图',
    hint: '截图是只读操作，不需要调试模式。',
    items: [item('当前画面截图', 'GET', '/debug/screenshot')],
  },
  {
    key: 'read',
    label: '主动读取',
    hint: '只读读取不需要调试模式；切 mode 会改变观察状态（不向游戏发输入）。',
    items: [
      item('读取区域/OCR', 'POST', '/debug/read-only/read', { body: '{"label":"debug","rect":{"x":0,"y":0,"width":800,"height":600},"ocr":true}' }),
      item('切换只读模式', 'POST', '/debug/read-only/mode', { body: '{"enabled":true,"state":"active"}' }),
      item('读取证据', 'GET', '/debug/read-only/evidence', { query: [q('id', '证据ID'), q('section', 'metadata/ocr/parameters/history/image'), q('cursor')] }),
      item('证据原图', 'GET', '/debug/read-only/image', { query: [q('id', '证据ID')] }),
      item('模拟聊天', 'POST', '/debug/simulate-chat', { body: '{"role":"user","text":"[测试玩家]：你好"}' }),
    ],
  },
  {
    key: 'write',
    label: '命令与运行',
    hint: '只有这些输入类命令需要调试模式；确认后再执行。',
    items: [
      item('调试命令', 'POST', '/debug/command', { body: '{"command":"click","x":100,"y":200}' }),
      item('保存调试任务', 'POST', '/debug/tasks', { body: '{}' }),
      item('运行调试任务', 'POST', '/debug/run', { body: '{"taskId":"demo","mode":"simulated"}' }),
      item('运行全部', 'POST', '/debug/run/all', { query: [q('runId', '运行ID')] }),
      item('单步', 'POST', '/debug/run/step', { query: [q('runId', '运行ID')] }),
      item('取消', 'POST', '/debug/run/cancel', { query: [q('runId', '运行ID')] }),
      item('运行状态', 'POST', '/debug/run/status', { query: [q('id', '运行ID')] }),
      item('运行证据', 'GET', '/debug/run/evidence', { query: [q('id', '运行ID'), q('index', '步骤序号')] }),
      item('回放', 'POST', '/debug/run/replay', { query: [q('id', '运行ID')] }),
      item('调试事件回放', 'GET', '/debug/events', { query: [q('since', '游标'), q('limit', '1..50')] }),
      item('调试模式', 'GET', '/debug/mode', { query: [q('enabled', 'true 或 false')] }),
      item('决策提交', 'POST', '/decisions/submit', { query: [q('id', '决策ID'), q('action', 'confirm/skip/switch_source/ai')] }),
    ],
  },
]

const currentTab = computed(() => tabs.find((tab) => tab.key === tabKey.value) || tabs[0])
const activeItem = computed(() => currentTab.value.items.find((item) => itemKey(item) === activeKey.value) || currentTab.value.items[0] || null)

watch(
  () => tabKey.value,
  () => {
    activeKey.value = currentTab.value.items.length ? itemKey(currentTab.value.items[0]) : ''
  },
  { immediate: true },
)

function itemKey(item) { return `${item.method} ${item.path}` }

function clearResult(item) {
  const key = itemKey(item)
  resultOf[key] = { status: '', text: '' }
  delete images[key]
}

function buildUrl(item) {
  const params = new URLSearchParams()
  for (const param of item.query || []) {
    if (param.value !== '' && param.value != null) params.set(param.key, param.value)
  }
  const query = params.toString()
  return query ? `${item.path}?${query}` : item.path
}

async function run(item) {
  const key = itemKey(item)
  const write = item.method === 'POST' || WRITE_PATHS.has(item.path)
  if (write && !window.confirm(`确认执行 ${item.method} ${item.path}？`)) return
  busy.value = item
  resultOf[key] = { status: '', text: '' }
  try {
    if (item.path === '/debug/screenshot') {
      const response = await fetch(item.path, { headers: { 'X-Miliastra-Token': readToken() } })
      if (!response.ok) {
        if (response.status === 401) {
          state.needsToken = true
          state.autoRefresh = false
          throw new Error('访问令牌无效或已过期，请到设置页重新填写')
        }
        throw new Error(`HTTP ${response.status}`)
      }
      const blob = await response.blob()
      if (images[key]) URL.revokeObjectURL(images[key])
      images[key] = URL.createObjectURL(blob)
      resultOf[key] = { status: `图片 ${Math.round(blob.size / 1024)} KB`, text: '' }
      return
    }
    const url = buildUrl(item)
    if (item.path === '/debug/events') {
      // SSE 文本流：不要按 JSON 解析。
      const text = await request(url)
      resultOf[key] = { status: 'OK（SSE 文本）', text }
      return
    }
    const value = item.method === 'GET'
      ? await getJson(url)
      : await requestJson(url, parseBody(item.body))
    resultOf[key] = { status: 'OK', text: JSON.stringify(value, null, 2) }
  } catch (error) {
    resultOf[key] = { status: '失败', text: String(error && error.message || error) }
    toast(`调试接口失败：${error.message || error}`, 'err')
  } finally {
    busy.value = null
  }
}

function parseBody(text) {
  const value = String(text || '').trim()
  if (!value) return {}
  try {
    return JSON.parse(value)
  } catch {
    throw new Error('请求体不是合法 JSON')
  }
}
</script>

<style scoped>
.debug-tabs { display: flex; gap: 8px; flex-wrap: wrap; }
.debug-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(420px, 100%), 1fr)); gap: 12px; padding: 0 24px 24px; align-items: start; }
.debug-params { display: flex; flex-wrap: wrap; gap: 8px; margin-bottom: 8px; }
.debug-param { display: flex; align-items: center; gap: 6px; font-size: var(--fs-tiny); }
.debug-param input { width: 140px; }
.debug-result { margin: 8px 0 0; max-height: 280px; overflow: auto; background: var(--bg-1); border-radius: 8px; padding: 8px; font-size: var(--fs-tiny); white-space: pre-wrap; word-break: break-all; }
</style>
<style scoped>
.debug-layout { display: grid; grid-template-columns: minmax(240px, 300px) minmax(0, 1fr); gap: 12px; padding: 0 22px 24px; align-items: start; }
.debug-nav { position: sticky; top: 76px; }
.debug-tabs { display: flex; flex-wrap: wrap; gap: 4px; margin-bottom: 6px; }
.debug-endpoints { display: flex; flex-direction: column; gap: 3px; max-height: calc(100vh - 260px); overflow: auto; margin-top: 8px; }
.debug-endpoint { display: flex; flex-direction: column; gap: 1px; width: 100%; text-align: left; padding: 6px 9px; border: 1px solid transparent; border-radius: 8px; background: rgba(255, 255, 255, 0.02); color: var(--text-1); cursor: pointer; }
.debug-endpoint:hover { background: rgba(255, 255, 255, 0.05); }
.debug-endpoint.active { border-color: rgba(91, 140, 255, 0.5); background: var(--accent-soft); }
.debug-endpoint-label { font-size: 12px; }
.debug-endpoint .config-field-hint { font-size: 10.5px; }
.debug-runner { position: sticky; top: 76px; }
.debug-params { display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 8px; margin-bottom: 8px; }
.debug-param { display: flex; flex-direction: column; gap: 3px; font-size: 11px; color: var(--text-2); }
.debug-body { width: 100%; min-height: 80px; font-family: var(--font-mono); }
.debug-run-actions { display: flex; align-items: center; gap: 8px; margin-top: 8px; }
.debug-image img { max-width: 100%; border-radius: 8px; margin-top: 8px; }
.debug-result { margin: 10px 0 0; max-height: calc(100vh - 340px); overflow: auto; background: var(--bg-1); border-radius: 8px; padding: 10px; font-size: 11.5px; white-space: pre-wrap; word-break: break-all; }
@media (max-width: 900px) {
  .debug-layout { grid-template-columns: 1fr; padding: 0 16px 24px; }
  .debug-nav, .debug-runner { position: static; }
  .debug-endpoints { max-height: 240px; }
}
</style>
<style scoped>
.debug-tabs { display: flex; gap: 8px; flex-wrap: wrap; }
.debug-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(420px, 100%), 1fr)); gap: 12px; padding: 0 24px 24px; align-items: start; }
.debug-params { display: flex; flex-wrap: wrap; gap: 8px; margin-bottom: 8px; }
.debug-param { display: flex; align-items: center; gap: 6px; font-size: var(--fs-tiny); }
.debug-param input { width: 140px; }
.debug-result { margin: 8px 0 0; max-height: 280px; overflow: auto; background: var(--bg-1); border-radius: 8px; padding: 8px; font-size: var(--fs-tiny); white-space: pre-wrap; word-break: break-all; }
</style>
