<template>
  <div class="game-page">
    <h1 class="page-title">游戏与聊天</h1>
    <p class="page-subtitle">值守状态、最近聊天与手动操作；命令可直接以 @ 形式发送到游戏聊天</p>

    <div class="config-status">
      <span class="chip"><i class="dot"></i><span>服务</span><em>{{ serviceLabel }}</em></span>
      <span class="chip"><i class="dot"></i><span>界面</span><em>{{ uiStateLabel }}</em></span>
      <span class="chip"><i class="dot"></i><span>值守</span><em>{{ listenerLabel }}</em></span>
      <span class="chip"><i class="dot"></i><span>大厅</span><em>{{ hallRemainingLabel }}</em></span>
      <span class="chip"><i class="dot"></i><span>命令</span><em>{{ commandsEnabled ? '已启用' : '已禁用' }}</em></span>
      <span class="chip"><i class="dot"></i><span>闲置退出</span><em>{{ idleExitLabel }}</em></span>
      <button class="btn small" style="margin-left:auto" :disabled="loading" @click="refresh(true)">
        {{ loading ? '刷新中…' : '刷新' }}
      </button>
    </div>

    <div v-if="refreshError" class="config-message err" style="margin:12px 0">{{ refreshError }}</div>
    <div v-if="!monitor && !loading" class="loading-card">
      暂时读不到 /monitor；可重试刷新。游戏与聊天页仍可手动发送命令。
    </div>

    <div class="page-grid">
      <div class="panel">
        <div class="panel-head">
          <h2>聊天值守</h2>
          <small>{{ listener.pendingMode ? `等待切换到 ${displayMode(listener.pendingMode)}` : '即时生效' }}</small>
        </div>
        <div class="panel-body">
          <div class="tabs" role="tablist" aria-label="聊天值守模式">
            <button class="tab" :class="{ active: modeIsPrimary }" type="button" :disabled="busy" @click="setListener('primary')">一级</button>
            <button class="tab" :class="{ active: modeIsSecondary }" type="button" :disabled="busy" @click="setListener('secondary')">二级</button>
          </div>
          <div class="result" style="margin-top:12px;min-height:0">
            <div v-for="(line, index) in listenerDetailLines" :key="index">{{ line }}</div>
          </div>
        </div>
      </div>

      <div class="panel">
        <div class="panel-head"><h2>手动发送</h2><small>进入任务队列</small></div>
        <div class="panel-body">
          <input
            v-model="chatText"
            class="config-input"
            placeholder="发送到游戏聊天，如 @点歌晴天"
            aria-label="发送到游戏聊天"
            @keyup.enter="sendChat"
          />
          <div class="row" style="margin-top:8px;align-items:center">
            <label class="switch-line" style="display:flex;gap:6px;align-items:center">
              <input v-model="usePrefix" type="checkbox" /> 前缀
            </label>
            <input v-model="prefix" class="config-input" :disabled="!usePrefix" placeholder="[控制台]: " aria-label="发言前缀" />
          </div>
          <div class="controls" style="margin-top:8px">
            <button class="btn primary" :disabled="busy || !chatText.trim()" @click="sendChat">发送</button>
            <button class="btn ghost" @click="chatText = ''">清空</button>
          </div>
          <div class="row" style="margin-top:8px;flex-wrap:wrap">
            <button v-for="item in quickCommands" :key="item" class="btn small ghost" @click="chatText = item">{{ item }}</button>
          </div>
          <p class="config-field-hint">命令不依赖空格；发送后由值守任务执行，这里只负责入队。</p>
        </div>
      </div>

      <div class="panel">
        <div class="panel-head"><h2>游戏流程</h2><small>排队执行</small></div>
        <div class="panel-body">
          <div class="three">
            <button class="btn primary" :disabled="busy" @click="runAction('启动游戏', '/startup/game')">启动游戏</button>
            <button class="btn primary" :disabled="busy" @click="runAction('进入千星', '/startup/enter-wonderland')">进入千星</button>
            <button class="btn primary" :disabled="busy" @click="runAction('启动并进入', '/startup/wonderland')">启动并进入</button>
          </div>
        </div>
      </div>

      <div class="panel">
        <div class="panel-head"><h2>机器人操作</h2><small>排队执行</small></div>
        <div class="panel-body">
          <div class="controls">
            <button class="btn" :disabled="busy" @click="runAction('发送歌词', '/operator/lyrics')">发送歌词</button>
            <button class="btn" :disabled="busy" @click="runAction('麦克风', '/operator/microphone')">麦克风</button>
            <button class="btn" :disabled="busy" @click="runAction('大厅检测', '/operator/hall-detect')">大厅检测</button>
            <button class="btn" :disabled="busy" @click="runAction('大厅时间', '/operator/hall-time')">大厅时间</button>
            <button class="btn" :disabled="busy" @click="runAction('启用命令', '/operator/commands?enabled=1')">启用命令</button>
            <button class="btn warn" :disabled="busy" @click="runAction('禁用命令', '/operator/commands?enabled=0')">禁用命令</button>
            <button class="btn ghost" type="button" :disabled="Boolean(screenshotUrl)" @click="openScreenshot">查看截图</button>
          </div>
          <div class="row" style="margin-top:12px">
            <input v-model="idleExitMinutes" class="config-input" inputmode="numeric" placeholder="15-1440 分钟" aria-label="闲置退出分钟" />
            <button class="btn" :disabled="busy" @click="applyIdleExit">设置闲置退出</button>
            <button class="btn ghost" :disabled="busy" @click="clearIdleExit">取消</button>
          </div>
        </div>
      </div>

      <div class="panel wide">
        <div class="panel-head">
          <h2>最近聊天</h2>
          <small>{{ ocrMeta }}</small>
        </div>
        <div class="panel-body">
          <pre class="scroll" style="max-height:220px;min-height:88px">{{ recentChatText }}</pre>
        </div>
      </div>

      <div class="panel wide">
        <div class="panel-head"><h2>事件日志</h2><small>最近 {{ logLines.length }} 条</small></div>
        <div class="panel-body">
          <pre class="scroll" style="max-height:220px;min-height:88px">{{ logTailText }}</pre>
        </div>
      </div>

      <div class="panel">
        <div class="panel-head"><h2>运行状态</h2><small>/state · /state/save</small></div>
        <div class="panel-body">
          <div class="result" style="min-height:0">
            <div>大厅剩余：{{ runtimeHallLabel }}</div>
            <div>播放状态：{{ runtimePlaybackLabel }}</div>
            <div>队列：{{ runtimeQueueLabel }}</div>
          </div>
          <div class="row" style="margin-top:10px">
            <input v-model="hallMinutesDraft" class="config-input" inputmode="numeric" placeholder="大厅剩余分钟" aria-label="大厅剩余分钟" />
            <button class="btn" :disabled="busy" @click="saveHallMinutes">保存大厅时间</button>
            <button class="btn ghost" :disabled="busy" @click="clearHallMinutes">清除大厅时间</button>
          </div>
          <p class="config-field-hint">POST /state/save?json=…；会写入 hallRemainingMinutes 与当前时间戳。</p>
        </div>
      </div>

      <div class="panel">
        <div class="panel-head"><h2>工作流</h2><small>/operator/workflows</small></div>
        <div class="panel-body">
          <select v-model="workflowName" aria-label="工作流">
            <option value="">选择工作流</option>
            <option v-for="item in workflows" :key="item.name" :value="item.name">{{ item.name }}{{ item.confirmBeforeRun ? '（需确认）' : '' }}</option>
          </select>
          <input v-model="workflowArgs" class="config-input" style="margin-top:8px" placeholder="参数（可选）" aria-label="工作流参数" />
          <div class="controls" style="margin-top:8px">
            <button class="btn primary" :disabled="busy || !workflowName" @click="runWorkflow">执行工作流</button>
            <button class="btn ghost" :disabled="busy" @click="loadWorkflows">刷新列表</button>
          </div>
          <p class="config-field-hint">POST /operator/workflows/run?name=…&amp;args=…；标记需确认的工作流会先弹确认。</p>
        </div>
      </div>

      <div class="panel wide">
        <div class="panel-head">
          <h2>命令历史</h2>
          <small>{{ history.length }} 条 · /history</small>
        </div>
        <div class="panel-body">
          <div class="history" style="max-height:260px">
            <div v-for="(item, index) in history" :key="index" class="history-item" :class="{ err: item.ok === false }">
              <div><span class="history-time">{{ item.time || '' }}</span><b>{{ item.command || '未知命令' }}</b></div>
              <div class="history-result">{{ historyResult(item) }}</div>
            </div>
            <div v-if="!history.length" class="empty-hint">暂无命令记录</div>
          </div>
          <div class="controls" style="margin-top:8px">
            <button class="btn ghost" :disabled="busy" @click="loadHistory">刷新历史</button>
            <button class="btn warn" :disabled="busy || !history.length" @click="clearHistory">清空历史</button>
          </div>
        </div>
      </div>

      <div class="panel">
        <div class="panel-head"><h2>播放器</h2><small>/status</small></div>
        <div class="panel-body">
          <div class="result" style="min-height:0">
            <div>曲目：{{ playerTitle }}</div>
            <div>状态：{{ playerStateText }}</div>
            <div>队列：{{ playerQueueLabel }}</div>
          </div>
        </div>
      </div>
    </div>

    <p class="config-field-hint" style="margin-top:12px">
      每 5 秒刷新 /monitor 与 /status；发送与按钮操作只负责入队，实际执行以游戏值守任务为准。
    </p>
    <div v-if="screenshotUrl" class="screenshot-modal" @click.self="closeScreenshot">
      <div class="screenshot-modal-body">
        <div class="screenshot-modal-actions">
          <a class="btn small" :href="screenshotUrl" download="screenshot.jpg">下载</a>
          <button class="btn small" @click="closeScreenshot">关闭</button>
        </div>
        <img :src="screenshotUrl" alt="游戏截图" />
      </div>
    </div>
  </div>
