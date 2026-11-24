<template>
  <div id="page-tasks">
    <h1 class="page-title">任务与日志</h1>
    <p class="page-subtitle">任务队列、事件日志与接口调用记录</p>

    <div class="config-status">
      <span class="chip" :data-state="statusState">
        <i class="dot"></i><span>服务</span><em>{{ monitor?.status || '未知' }}</em>
      </span>
      <span class="chip"><span>任务</span><em>{{ tasks.length }}</em></span>
      <span class="chip"><span>活动</span><em>{{ activeCount }}</em></span>
      <span class="chip"><span>接口记录</span><em>{{ history.length }}</em></span>
      <span class="config-field-hint">{{ refreshHint }}</span>
      <span style="flex: 1"></span>
      <button
        class="btn small warn"
        type="button"
        :disabled="loading || cancelingAll || !queuedTasks.length"
        :title="queuedTasks.length ? `逐个 POST /tasks/cancel?id= 撤销 ${queuedTasks.length} 个等待任务` : '没有等待中的任务'"
        @click="cancelQueuedTasks"
      >
        {{ cancelingAll ? '撤销中…' : `撤销全部排队（${queuedTasks.length}）` }}
      </button>
      <button
        class="btn small warn"
        type="button"
        :disabled="!history.length"
        title="POST /clear-history：清空接口调用记录；事件日志与命令记录来自 /monitor 内存环形缓冲，不受影响"
        @click="clearHistory"
      >
        清空接口记录
      </button>
      <button class="btn small" type="button" :disabled="loading" @click="refreshAll">
        {{ loading ? '刷新中…' : '刷新' }}
      </button>
      <label class="switch-line">
        <input v-model="autoRefresh" type="checkbox" /> 自动刷新
      </label>
    </div>

    <div v-if="error" class="config-message err" role="status">{{ error }}</div>

    <div v-if="loading && !monitor" class="loading-card">正在读取任务与日志…</div>
    <div v-else class="page-grid">
      <div class="panel task-queue-panel">
        <div class="panel-head">
          <h2>任务队列</h2>
          <small>{{ tasks.length }} 条</small>
        </div>
        <div class="panel-body">
          <div class="tasks-mac-list" role="list">
            <div
              v-for="task in tasks"
              :key="task.id"
              class="task-item"
              :class="{ selected: selectedId === task.id }"
              role="button"
              tabindex="0"
              @click="selectedId = task.id"
              @keydown.enter.prevent="selectedId = task.id"
              @keydown.space.prevent="selectedId = task.id"
            >
              <span class="task-status" :class="task.status">{{ statusLabel(task.status) }}</span>
              <div class="queue-title">#{{ task.id }} {{ task.label || '未命名任务' }}</div>
              <div class="queue-meta">{{ elapsedText(task) }}</div>
              <div v-if="task.result" class="task-result" :title="task.result">{{ task.result }}</div>
              <button
                v-if="task.status === 'queued' || task.status === 'running'"
                type="button"
                class="btn small ghost"
                :disabled="cancelingId === task.id"
                @click.stop="cancelTask(task)"
              >
                {{ cancelingId === task.id ? '撤销中…' : '撤销' }}
              </button>
            </div>
            <div v-if="!tasks.length" class="tasks-mac-empty">暂无任务</div>
          </div>
        </div>
      </div>

      <div v-if="!selectedTask" class="panel task-detail-panel">
        <div class="panel-head"><h2>任务详情</h2></div>
        <div class="panel-body">
          <p class="tasks-mac-empty">选择左侧任务查看详情</p>
        </div>
      </div>
      <div v-else class="panel task-detail-panel">
        <div class="panel-head">
          <h2>任务详情</h2>
          <small>#{{ selectedTask.id }}</small>
        </div>
        <div class="panel-body">
          <div class="tasks-mac-detail-title">
            <span class="task-status" :class="selectedTask.status">{{ statusLabel(selectedTask.status) }}</span>
            <b>{{ selectedTask.label || '未命名任务' }}</b>
            <span style="flex: 1"></span>
            <button
              v-if="selectedTask.status === 'queued' || selectedTask.status === 'running'"
              type="button"
              class="btn small warn"
              :disabled="cancelingId === selectedTask.id"
              title="POST /tasks/cancel?id=：等待中的任务会直接撤销，执行中的任务会请求停止"
              @click="cancelTask(selectedTask)"
            >
              {{ cancelingId === selectedTask.id ? '撤销中…' : '撤销任务' }}
            </button>
          </div>
          <dl class="tasks-mac-facts">
            <div class="tasks-mac-fact">
              <dt>入队</dt>
              <dd>{{ formatTime(selectedTask.queuedAtMs) }}</dd>
            </div>
            <div class="tasks-mac-fact">
              <dt>开始</dt>
              <dd>{{ formatTime(selectedTask.startedAtMs) }}</dd>
            </div>
            <div class="tasks-mac-fact">
              <dt>结束</dt>
              <dd>{{ formatTime(selectedTask.finishedAtMs) }}</dd>
            </div>
            <div class="tasks-mac-fact">
              <dt>耗时</dt>
              <dd>{{ elapsedText(selectedTask) }}</dd>
            </div>
          </dl>
          <p class="tasks-mac-result">{{ selectedTask.result || '暂无结果' }}</p>
        </div>
      </div>

      <div class="panel wide">
        <div class="panel-head">
          <div class="tabs" role="tablist" aria-label="任务日志">
            <button
              v-for="item in tabs"
              :key="item.key"
              class="tab"
              :class="{ active: tab === item.key }"
              type="button"
              role="tab"
              :aria-selected="tab === item.key"
              @click="tab = item.key"
            >
              {{ item.label }}
            </button>
          </div>
          <div style="display: flex; align-items: center; gap: 8px">
            <button class="btn small ghost" type="button" :disabled="!currentExport.length" @click="exportCurrent">
              导出 JSON
            </button>
          </div>
        </div>
        <div class="panel-body">
          <pre v-if="tab === 'logs'" class="scroll tasks-mac-log">{{ logsText }}</pre>
          <pre v-else-if="tab === 'commands'" class="scroll tasks-mac-log">{{ commandsText }}</pre>
          <div v-else class="tasks-mac-history">
            <div v-for="(item, index) in history" :key="`${item.time}-${index}`" class="history-item" :class="{ err: !item.ok }">
              <div>
                <span class="history-time">{{ item.time }}</span>
                <b>{{ item.command }}</b>
                <span class="config-field-hint"> {{ queryText(item.query) }}</span>
              </div>
              <div class="history-result">{{ item.result }}</div>
            </div>
            <p v-if="!history.length" class="tasks-mac-empty">暂无接口记录</p>
          </div>
          <p class="config-field-hint">
            事件/命令日志来自 GET /monitor 的内存环形缓冲，不落盘、无法清空；「清空接口记录」调用 POST /clear-history。
          </p>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup>
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { getJson, request, requestJson } from '../api.js'
import { handleAuth, loadStatus, state, toast } from '../store.js'

