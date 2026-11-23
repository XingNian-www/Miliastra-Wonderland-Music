<template>
  <div id="page-tools" class="tools-page">
    <header class="tools-head">
      <h1 class="page-title">高级控制</h1>
      <p class="page-subtitle">OCR、模板匹配、手动输入与只读观测；写操作会真实操作游戏</p>
    </header>

    <!-- 工具条：高频操作一排排开，统高等宽；危险操作全部下沉到折叠区末尾 -->
    <div class="tools-bar">
      <div class="tools-bar-actions">
        <button class="btn primary tools-bar-btn" :disabled="tracking" @click="runTool('/tools/ocr')">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 7V5a2 2 0 0 1 2-2h2M17 3h2a2 2 0 0 1 2 2v2M21 17v2a2 2 0 0 1-2 2h-2M7 21H5a2 2 0 0 1-2-2v-2"/><circle cx="12" cy="12" r="3.5"/></svg>
          <span>全屏 OCR</span>
        </button>
        <button class="btn tools-bar-btn" :disabled="tracking" @click="runTool('/tools/scan-chat')">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h12a2 2 0 0 1 2 2z"/></svg>
          <span>扫描聊天</span>
        </button>
        <button class="btn tools-bar-btn" :disabled="tracking" @click="runTool('/tools/ui-state')">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="4" width="18" height="15" rx="2"/><path d="M3 9h18M9 9v10"/></svg>
          <span>检测 UI</span>
        </button>
        <button class="btn tools-bar-btn" :disabled="tracking" @click="runTool('/tools/hall-name')">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="5" y="3" width="14" height="18" rx="3"/><path d="M9 8h6M9 12h6M9 16h3"/></svg>
          <span>大厅名称</span>
        </button>
        <button class="btn tools-bar-btn" :disabled="screenshotBusy" @click="openScreenshot">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M23 19a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4l2-3h6l2 3h4a2 2 0 0 1 2 2z"/><circle cx="12" cy="13" r="4"/></svg>
          <span>{{ screenshotBusy ? '截图中…' : '获取截图' }}</span>
        </button>
      </div>
      <div class="tools-bar-meta">
        <i class="tools-bar-dot" :class="{ busy: tracking }"></i>
        <span class="tools-bar-state">{{ tracking ? `跟踪 #${activeTask}` : '待命' }}</span>
        <span class="tools-bar-last">{{ lastRunText }}</span>
      </div>
    </div>

    <div class="tools-deck">
      <div class="tools-deck-main">
        <!-- 区域 OCR：中等频率，默认收起；摘要行保留一键执行 -->
        <details class="tools-fold" open>
          <summary class="tools-fold-sum">
            <svg class="tools-fold-chev" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 6l6 6-6 6"/></svg>
            <span class="tools-fold-title">区域 OCR</span>
            <span class="tools-fold-meta">{{ ocrRect ? `区域 ${ocrRect}` : '未填区域 · 默认整屏' }}</span>
            <span class="tools-fold-trail">
              <button class="btn small tools-fold-btn" type="button" :disabled="tracking" @click.stop.prevent="runTool('/tools/ocr', { rect: ocrRect })">区域 OCR</button>
            </span>
          </summary>
          <div class="tools-fold-body">
            <div class="tools-field">
              <label class="tools-field-label" for="tools-ocr-rect">识别区域</label>
              <div class="tools-grid-row">
                <input id="tools-ocr-rect" v-model="ocrRect" placeholder="x,y,width,height（留空 = 整屏）" />
                <button class="btn" :disabled="tracking" @click="runTool('/tools/ocr', { rect: ocrRect })">运行</button>
              </div>
            </div>
          </div>
        </details>

        <!-- 手动输入：高频，默认展开 -->
        <details class="tools-fold" open>
          <summary class="tools-fold-sum">
            <svg class="tools-fold-chev" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 6l6 6-6 6"/></svg>
            <span class="tools-fold-title">手动输入</span>
            <span class="tools-fold-meta">{{ keyName ? `按键 ${keyName}` : (clickX || clickY) ? `坐标 ${clickX || '?'} , ${clickY || '?'}` : '坐标与按键 · 会向游戏发送真实输入' }}</span>
            <span class="tools-fold-trail">
              <small class="tools-fold-note">会发送真实输入</small>
            </span>
          </summary>
          <div class="tools-fold-body">
            <div class="tools-field">
              <label class="tools-field-label" for="tools-click-x">点击坐标</label>
              <div class="tools-xyz">
                <input id="tools-click-x" v-model="clickX" placeholder="x" inputmode="numeric" />
                <input v-model="clickY" placeholder="y" inputmode="numeric" aria-label="y 坐标" />
                <button class="btn warn" :disabled="tracking" @click="clickPoint">点击</button>
              </div>
            </div>
            <div class="tools-field">
              <label class="tools-field-label" for="tools-key-name">按键名</label>
              <div class="tools-grid-row">
                <input id="tools-key-name" v-model="keyName" placeholder="Escape / Return / F2" />
                <button class="btn warn" :disabled="tracking" @click="sendKey">发送按键</button>
              </div>
            </div>
          </div>
        </details>

        <!-- 模板匹配：低频，默认收起；摘要行保留「匹配」 -->
        <details class="tools-fold">
          <summary class="tools-fold-sum">
            <svg class="tools-fold-chev" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 6l6 6-6 6"/></svg>
            <span class="tools-fold-title">模板匹配</span>
            <span class="tools-fold-meta">{{ templateName || '未选择模板' }}</span>
            <span class="tools-fold-trail">
              <button class="btn small tools-fold-btn" type="button" :disabled="tracking" @click.stop.prevent="matchTemplate(false)">匹配</button>
            </span>
          </summary>
          <div class="tools-fold-body">
            <div class="tools-field">
              <label class="tools-field-label" for="tools-template-name">模板</label>
              <select id="tools-template-name" v-model="templateName">
                <option value="">（选择模板）</option>
                <option v-for="item in templates" :key="item.name || item[0]" :value="item.name || item[0]">{{ item.label || item[1] || item.name || item[0] }}</option>
              </select>
            </div>
            <div class="tools-field">
              <label class="tools-field-label" for="tools-template-rect">匹配区域</label>
              <div class="tools-grid-row">
                <input id="tools-template-rect" v-model="templateRect" placeholder="x,y,width,height（留空 = 全屏）" />
                <button class="btn small ghost" type="button" @click="useConfiguredTemplateRegion">读取配置区域</button>
              </div>
            </div>
            <div class="tools-field">
              <label class="tools-field-label" for="tools-threshold">阈值</label>
              <input id="tools-threshold" v-model="threshold" placeholder="留空使用配置" inputmode="decimal" />
            </div>
            <div class="tools-fold-foot">
              <button class="btn" :disabled="tracking" @click="matchTemplate(false)">匹配</button>
              <button class="btn warn" :disabled="tracking" @click="matchTemplate(true)">匹配并点击</button>
            </div>
          </div>
        </details>

        <!-- 诊断：低频，默认收起 -->
        <details class="tools-fold">
          <summary class="tools-fold-sum">
            <svg class="tools-fold-chev" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 6l6 6-6 6"/></svg>
            <span class="tools-fold-title">诊断</span>
            <span class="tools-fold-meta">OCR 后端 · 截图性能 · 聊天变化 · 面板基准 · AI 诊断</span>
          </summary>
          <div class="tools-fold-body">
            <button class="btn stretch" :disabled="tracking" @click="runTool('/tools/ocr-backends')">OCR 后端探测</button>
            <div class="tools-field">
              <label class="tools-field-label" for="tools-capture-rounds">截图 API 轮数</label>
              <div class="tools-grid-row">
                <input id="tools-capture-rounds" v-model.number="captureRounds" inputmode="numeric" placeholder="例如 3" />
                <button class="btn" :disabled="tracking" @click="runTool('/tools/capture-benchmark', { rounds: captureRounds })">截图性能</button>
              </div>
            </div>
            <div class="tools-field">
              <label class="tools-field-label" for="tools-change-samples">变化采样次数</label>
              <div class="tools-grid-row">
                <input id="tools-change-samples" v-model.number="changeSamples" inputmode="numeric" placeholder="例如 10" />
                <button class="btn" :disabled="tracking" @click="runTool('/tools/chat-change-samples', { samples: changeSamples })">聊天变化</button>
              </div>
            </div>
            <div class="tools-field">
              <label class="tools-field-label" for="tools-benchmark-rounds">面板基准轮数</label>
              <div class="tools-grid-row">
                <input id="tools-benchmark-rounds" v-model.number="benchmarkRounds" inputmode="numeric" placeholder="例如 3" />
                <button class="btn" :disabled="tracking" @click="runTool('/tools/panel-benchmark', { rounds: benchmarkRounds })">面板基准</button>
              </div>
            </div>
            <div class="tools-field">
              <label class="tools-field-label" for="tools-ai-keyword">AI 候选诊断关键词</label>
              <div class="tools-ai-row">
                <input id="tools-ai-keyword" v-model="aiKeyword" placeholder="关键词" />
                <label class="tools-check"><input v-model="aiAccompaniment" type="checkbox" />优先伴奏</label>
                <button class="btn" :disabled="tracking" @click="aiPreview">AI 诊断</button>
              </div>
            </div>
          </div>
        </details>
      </div>

      <aside class="tools-rail">
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
      </aside>
    </div>

    <!-- 只读观测：最不常用，默认收起 -->
    <details class="tools-fold tools-fold-wide">
      <summary class="tools-fold-sum">
        <svg class="tools-fold-chev" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 6l6 6-6 6"/></svg>
        <span class="tools-fold-title">游戏状态只读观测</span>
        <span class="tools-fold-meta">{{ readOnly.enabled ? `已启用 · ${readOnly.state === 'push' ? '推送' : '主动读取'}` : '未启用' }} · 不点击、不输入、不派发命令</span>
        <span class="tools-fold-trail">
          <button class="btn small tools-fold-btn" type="button" @click.stop.prevent="loadReadOnlyMode">刷新状态</button>
        </span>
      </summary>
      <div class="tools-fold-body">
        <div class="tools-row">
          <label class="tools-check"><input v-model="readOnly.enabled" type="checkbox" @change="setReadOnlyMode" />启用只读观测</label>
          <select v-model="readOnly.state" style="max-width:180px" @change="setReadOnlyMode">
            <option value="active">主动读取</option>
            <option value="push">推送</option>
          </select>
          <button class="btn" @click="loadReadOnlyMode">刷新状态</button>
        </div>
        <p class="config-field-hint">程序继续正常监听、点歌和播放；主动读取只在点击时观察一次；推送只记录正常运行中触发的命令或流程。</p>
        <div class="tools-field">
          <label class="tools-field-label" for="tools-evidence-label">证据标签（可选）</label>
          <div class="tools-row">
            <input id="tools-evidence-label" v-model="readOnlyLabel" maxlength="120" placeholder="留空则不带标签" />
            <button class="btn primary" :disabled="readOnlyBusy || !readOnly.enabled || readOnly.state !== 'active'" @click="readOnce">读取一次</button>
          </div>
        </div>
        <p v-if="readOnlyStatus" class="config-field-hint">{{ readOnlyStatus }}</p>
        <div class="tools-field">
          <label class="tools-field-label" for="tools-evidence-limit">证据条数</label>
          <div class="tools-row">
            <input id="tools-evidence-limit" v-model.number="evidenceLimit" inputmode="numeric" placeholder="20" style="max-width:120px" />
            <button class="btn small" @click="loadEvidenceEntries">读取证据列表</button>
            <button class="btn small ghost" @click="clearEvidence">清空</button>
          </div>
        </div>
        <div class="tools-evidence">
          <article v-for="(item, index) in evidenceEntries" :key="index" class="tools-evidence-card">
            <pre>{{ item }}</pre>
          </article>
        </div>
      </div>
    </details>

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
import { fetchBlob, getJson, request, requestJson } from '../api.js'
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
// 工具条副标题：最近一次操作 + 耗时。纯展示，不影响任何执行路径。
const lastRunText = ref('尚无执行记录')