</template>

<script setup>
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { fetchBlob, getJson, request } from '../api.js'
import { playerStateLabel } from '../util/format.js'
import { handleAuth, loadStatus, state, toast } from '../store.js'

const monitor = ref(null)
const loading = ref(false)
const busy = ref(false)
const refreshError = ref('')
const chatText = ref('')
const usePrefix = ref(true)
const prefix = ref('[控制台]: ')
const idleExitMinutes = ref('30')
const runtimeState = ref(null)
const hallMinutesDraft = ref('')
const workflows = ref([])
const workflowName = ref('')
const workflowArgs = ref('')
const history = ref([])

const quickCommands = ['@状态', '@歌单', '@随机歌单', '@点歌晴天']

const listener = computed(() => monitor.value?.chatListener || {})
const operational = computed(() => monitor.value?.operational || {})
const ocr = computed(() => monitor.value?.ocr || null)
const player = computed(() => state.player || null)

const serviceLabel = computed(() => {
  if (state.health) return monitor.value?.status || '运行中'
  return monitor.value?.status || '未知'
})
const uiStateLabel = computed(() => operational.value.uiState || '未知')
const listenerLabel = computed(() => displayMode(listener.value.mode) || '一级值守')
const commandsEnabled = computed(() => operational.value.commandsEnabled !== false)
const hallRemainingLabel = computed(() => {
  const minutes = operational.value.hallRemainingMinutes
  if (minutes == null) return '未知'
  const updated = Number(operational.value.hallRemainingUpdatedAt || 0)
  const remaining = updated > 0
    ? Math.max(0, Number(minutes) - Math.floor(Math.max(0, Date.now() / 1000 - updated) / 60))
    : Number(minutes)
  return remaining === 0 ? '即将到期' : `${remaining} 分钟`
})
const idleExitLabel = computed(() => {
  const seconds = operational.value.idleExitRemainingSeconds
  return seconds == null ? '未启用' : `${Math.ceil(Number(seconds) / 60)} 分钟`
})
const modeIsPrimary = computed(() => displayMode(listener.value.mode) === '一级值守')
const modeIsSecondary = computed(() => displayMode(listener.value.mode) === '二级值守')

