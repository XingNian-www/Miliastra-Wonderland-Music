<template>
  <div id="page-tools">
    <h1 class="page-title">高级控制</h1>
    <p class="page-subtitle">OCR、模板匹配、手动输入与只读观测；写操作会真实操作游戏</p>

    <div class="tools-grid">
      <section class="panel">
        <div class="panel-head"><h2>画面与识别</h2></div>
        <div class="panel-body tools-body">
          <div class="tools-two">
            <button class="btn primary" :disabled="tracking" @click="runTool('/tools/ocr')">全屏 OCR</button>
            <button class="btn" :disabled="tracking" @click="runTool('/tools/scan-chat')">扫描聊天</button>
          </div>
          <div class="tools-grid-row">
            <input v-model="ocrRect" placeholder="OCR 区域 x,y,width,height" />
            <button class="btn" :disabled="tracking" @click="runTool('/tools/ocr', { rect: ocrRect })">区域 OCR</button>
          </div>
          <div class="tools-two">
            <button class="btn" :disabled="tracking" @click="runTool('/tools/ui-state')">检测 UI</button>
            <button class="btn" :disabled="tracking" @click="runTool('/tools/hall-name')">大厅名称</button>
          </div>
          <button class="btn stretch" :disabled="screenshotBusy" @click="openScreenshot">
            {{ screenshotBusy ? '截图中…' : '获取截图' }}
          </button>
        </div>
      </section>

      <section class="panel">
        <div class="panel-head"><h2>模板匹配</h2></div>
        <div class="panel-body tools-body">
          <select v-model="templateName">
            <option value="">（选择模板）</option>
            <option v-for="item in templates" :key="item.name || item[0]" :value="item.name || item[0]">{{ item.label || item[1] || item.name || item[0] }}</option>
          </select>
          <div class="tools-grid-row">
            <input v-model="templateRect" placeholder="匹配区域 x,y,width,height，留空全屏" />
            <button class="btn small ghost" type="button" @click="useConfiguredTemplateRegion">读取配置区域</button>
          </div>
          <input v-model="threshold" placeholder="阈值，留空使用配置" inputmode="decimal" />
          <div class="tools-two">
            <button class="btn" :disabled="tracking" @click="matchTemplate(false)">匹配</button>
            <button class="btn warn" :disabled="tracking" @click="matchTemplate(true)">匹配并点击</button>
          </div>
        </div>
      </section>

      <section class="panel">
        <div class="panel-head"><h2>手动输入</h2><small>会向游戏发送真实输入</small></div>
        <div class="panel-body tools-body">
          <div class="tools-xyz">
            <input v-model="clickX" placeholder="x" inputmode="numeric" aria-label="x 坐标" />
            <input v-model="clickY" placeholder="y" inputmode="numeric" aria-label="y 坐标" />
            <button class="btn warn" :disabled="tracking" @click="clickPoint">点击</button>
          </div>
          <div class="tools-grid-row">
            <input v-model="keyName" placeholder="按键，如 Escape / Return / F2" />
            <button class="btn warn" :disabled="tracking" @click="sendKey">发送按键</button>
          </div>
        </div>
      </section>

      <section class="panel">
        <div class="panel-head"><h2>诊断</h2></div>
        <div class="panel-body tools-body">
          <button class="btn stretch" :disabled="tracking" @click="runTool('/tools/ocr-backends')">OCR 后端探测</button>
          <div class="tools-grid-row">
            <input v-model.number="captureRounds" inputmode="numeric" placeholder="截图 API 轮数" aria-label="截图 API 轮数" />
            <button class="btn" :disabled="tracking" @click="runTool('/tools/capture-benchmark', { rounds: captureRounds })">截图性能</button>
          </div>
          <div class="tools-grid-row">
            <input v-model.number="changeSamples" inputmode="numeric" placeholder="变化采样次数" aria-label="变化采样次数" />
            <button class="btn" :disabled="tracking" @click="runTool('/tools/chat-change-samples', { samples: changeSamples })">聊天变化</button>
          </div>
          <div class="tools-grid-row">
            <input v-model.number="benchmarkRounds" inputmode="numeric" placeholder="面板基准轮数" aria-label="面板基准轮数" />
            <button class="btn" :disabled="tracking" @click="runTool('/tools/panel-benchmark', { rounds: benchmarkRounds })">面板基准</button>
          </div>
          <div class="tools-ai-row">
            <input v-model="aiKeyword" placeholder="AI 候选诊断关键词" />
            <label class="tools-check"><input v-model="aiAccompaniment" type="checkbox" />优先伴奏</label>
            <button class="btn" :disabled="tracking" @click="aiPreview">AI 诊断</button>
          </div>
        </div>
      </section>
    </div>

    <div class="tools-wide">
      <section class="panel">
        <div class="panel-head">
          <h2>任务结果</h2>
          <span class="tools-row">
            <small>{{ activeTask ? `#${activeTask}` : '未开始' }}</small>
            <button v-if="tracking" class="btn small" @click="stopTracking">停止跟踪</button>
          </span>
        </div>
        <div class="panel-body"><pre class="tools-output">{{ resultText }}</pre></div>
      </section>
      <section class="panel">
        <div class="panel-head">
          <h2>最近任务</h2>
          <button class="btn small" @click="loadRecentTasks">刷新</button>
        </div>
        <div class="panel-body">
          <div v-if="!recentTasks.length" class="config-field-hint">暂无任务</div>
          <button
            v-for="task in recentTasks"
            :key="task.id"
            type="button"
            class="tools-task"
            :class="{ active: task.id === activeTask }"
            @click="inspectTask(task.id)"
          >
            <span class="tools-task-state" :class="task.status">{{ statusLabel(task.status) }}</span>
            <span>#{{ task.id }} {{ task.label }}</span>
          </button>
        </div>
      </section>
    </div>

    <section class="panel tools-wide">
      <div class="panel-head"><h2>游戏状态只读观测</h2><small>不点击、不输入、不派发命令</small></div>
      <div class="panel-body tools-body">
        <div class="tools-row">
          <label class="tools-check"><input v-model="readOnly.enabled" type="checkbox" @change="setReadOnlyMode" />启用只读观测</label>
          <select v-model="readOnly.state" style="max-width:180px" @change="setReadOnlyMode">
            <option value="active">主动读取</option>
            <option value="push">推送</option>
          </select>
          <button class="btn" @click="loadReadOnlyMode">刷新状态</button>
        </div>
        <p class="config-field-hint">程序继续正常监听、点歌和播放；主动读取只在点击时观察一次；推送只记录正常运行中触发的命令或流程。</p>
        <div class="tools-row">
          <input v-model="readOnlyLabel" maxlength="120" placeholder="证据标签（可选）" />
          <button class="btn primary" :disabled="readOnlyBusy || !readOnly.enabled || readOnly.state !== 'active'" @click="readOnce">读取一次</button>
        </div>
        <p v-if="readOnlyStatus" class="config-field-hint">{{ readOnlyStatus }}</p>
        <div class="tools-row">
          <input v-model.number="evidenceLimit" inputmode="numeric" placeholder="条数" aria-label="证据条数" style="max-width:120px" />
          <button class="btn small" @click="loadEvidenceEntries">读取证据列表</button>
          <button class="btn small ghost" @click="clearEvidence">清空</button>
        </div>
        <div class="tools-evidence">
          <article v-for="(item, index) in evidenceEntries" :key="index" class="tools-evidence-card">
            <pre>{{ item }}</pre>
          </article>
        </div>
      </div>
    </section>

    <div v-if="screenshotUrl" class="tools-modal" @click.self="closeScreenshot">
      <div class="tools-modal-body">
        <div class="tools-row">
          <a class="btn small" :href="screenshotUrl" download="screenshot.jpg">下载</a>
          <button class="btn small" @click="closeScreenshot">关闭</button>
        </div>
        <img :src="screenshotUrl" alt="游戏截图" />
      </div>
    </div>
  </div>