const TOOL_LABELS = {
  '/tools/scan-chat': '扫描聊天',
  '/tools/ui-state': '检测 UI',
  '/tools/hall-name': '大厅名称',
  '/tools/ocr-backends': 'OCR 后端探测',
  '/tools/capture-benchmark': '截图性能',
  '/tools/chat-change-samples': '聊天变化',
  '/tools/panel-benchmark': '面板基准',
  '/tools/ai-preview': 'AI 诊断',
  '/tools/template': '模板匹配',
  '/tools/click': '点击',
  '/tools/key': '发送按键',
}

function toolLabel(path, values) {
  if (path === '/tools/ocr') return values && values.rect ? '区域 OCR' : '全屏 OCR'
  return TOOL_LABELS[path] || path
}

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
  const startedAt = Date.now()
  const label = toolLabel(path, values)
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
    lastRunText.value = `上次 ${label} · ${((Date.now() - startedAt) / 1000).toFixed(1)}s`
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
    const blob = await fetchBlob('/screenshot?quality=88')
    if (screenshotUrl.value) URL.revokeObjectURL(screenshotUrl.value)
    screenshotUrl.value = URL.createObjectURL(blob)
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
/* 本页特有的结构样式；通用皮肤（按钮/输入）在 components.css 的
   「高级控制页 · 方案 C」段末尾，二者都不改既有全局规则。 */
.tools-row { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.tools-row input:not([type="checkbox"]):not([type="radio"]) { flex: 1 1 140px; min-width: 100px; }
.tools-row input[type="checkbox"] { flex: 0 0 auto; width: 36px; min-width: 36px; }
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