const listenerDetailLines = computed(() => {
  const item = listener.value
  const rows = [
    `模式：${displayMode(item.mode) || '一级值守'}`,
    `命令：${commandsEnabled.value ? '已启用' : '已禁用'}`,
    `大厅：${hallRemainingLabel.value}`,
    `闲置退出：${idleExitLabel.value}`,
  ]
  if (item.pendingMode) rows.push(`等待切换：${displayMode(item.pendingMode)}`)
  if (item.temporaryPrimary) rows.push('监听正在临时一级阶段')
  if (item.initialUnreadClear) rows.push('正在清理初始未读')
  if (item.unreadTaskPending) rows.push('正在处理好友未读')
  if (item.hallRoundRequired) rows.push('等待返回当前大厅扫描')
  if (operational.value.scannerPaused) rows.push('扫描已暂停')
  return rows
})

const ocrMeta = computed(() => {
  const value = ocr.value
  if (!value) return '暂无采样'
  return `${value.source || '聊天'} · ${value.messages?.length || 0} 条`
})
const recentChatText = computed(() => {
  const messages = ocr.value?.messages
  if (!Array.isArray(messages) || !messages.length) return '暂无 OCR 聊天内容'
  return messages.join('\n')
})
const logLines = computed(() => (Array.isArray(monitor.value?.logs) ? monitor.value.logs : []))
const logTailText = computed(() => (logLines.value.length ? logLines.value.slice(-160).join('\n') : '暂无日志'))