</template>

<script setup>
import { onBeforeUnmount, onMounted, reactive, ref } from 'vue'
import { getJson, readToken, request, requestJson } from '../api.js'
import { handleAuth, toast } from '../store.js'

const tracking = ref(false)
const activeTask = ref(0)
const resultText = ref('等待任务')
const recentTasks = ref([])
const templates = ref([])

const ocrRect = ref('')
const templateName = ref('')
const templateRect = ref('')
const threshold = ref('')
const clickX = ref('')
const clickY = ref('')
const keyName = ref('')
const captureRounds = ref(3)
const changeSamples = ref(10)
const benchmarkRounds = ref(3)
const aiKeyword = ref('')
const aiAccompaniment = ref(false)

const screenshotBusy = ref(false)
const screenshotUrl = ref('')

const readOnly = reactive({ enabled: false, state: 'active' })
const readOnlyLoaded = ref(false)
const readOnlyBusy = ref(false)
const readOnlyStatus = ref('')
const readOnlyLabel = ref('')
const evidenceLimit = ref(20)
const evidenceEntries = ref([])

const STATUS_LABELS = { queued: '等待业务空闲', running: '执行中', completed: '已完成', failed: '失败' }
function statusLabel(status) {
  return STATUS_LABELS[status] || status || '未知'
}

function report(error, prefix = '操作失败') {
  if (handleAuth(error)) return
  resultText.value = `${prefix}：${error?.message || error}`
  toast(resultText.value, 'err')
}

