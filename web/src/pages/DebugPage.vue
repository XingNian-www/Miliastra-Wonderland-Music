<template>
  <h1 class="page-title">调试沙盒</h1>
  <p class="page-subtitle">只读诊断、截图、主动读取与调试命令；写操作都会二次确认</p>

  <div class="panel" style="margin:0 24px 16px">
    <div class="panel-body">
      <div class="debug-tabs">
        <button v-for="tab in tabs" :key="tab.key" class="btn small" :class="{ primary: tabKey === tab.key }" @click="tabKey = tab.key">{{ tab.label }}</button>
      </div>
      <p class="config-field-hint">{{ currentTab.hint }}</p>
    </div>
  </div>

  <div class="debug-grid">
    <section v-for="item in currentTab.items" :key="item.path + item.method" class="panel">
      <div class="panel-head">
        <h2>{{ item.label }}</h2>
        <span class="config-effect">{{ item.method }} {{ item.path }}</span>
      </div>
      <div class="panel-body">
        <div v-if="item.query && item.query.length" class="debug-params">
          <label v-for="param in item.query" :key="param.key" class="debug-param">
            <span>{{ param.key }}</span>
            <input v-model="param.value" :placeholder="param.placeholder || ''" />
          </label>
        </div>
        <textarea
          v-if="item.method === 'POST'"
          v-model="item.body"
          class="config-textarea"
          style="width:100%;min-height:70px;font-family:monospace"
          placeholder='请求体 JSON，例如 {"command":"click","x":10,"y":20}'
        ></textarea>
        <div style="display:flex;gap:8px;align-items:center;margin-top:8px">
          <button class="btn small" :disabled="busy === item" @click="run(item)">{{ busy === item ? '执行中…' : '执行' }}</button>
          <span v-if="resultOf[itemKey(item)]?.status" class="config-field-hint">{{ resultOf[itemKey(item)].status }}</span>
        </div>
        <div v-if="images[itemKey(item)]" style="margin-top:8px">
          <img :src="images[itemKey(item)]" style="max-width:100%;border-radius:8px" alt="debug screenshot" />
        </div>
        <pre v-if="resultOf[itemKey(item)]?.text" class="debug-result">{{ resultOf[itemKey(item)].text }}</pre>
      </div>
    </section>
  </div>
</template>

<script setup>
import { computed, reactive, ref } from 'vue'
import { readToken } from '../api.js'
import { getJson, requestJson } from '../api.js'
import { toast } from '../store.js'

const tabKey = ref('readonly')
const busy = ref(null)
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
    hint: 'GET 请求，不触发游戏输入。',
    items: [
      item('运行状态', 'GET', '/debug/state'),
      item('界面判定', 'GET', '/debug/ui-state'),
      item('窗口信息', 'GET', '/debug/window'),
      item('任务队列', 'GET', '/debug/tasks'),
      item('运行记录列表', 'GET', '/debug/runs'),
      item('界面目录', 'GET', '/debug/ui-catalogue'),
      item('只读观察摘要', 'GET', '/debug/read-only/summary'),
      item('只读观察条目', 'GET', '/debug/read-only/entries', { query: [q('cursor'), q('limit')] }),
    ],
  },
  {
    key: 'capture',
    label: '截图',
    hint: '截图接口返回图片；这里用带 token 的 fetch 显示。',
    items: [item('当前画面截图', 'GET', '/debug/screenshot')],
  },
  {
    key: 'read',
    label: '主动读取',
    hint: '只读观察模式下可用；mode 切换会改变运行模式。',
    items: [
      item('读取区域/OCR', 'POST', '/debug/read-only/read', { body: '{"label":"debug","rect":{"x":0,"y":0,"width":800,"height":600},"ocr":true}' }),
      item('切换只读模式', 'POST', '/debug/read-only/mode', { body: '{"enabled":true,"state":"active"}' }),
      item('读取证据', 'GET', '/debug/read-only/evidence', { query: [q('id'), q('section'), q('cursor')] }),
      item('模拟聊天', 'POST', '/debug/simulate-chat', { body: '{"text":"[测试玩家]：你好"}' }),
    ],
  },
  {
    key: 'write',
    label: '命令与运行',
    hint: '这些会真的操作游戏或调试执行器，确认后再执行。',
    items: [
      item('调试命令', 'POST', '/debug/command', { body: '{"command":"click","x":100,"y":200}' }),
      item('运行调试任务', 'POST', '/debug/run', { body: '{}' }),
      item('运行全部', 'GET', '/debug/run/all'),
      item('单步', 'GET', '/debug/run/step'),
      item('取消', 'GET', '/debug/run/cancel'),
      item('运行状态', 'GET', '/debug/run/status'),
      item('运行证据', 'GET', '/debug/run/evidence', { query: [q('id')] }),
      item('回放', 'GET', '/debug/run/replay', { query: [q('id')] }),
      item('调试模式', 'GET', '/debug/mode', { query: [q('enabled', 'true 或 false')] }),
      item('决策提交', 'POST', '/decisions/submit', { body: '{}' }),
    ],
  },
]

const currentTab = computed(() => tabs.find((tab) => tab.key === tabKey.value) || tabs[0])

function itemKey(item) { return `${item.method} ${item.path}` }

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
  const write = item.method === 'POST' || item.path.startsWith('/debug/run') || item.path === '/debug/mode' || item.path === '/decisions/submit'
  if (write && !window.confirm(`确认执行 ${item.method} ${item.path}？`)) return
  busy.value = item
  resultOf[key] = { status: '', text: '' }
  try {
    if (item.path === '/debug/screenshot') {
      const response = await fetch(item.path, { headers: { 'X-Miliastra-Token': readToken() } })
      if (!response.ok) throw new Error(`HTTP ${response.status}`)
      const blob = await response.blob()
      if (images[key]) URL.revokeObjectURL(images[key])
      images[key] = URL.createObjectURL(blob)
      resultOf[key] = { status: `图片 ${Math.round(blob.size / 1024)} KB`, text: '' }
      return
    }
    const value = item.method === 'GET'
      ? await getJson(buildUrl(item))
      : await requestJson(item.path, parseBody(item.body))
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
.debug-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(420px, 1fr)); gap: 12px; padding: 0 24px 24px; align-items: start; }
.debug-params { display: flex; flex-wrap: wrap; gap: 8px; margin-bottom: 8px; }
.debug-param { display: flex; align-items: center; gap: 6px; font-size: var(--fs-tiny); }
.debug-param input { width: 140px; }
.debug-result { margin: 8px 0 0; max-height: 280px; overflow: auto; background: var(--bg-1); border-radius: 8px; padding: 8px; font-size: var(--fs-tiny); white-space: pre-wrap; word-break: break-all; }
</style>