const STATUS_LABELS = {
  queued: '等待',
  running: '执行中',
  completed: '完成',
  failed: '失败',
  canceled: '已撤销',
}

const tabs = [
  { key: 'logs', label: '事件日志' },
  { key: 'commands', label: '命令记录' },
  { key: 'history', label: '接口记录' },
]

const monitor = ref(null)
const history = ref([])
const loading = ref(false)
const error = ref('')
const autoRefresh = ref(true)
const selectedId = ref(null)
const cancelingId = ref(null)
const cancelingAll = ref(false)
const tab = ref('logs')
const lastRefresh = ref('')

let timer = null

const tasks = computed(() => {
  const list = Array.isArray(monitor.value?.tasks) ? monitor.value.tasks : []
  const active = list.filter((task) => task.status === 'queued' || task.status === 'running')
  const finished = list
    .filter((task) => task.status !== 'queued' && task.status !== 'running')
    .slice()
    .sort((left, right) => Number(right.id) - Number(left.id))
  return [...active, ...finished]
})

const activeCount = computed(() => tasks.value.filter((task) => task.status === 'queued' || task.status === 'running').length)
const queuedTasks = computed(() => tasks.value.filter((task) => task.status === 'queued'))
const selectedTask = computed(() => tasks.value.find((task) => task.id === selectedId.value) || null)