function params(values) {
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(values || {})) {
    if (value !== '' && value != null) search.set(key, String(value))
  }
  const text = search.toString()
  return text ? `?${text}` : ''
}

async function runTool(path, values) {
  if (tracking.value) return
  tracking.value = true
  resultText.value = '任务已提交…'
  try {
    const task = await request(path + params(values), 'POST')
    const parsed = typeof task === 'string' ? JSON.parse(task) : task
    const id = Number(parsed && parsed.id)
    if (!Number.isSafeInteger(id) || id < 1) throw new Error('任务响应无效')
    activeTask.value = id
    await trackTask(id)
  } catch (error) {
    report(error, '提交失败')
  } finally {
    tracking.value = false
    loadRecentTasks()
  }
}

async function trackTask(id) {
  activeTask.value = id
  let failures = 0
  let delay = 500
  while (tracking.value) {
    let task
    try {
      task = await getJson(`/tools/task?id=${encodeURIComponent(id)}`)
      failures = 0
      delay = 500
    } catch (error) {
      failures += 1
      if (failures >= 5) {
        report(error, `连续 ${failures} 次读取任务失败，已停止跟踪`)
        return
      }
      delay = Math.min(5000, delay * 2)
      await sleep(delay)
      continue
    }
    const status = task && task.status
    resultText.value = task && task.result ? task.result : status === 'failed' ? '任务失败，未返回详情' : status === 'completed' ? '任务完成，无输出' : '等待执行…'
    if (status !== 'queued' && status !== 'running') return
    await sleep(delay)
  }
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

function stopTracking() {
  tracking.value = false
  resultText.value += '\n\n（已停止跟踪；后台任务不会被取消）'
}

function inspectTask(id) {
  if (tracking.value) return
  tracking.value = true
  trackTask(Number(id)).finally(() => {
    tracking.value = false
  })
}

function requireValue(value, message) {
  if (String(value || '').trim()) return true
  resultText.value = message
  return false
}

function useConfiguredTemplateRegion() {
  const item = templates.value.find((entry) => (entry.name || entry[0]) === templateName.value)
  const region = item && (item.region || item[2])
  if (!region || typeof region !== 'object') {
    resultText.value = '该模板没有配置区域'
    return
  }
  templateRect.value = [region.x, region.y, region.width, region.height].join(',')
}

function matchTemplate(click) {
  if (!requireValue(templateName.value, '请选择模板')) return
  runTool('/tools/template', {
    template: templateName.value,
    rect: templateRect.value,
    threshold: threshold.value,
    click: click ? '1' : '',
  })
}

function clickPoint() {
  if (!requireValue(clickX.value && clickY.value, '请输入 x 和 y 坐标')) return
  if (!window.confirm(`确认点击 (${clickX.value}, ${clickY.value})？`)) return
  runTool('/tools/click', { x: clickX.value, y: clickY.value })
}

function sendKey() {
  if (!requireValue(keyName.value, '请输入要发送的按键')) return
  if (!window.confirm(`确认发送按键「${keyName.value}」？`)) return
  runTool('/tools/key', { key: keyName.value })
}

function aiPreview() {
  if (!requireValue(aiKeyword.value, '请输入 AI 候选诊断关键词')) return
  runTool('/tools/ai-preview', {
    keyword: aiKeyword.value,
    preferAccompaniment: aiAccompaniment.value ? '1' : '',
  })
}

async function openScreenshot() {
  if (screenshotBusy.value) return
  screenshotBusy.value = true
  try {
    const token = readToken()
    const response = await fetch('/screenshot?quality=88', { headers: token ? { 'X-Miliastra-Token': token } : {} })
    if (!response.ok) {
      if (response.status === 401) throw new Error('访问令牌无效或已过期，请到设置页重新填写')
      throw new Error(`HTTP ${response.status}`)
    }
    if (screenshotUrl.value) URL.revokeObjectURL(screenshotUrl.value)
    screenshotUrl.value = URL.createObjectURL(await response.blob())
  } catch (error) {
    report(error, '截图失败')
  } finally {
    screenshotBusy.value = false
  }
}

function closeScreenshot() {
  if (screenshotUrl.value) URL.revokeObjectURL(screenshotUrl.value)
  screenshotUrl.value = ''
}

async function loadRecentTasks() {
  try {
    const monitor = await getJson('/monitor')
    recentTasks.value = Array.isArray(monitor?.webTools) ? monitor.webTools : []
  } catch (error) {
    report(error, '读取最近任务失败')
  }
}

async function loadTemplates() {
  try {
    const items = await getJson('/tools/templates')
    templates.value = Array.isArray(items) ? items : []
  } catch (error) {
    report(error, '读取模板失败')
  }
}

async function loadReadOnlyMode() {
  readOnlyBusy.value = true
  try {
    const payload = await getJson('/debug/read-only/mode')
    const mode = payload?.mode || payload
    if (mode && typeof mode.enabled === 'boolean') {
      readOnly.enabled = mode.enabled
      readOnly.state = mode.state || 'active'
    }
    readOnlyLoaded.value = true
    readOnlyStatus.value = readOnly.enabled ? '只读观测已启用' : '只读观测已禁用'
  } catch (error) {
    report(error, '读取只读状态失败')
  } finally {
    readOnlyBusy.value = false
  }
}

async function setReadOnlyMode() {
  if (!readOnlyLoaded.value) {
    await loadReadOnlyMode()
  }
  readOnlyBusy.value = true
  try {
    const payload = await requestJson('/debug/read-only/mode', { enabled: readOnly.enabled, state: readOnly.state })
    const mode = payload?.mode || payload
    if (mode && typeof mode.enabled === 'boolean') {
      readOnly.enabled = mode.enabled
      readOnly.state = mode.state || readOnly.state
    }
    readOnlyStatus.value = '模式已更新'
  } catch (error) {
    report(error, '切换只读状态失败')
  } finally {
    readOnlyBusy.value = false
  }
}

async function readOnce() {
  readOnlyBusy.value = true
  readOnlyStatus.value = '正在采集一次只读证据…'
  try {
    const value = await requestJson('/debug/read-only/read', {
      label: readOnlyLabel.value,
      ocr: true,
      rect: null,
    })
    const evidence = value?.evidence || value
    evidenceEntries.value = [JSON.stringify(evidence, null, 2), ...evidenceEntries.value].slice(0, 12)
    readOnlyStatus.value = '本次读取已完成；不会定时读取。'
  } catch (error) {
    report(error, '主动读取失败')
    readOnlyStatus.value = '本次读取失败'
  } finally {
    readOnlyBusy.value = false
  }
}

async function loadEvidenceEntries() {
  try {
    const value = await getJson(`/debug/read-only/entries?limit=${encodeURIComponent(evidenceLimit.value || 20)}`)
    const entries = value?.entries?.items || value?.items || []
    evidenceEntries.value = Array.isArray(entries) ? entries.map((item) => JSON.stringify(item, null, 2)) : [JSON.stringify(value, null, 2)]
  } catch (error) {
    report(error, '读取证据列表失败')
  }
}

function clearEvidence() {
  evidenceEntries.value = []
}

onMounted(() => {
  loadRecentTasks()
  loadTemplates()
  loadReadOnlyMode()
})
onBeforeUnmount(() => {
  tracking.value = false
  if (screenshotUrl.value) URL.revokeObjectURL(screenshotUrl.value)
})
</script>

<style scoped>
.tools-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(320px, 100%), 1fr)); gap: 12px; padding: 0 24px 12px; }
.tools-wide { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(420px, 100%), 1fr)); gap: 12px; padding: 0 24px 12px; }
.tools-body { display: flex; flex-direction: column; gap: 8px; }
.tools-two { display: grid; grid-template-columns: 1fr 1fr; gap: 8px; }
.tools-row { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.tools-row input { flex: 1 1 140px; min-width: 100px; }
.tools-check { display: inline-flex; align-items: center; gap: 4px; font-size: var(--fs-tiny); }
.tools-output { margin: 0; max-height: 320px; overflow: auto; white-space: pre-wrap; word-break: break-all; background: var(--bg-1); padding: 8px; border-radius: 8px; font-size: var(--fs-tiny); }
.tools-task { display: flex; gap: 8px; width: 100%; text-align: left; background: var(--bg-1); border: 1px solid var(--sep-1); border-radius: 8px; padding: 6px 8px; cursor: pointer; font-size: var(--fs-tiny); }
.tools-task.active { border-color: var(--accent); }
.tools-task-state { color: var(--text-2); }
.tools-task-state.failed { color: var(--danger); }
.tools-task-state.completed { color: var(--ok); }
.tools-evidence { display: flex; flex-direction: column; gap: 8px; max-height: 420px; overflow: auto; }
.tools-evidence-card pre { margin: 0; white-space: pre-wrap; word-break: break-all; background: var(--bg-1); padding: 8px; border-radius: 8px; font-size: var(--fs-tiny); }
.tools-modal { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.72); display: flex; align-items: center; justify-content: center; z-index: 120; }
.tools-modal-body { max-width: 92vw; max-height: 92vh; background: var(--bg-1); border-radius: 12px; padding: 12px; display: flex; flex-direction: column; gap: 8px; }
.tools-modal-body img { max-width: 86vw; max-height: 78vh; object-fit: contain; }
</style>