const playerTitle = computed(() => {
  const metadata = player.value?.currentTrack?.metadata
  return metadata?.title || (player.value ? '空闲' : '未知')
})
const playerStateText = computed(() => {
  const value = player.value
  if (!value) return '未知'
  const raw = value.status || value.state || value.transport || 'idle'
  return playerStateLabel(raw)
})
const playerQueueLabel = computed(() => {
  const queue = state.monitor?.queue
  return Array.isArray(queue) ? `${queue.length} 首` : '—'
})
const runtimeHallLabel = computed(() => {
  const value = runtimeState.value?.hallRemainingMinutesNow ?? runtimeState.value?.hallRemainingMinutes
  return value == null ? '未知' : `${value} 分钟`
})
const runtimePlaybackLabel = computed(() => {
  const playback = runtimeState.value?.playback || {}
  return playback.status || playback.state || '未知'
})
const runtimeQueueLabel = computed(() => {
  const queue = runtimeState.value?.playback?.queue
  return Array.isArray(queue) ? `${queue.length} 首` : '—'
})

function displayMode(mode) {
  if (!mode) return ''
  if (mode === 'primary' || mode.startsWith('一级')) return '一级值守'
  if (mode === 'secondary' || mode.startsWith('二级')) return '二级值守'
  return mode
}

function describeResponse(text) {
  if (!text) return '已提交'
  try {
    const data = JSON.parse(text)
    if (data && typeof data === 'object') {
      if (data.message) return String(data.message)
      if (data.ok === true) return '已提交'
      if (data.queued) return '已入队'
    }
  } catch {}
  return String(text).slice(0, 200)
}

async function loadState() {
  try {
    runtimeState.value = await getJson('/state')
    const minutes = runtimeState.value?.hallRemainingMinutesNow ?? runtimeState.value?.hallRemainingMinutes
    if (document.activeElement?.tagName !== 'INPUT') hallMinutesDraft.value = minutes == null ? '' : String(minutes)
  } catch (error) {
    toast(`读取 /state 失败：${error.message || error}`, 'err')
  }
}
async function loadWorkflows() {
  try {
    const value = await getJson('/operator/workflows')
    workflows.value = Array.isArray(value) ? value : (Array.isArray(value?.workflows) ? value.workflows : [])
    if (!workflows.value.some((item) => item.name === workflowName.value)) workflowName.value = ''
  } catch (error) {
    toast(`读取工作流失败：${error.message || error}`, 'err')
  }
}
async function loadHistory() {
  try {
    const value = await getJson('/history')
    history.value = Array.isArray(value) ? value : []
  } catch (error) {
    toast(`读取命令历史失败：${error.message || error}`, 'err')
  }
}
function historyResult(item) {
  const query = item.query && typeof item.query === 'object'
    ? Object.entries(item.query).map(([key, value]) => `${key}=${value}`).join(' ')
    : ''
  return [query, item.result || ''].filter(Boolean).join(' · ') || '—'
}
async function saveHallMinutes() {
  const minutes = Number(hallMinutesDraft.value)
  if (!Number.isInteger(minutes) || minutes < 0) {
    toast('大厅剩余分钟必须是非负整数', 'err')
    return
  }
  if (!window.confirm(`把大厅剩余时间设置为 ${minutes} 分钟？`)) return
  const patch = { hallRemainingMinutes: minutes, hallRemainingUpdatedAt: Math.floor(Date.now() / 1000) }
  if (await action('保存大厅时间', `/state/save?json=${encodeURIComponent(JSON.stringify(patch))}`)) {
    await loadState()
  }
}
async function clearHallMinutes() {
  if (!window.confirm('清除大厅剩余时间与告警状态？')) return
  const patch = { hallRemainingMinutes: null, hallRemainingUpdatedAt: null, hallExpiringWarningSent: false }
  if (await action('清除大厅时间', `/state/save?json=${encodeURIComponent(JSON.stringify(patch))}`)) {
    await loadState()
  }
}
async function runWorkflow() {
  const selected = workflows.value.find((item) => item.name === workflowName.value)
  if (!selected) {
    toast('请选择工作流', 'err')
    return
  }
  const hint = selected.confirmBeforeRun ? '该工作流标记为需确认。' : '执行后可能触发游戏操作。'
  if (!window.confirm(`工作流「${selected.name}」：${hint}确定执行？`)) return
  const query = new URLSearchParams({ name: selected.name })
  if (workflowArgs.value.trim()) query.set('args', workflowArgs.value.trim())
  await action('执行工作流', `/operator/workflows/run?${query}`)
}
async function clearHistory() {
  if (!window.confirm('确定清空全部命令历史？')) return
  if (await action('清空历史', '/clear-history')) await loadHistory()
}