watch(
  tasks,
  (list) => {
    if (list.some((task) => task.id === selectedId.value)) return
    selectedId.value = list.length ? list[0].id : null
  },
  { immediate: true },
)
const logs = computed(() => (Array.isArray(monitor.value?.logs) ? monitor.value.logs : []))
const commands = computed(() => (Array.isArray(monitor.value?.commands) ? monitor.value.commands : []))
const logsText = computed(() => (logs.value.length ? logs.value.join('\n') : '暂无日志'))
const commandsText = computed(() => (commands.value.length ? commands.value.join('\n') : '暂无命令记录'))
const statusState = computed(() => {
  if (!monitor.value) return error.value ? 'err' : ''
  const status = String(monitor.value.status || '')
  if (/退出|关停|失败|错误/.test(status)) return 'err'
  if (status === '运行中') return 'ok'
  return ''
})
const refreshHint = computed(() => {
  if (loading.value) return '刷新中…'
  if (!lastRefresh.value) return '等待刷新'
  return `${autoRefresh.value ? '自动刷新' : '已暂停'} · 最近 ${lastRefresh.value}`
})
const currentExport = computed(() => {
  if (tab.value === 'logs') return logs.value
  if (tab.value === 'commands') return commands.value
  return history.value
})

function statusLabel(status) {
  return STATUS_LABELS[status] || status || '未知'
}

function formatTime(ms) {
  if (ms == null || ms === '') return '—'
  const value = Number(ms)
  if (!Number.isFinite(value) || value <= 0) return '—'
  return new Date(value).toLocaleString('zh-CN', { hour12: false })
}

function elapsedText(task) {
  if (!task) return '—'
  let elapsed = Number(task.elapsedMs)
  if ((!Number.isFinite(elapsed) || elapsed < 0) && task.startedAtMs) {
    elapsed = Date.now() - Number(task.startedAtMs)
  }
  if (!Number.isFinite(elapsed) || elapsed < 0) return task.startedAtMs ? '进行中' : '尚未开始'
  if (elapsed < 1000) return `${Math.round(elapsed)}ms`
  if (elapsed < 60_000) return `${(elapsed / 1000).toFixed(1)}s`
  return `${Math.floor(elapsed / 60_000)}m${Math.round((elapsed % 60_000) / 1000)}s`
}

function queryText(query) {
  if (!query || typeof query !== 'object') return ''
  const rows = Object.entries(query).filter(([, value]) => value != null && String(value) !== '')
  return rows.length ? rows.map(([key, value]) => `${key}=${value}`).join(' ') : ''
}

async function loadMonitor() {
  try {
    await loadStatus()
    const data = state.monitor
    if (!data || typeof data !== 'object' || Array.isArray(data)) throw new Error('/monitor 响应格式不正确')
    monitor.value = data
    return true
  } catch (authOrError) {
    if (handleAuth(authOrError)) return false
    error.value = `读取任务/日志失败：${authOrError.message || authOrError}`
    return false
  }
}

async function loadHistory() {
  try {
    const data = await getJson('/history')
    if (!Array.isArray(data)) throw new Error('/history 响应不是数组')
    history.value = data
    return true
  } catch (authOrError) {
    if (handleAuth(authOrError)) return false
    error.value = `读取接口记录失败：${authOrError.message || authOrError}`
    return false
  }
}

async function refreshAll() {
  if (loading.value) return
  loading.value = true
  error.value = ''
  const [monitorOk, historyOk] = await Promise.all([loadMonitor(), loadHistory()])
  if (monitorOk || historyOk) {
    lastRefresh.value = new Date().toLocaleTimeString('zh-CN', { hour12: false })
  }
  loading.value = false
}

async function cancelTask(task) {
  if (!task || cancelingId.value) return
  cancelingId.value = task.id
  try {
    const data = await requestJson(`/tasks/cancel?id=${encodeURIComponent(task.id)}`, {})
    if (data && data.cancellationRequested === true && data.canceled === false) {
      toast(`已请求撤销任务 #${task.id}（执行中，收到请求后停止）`, 'ok')
    } else {
      toast(`已撤销任务 #${task.id}`, 'ok')
    }
    await loadMonitor()
  } catch (authOrError) {
    if (!handleAuth(authOrError)) toast(`撤销任务失败：${authOrError.message || authOrError}`, 'err')
  } finally {
    cancelingId.value = null
  }
}