async function refresh(manual = false) {
  if (loading.value) return
  loading.value = true
  refreshError.value = ''
  try {
    await loadStatus()
    monitor.value = state.monitor
  } catch (error) {
    refreshError.value = `读取 /monitor 失败：${error.message || error}`
  } finally {
    loading.value = false
  }
  await loadStatus()
  if (manual) {
    await Promise.allSettled([loadState(), loadWorkflows(), loadHistory()])
    toast('已刷新游戏与聊天状态')
  }
}

async function action(label, path) {
  if (busy.value) return false
  busy.value = true
  let ok = false
  try {
    const text = await request(path, 'POST')
    toast(`${label}：${describeResponse(text)}`)
    ok = true
  } catch (error) {
    toast(`${label}失败：${error.message || error}`, 'err')
  } finally {
    busy.value = false
    await refresh()
  }
  return ok
}

const HIGH_RISK_ACTIONS = new Set(['启动游戏', '进入千星', '启动并进入', '禁用命令'])

const screenshotUrl = ref('')

async function openScreenshot() {
  try {
    const blob = await fetchBlob('/screenshot')
    if (screenshotUrl.value) URL.revokeObjectURL(screenshotUrl.value)
    screenshotUrl.value = URL.createObjectURL(blob)
  } catch (error) {
    if (!handleAuth(error)) toast(`截图失败：${error.message || error}`, 'err')
  }
}

function closeScreenshot() {
  if (screenshotUrl.value) URL.revokeObjectURL(screenshotUrl.value)
  screenshotUrl.value = ''
}

function runAction(label, path) {
  if (HIGH_RISK_ACTIONS.has(label) && !window.confirm(`确认执行「${label}」？这会真实操作游戏。`)) return
  return action(label, path)
}

async function sendChat() {
  const text = chatText.value.trim()
  if (!text) return
  if (text.length > 500) {
    toast('发送内容过长（上限 500 字）', 'err')
    return
  }
  const params = new URLSearchParams({ text, usePrefix: usePrefix.value ? '1' : '0' })
  if (usePrefix.value) params.set('prefix', prefix.value || '[控制台]: ')
  if (await action('发送聊天', `/chat/send?${params.toString()}`)) chatText.value = ''
}

function setListener(mode) {
  return action('切换值守', `/chat-listener/mode?mode=${encodeURIComponent(mode)}`)
}

function applyIdleExit() {
  const value = String(idleExitMinutes.value || '').trim()
  if (!/^\d+$/.test(value) || Number(value) < 15 || Number(value) > 1440) {
    toast('闲置退出必须是 15 到 1440 分钟', 'err')
    return
  }
  return action('设置闲置退出', `/operator/idle-exit?minutes=${encodeURIComponent(value)}`)
}

function clearIdleExit() {
  return action('取消闲置退出', '/operator/idle-exit?enabled=0')
}

let timer = null
onMounted(() => {
  refresh()
  loadState()
  loadWorkflows()
  loadHistory()
  timer = setInterval(() => {
    if (!document.hidden) refresh()
  }, 5000)
})
onBeforeUnmount(() => {
  clearInterval(timer)
})
</script>

<style scoped>
.game-page :deep(.panel-body) { display: block; }
.game-page .wide { grid-column: 1 / -1; }
.game-page .switch-line { font-size: 13px; color: var(--text-2); white-space: nowrap; }
.screenshot-modal { position: fixed; inset: 0; z-index: 120; background: rgba(0, 0, 0, 0.72); display: flex; align-items: center; justify-content: center; }
.screenshot-modal-body { max-width: 92vw; max-height: 92vh; background: var(--bg-1); border-radius: 12px; padding: 12px; display: flex; flex-direction: column; gap: 8px; }
.screenshot-modal-actions { display: flex; gap: 8px; justify-content: flex-end; }
.screenshot-modal-body img { max-width: 86vw; max-height: 78vh; object-fit: contain; }
</style>