// 批量撤销只做「等待中」任务：执行中的任务一次只允许一个撤销请求，
// 避免把正在跑的游戏操作中途打断成不可恢复状态（单任务仍可逐个请求停止）。
async function cancelQueuedTasks() {
  if (cancelingAll.value) return
  const list = queuedTasks.value.slice()
  if (!list.length) return
  if (!window.confirm(`确定撤销队列中 ${list.length} 个等待任务？执行中的任务不受影响。`)) return
  cancelingAll.value = true
  let canceled = 0
  const failures = []
  try {
    for (const task of list) {
      try {
        await requestJson(`/tasks/cancel?id=${encodeURIComponent(task.id)}`, {})
        canceled += 1
      } catch (authOrError) {
        if (handleAuth(authOrError)) break
        failures.push(`#${task.id}：${authOrError.message || authOrError}`)
      }
    }
  } finally {
    cancelingAll.value = false
  }
  await loadMonitor()
  if (failures.length) {
    toast(
      `已撤销 ${canceled} 个等待任务，${failures.length} 个失败（${failures[0]}${failures.length > 1 ? '…' : ''}）`,
      'err',
    )
  } else {
    toast(`已撤销 ${canceled} 个等待任务`, 'ok')
  }
}

async function clearHistory() {
  if (!history.value.length) return
  if (!window.confirm('确定清空接口调用记录？事件日志与命令记录不受影响。')) return
  try {
    await request('/clear-history', 'POST')
    history.value = []
    toast('接口调用记录已清空', 'ok')
  } catch (authOrError) {
    if (!handleAuth(authOrError)) toast(`清空失败：${authOrError.message || authOrError}`, 'err')
  }
}

function downloadText(filename, text) {
  const blob = new Blob([text], { type: 'application/json;charset=utf-8' })
  const url = URL.createObjectURL(blob)
  const anchor = document.createElement('a')
  anchor.href = url
  anchor.download = filename
  document.body.appendChild(anchor)
  anchor.click()
  anchor.remove()
  setTimeout(() => URL.revokeObjectURL(url), 5000)
}

function exportCurrent() {
  if (!currentExport.value.length) return
  const stamp = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19)
  const name = tab.value === 'logs' ? '事件日志' : tab.value === 'commands' ? '命令记录' : '接口记录'
  downloadText(`miliastra-${name}-${stamp}.json`, JSON.stringify(currentExport.value, null, 2))
  toast(`已导出 ${currentExport.value.length} 条${name}`, 'ok')
}

function tick() {
  if (!autoRefresh.value || document.hidden || loading.value) return
  refreshAll()
}

watch(tab, (value) => {
  if (value === 'history' && !history.value.length && lastRefresh.value) loadHistory()
})

onMounted(() => {
  refreshAll()
  timer = setInterval(tick, 3000)
})

onBeforeUnmount(() => {
  clearInterval(timer)
  timer = null
})
</script>

<style scoped>
/* =============================================================================
 * 任务与日志：macOS 数据密集面板
 *
 * 只作用于本页（`<style scoped>`）与页面专属前缀 `.tasks-mac-*`，不改
 * components.css 里共享的 `.panel` / `.btn` / `.tab` / `.task-item` 规则。
 * 队列是 grouped 列表：容器一圈圆角，行与行之间只有 1px 淡分隔线，
 * 选中行是内嵌的蓝色圆角胶囊（radius 6px + 白字），不是整行铺满。
 * ========================================================================== */

/* ---------- 队列：grouped 列表 ---------- */

.tasks-mac-list {
  border: 1px solid var(--sep-2);
  border-radius: var(--radius-m);
  background: var(--bg-input);
  overflow: auto;
  max-height: 480px;
  padding: 2px;
}

.tasks-mac-list .task-item {
  grid-template-columns: 52px minmax(0, 1fr) auto auto;
  min-height: 30px;
  padding: 5px var(--sp-2);
  margin: 0 2px;
  background: transparent;
  border: 0;
  border-radius: var(--radius-s);
}

.tasks-mac-list .task-item + .task-item {
  border-top: 1px solid var(--sep-2);
  border-top-left-radius: 0;
  border-top-right-radius: 0;
}

.tasks-mac-list .task-item:hover {
  background: var(--fill-1);
}

.tasks-mac-list .task-item .queue-title {
  font-size: var(--fs-item);
  line-height: var(--lh-item);
  color: var(--text-1);
}

.tasks-mac-list .task-item .queue-meta {
  color: var(--text-2);
  font-variant-numeric: tabular-nums;
}

.tasks-mac-list .task-item .task-status {
  font-size: var(--fs-tiny);
  letter-spacing: 0.02em;
}

.tasks-mac-list .task-item .task-result {
  color: var(--text-3);
}

/* 选中：内嵌圆角胶囊 + **半透明蓝底**。
 *
 * 不用 `--accent` 实底铺满：那是 iOS 列表选中（UITableViewCell 高亮）的观感，
 * 整块高饱和蓝在深色面板里像一段被选中的文本，很扎眼。macOS 桌面侧栏用
 * 半透明着色（Finder/Mail 都是），文字保持原色只提亮，视觉重量轻得多。
 */
.tasks-mac-list .task-item.selected {
  background: var(--accent-soft);
  border-color: var(--select-border);
  color: var(--text-1);
}

.tasks-mac-list .task-item.selected .queue-title {
  color: var(--text-1);
  font-weight: 600;
}

.tasks-mac-list .task-item.selected .queue-meta,
.tasks-mac-list .task-item.selected .task-status,
.tasks-mac-list .task-item.selected .task-result {
  color: var(--text-2);
  opacity: 1;
}

/* 选中行里的操作按钮保持中性，不跟着底色变白（否则对比度反转太跳）。 */
.tasks-mac-list .task-item.selected .btn.ghost {
  color: var(--text-2);
  border-color: var(--sep-1);
  background: transparent;
  opacity: 1;
}

/* ---------- 任务详情：标签小、数值大 ---------- */

.tasks-mac-detail-title {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: var(--sp-2);
  min-height: 30px;
}

.tasks-mac-detail-title b {
  font-size: var(--fs-item);
  font-weight: 600;
  color: var(--text-1);
}

.tasks-mac-facts {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: var(--sp-2) var(--sp-3);
  margin: 0;
  padding: var(--sp-3);
  border: 1px solid var(--sep-2);
  border-radius: var(--radius-m);
  background: var(--bg-input);
}

.tasks-mac-fact dt {
  color: var(--text-3);
  font-size: var(--fs-tiny);
  line-height: var(--lh-tiny);
  letter-spacing: 0.04em;
}

.tasks-mac-fact dd {
  margin: 2px 0 0;
  color: var(--text-1);
  font-size: var(--fs-meta);
  line-height: var(--lh-meta);
  font-variant-numeric: tabular-nums;
  word-break: break-word;
}

.tasks-mac-result {
  margin: 0;
  padding: 12px;
  max-height: 200px;
  overflow: auto;
  background: var(--bg-0);
  border: 1px solid var(--sep-2);
  border-radius: var(--radius-s);
  color: var(--text-2);
  font-family: var(--font-mono);
  font-size: var(--fs-meta);
  line-height: 1.65;
  white-space: pre-wrap;
  word-break: break-word;
}

/* ---------- 日志块：深色圆角面板 + 等宽 + 12px 内边距 ---------- */

.tasks-mac-empty {
  margin: 0;
  padding: var(--sp-4) var(--sp-3);
  color: var(--text-3);
  font-size: var(--fs-meta);
  line-height: var(--lh-meta);
  text-align: center;
}

.scroll.tasks-mac-log {
  background: var(--bg-0);
  border: 1px solid var(--sep-2);
  border-radius: var(--radius-m);
  padding: 12px;
  min-height: 132px;
  max-height: 320px;
  color: var(--text-2);
  font-family: var(--font-mono);
  font-size: var(--fs-meta);
  line-height: 1.65;
  tab-size: 2;
}

/* ---------- 接口记录：无左侧色条，只有极淡分隔 + hover 填充 ---------- */

.tasks-mac-history {
  display: flex;
  flex-direction: column;
  max-height: 260px;
  overflow: auto;
  border: 1px solid var(--sep-2);
  border-radius: var(--radius-m);
  background: var(--bg-input);
}

.tasks-mac-history .history-item {
  border: 0;
  border-left: 0;
  border-radius: 0;
  background: transparent;
  padding: 6px var(--sp-3);
  transition: background 0.12s ease;
}

.tasks-mac-history .history-item + .history-item {
  border-top: 1px solid var(--sep-2);
}

.tasks-mac-history .history-item:hover {
  background: var(--fill-1);
}

.tasks-mac-history .history-time {
  font-variant-numeric: tabular-nums;
  color: var(--text-3);
}

.tasks-mac-history .history-item.err .history-time,
.tasks-mac-history .history-item.err b {
  color: var(--danger);
}

/* 两个面板在宽屏并排时底边对齐，避免一高一低留台阶。 */
.page-grid > .task-detail-panel {
  height: 100% !important;
}

@media (max-width: 720px) {
  .tasks-mac-list {
    max-height: none;
  }

  .tasks-mac-facts {
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }
}
</style>
