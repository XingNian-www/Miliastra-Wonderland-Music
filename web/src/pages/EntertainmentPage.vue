<template>
  <div class="entertainment-page">
    <h1 class="page-title">娱乐玩法</h1>
    <p class="page-subtitle">海龟汤与谁是卧底对局管理</p>

    <div class="tabs" role="tablist" aria-label="娱乐玩法">
      <button class="tab" :class="{ active: tab === 'turtle' }" type="button" @click="tab = 'turtle'">
        海龟汤
        <span class="tab-meta">{{ turtleMeta }}</span>
      </button>
      <button class="tab" :class="{ active: tab === 'undercover' }" type="button" @click="tab = 'undercover'">
        谁是卧底
        <span class="tab-meta">{{ undercoverMeta }}</span>
      </button>
    </div>

    <div v-if="loadError" class="config-message err" style="margin:12px 0">
      {{ loadError }}
      <div class="config-field-hint">页面仍保留手动刷新与操作入口；如果后端版本较旧，可能没有 /monitor 的 turtleSoup / undercover 字段。</div>
    </div>
    <div v-if="loading && !loaded" class="loading-card">正在读取对局状态…</div>

    <div v-show="tab === 'turtle'" class="page-grid">
      <div class="panel wide">
        <div class="panel-head">
          <h2>海龟汤</h2>
          <small>{{ turtleEnabled ? turtleMeta : '功能未启用' }}</small>
        </div>
        <div class="panel-body">
          <div class="turtle-toolbar">
            <input v-model="puzzleId" class="config-input" placeholder="题目 ID（随机开局可留空）" aria-label="海龟汤题目 ID" />
            <button class="btn primary" :disabled="busy || !turtleEnabled || turtleActive" @click="startTurtle(false)">随机开局</button>
            <button class="btn" :disabled="busy || !turtleEnabled || turtleActive" @click="startTurtle(true)">按 ID 开局</button>
            <button class="btn warn" :disabled="busy || !turtleEnabled || !turtleActive || turtle.phase === 'settling'" @click="endTurtle">结束</button>
            <button class="btn ghost" type="button" @click="showSubmit = !showSubmit">{{ showSubmit ? '收起投稿' : '投稿新题' }}</button>
          </div>

          <div class="turtle-stats">
            <div class="turtle-stat"><span>状态</span><strong>{{ turtle.phaseLabel || '空闲' }}</strong></div>
            <div class="turtle-stat"><span>时长</span><strong>{{ formatSeconds(turtle.elapsedSeconds) }}</strong></div>
            <div class="turtle-stat"><span>参与者</span><strong>{{ numberOrDash(turtle.participantCount) }}</strong></div>
            <div class="turtle-stat"><span>有效提问</span><strong>{{ numberOrDash(turtle.questionCount) }}</strong></div>
            <div class="turtle-stat"><span>AI 待处理</span><strong>{{ numberOrDash(turtle.pendingAi) }}</strong></div>
            <div class="turtle-stat"><span>剩余题目</span><strong>{{ turtle.remainingPuzzles == null ? '未知' : turtle.remainingPuzzles }}</strong></div>
          </div>

          <div class="result" style="margin-top:12px;min-height:72px">
            <div v-for="(line, index) in turtleSurfaceLines" :key="index">{{ line }}</div>
          </div>

          <div class="turtle-grid">
            <div class="history turtle-history" style="max-height:220px">
              <template v-if="turtleJudgments.length">
                <div v-for="item in turtleJudgments" :key="item.requestId" class="history-item">
                  <div>
                    <span class="history-time">#{{ item.requestId }}</span>
                    <b>{{ item.player || '未知玩家' }}</b> · {{ item.judgmentLabel || item.judgment || '未知' }}
                  </div>
                  <div class="history-result">{{ item.question || '' }} · {{ Math.round(Number(item.elapsedMs || 0)) }}ms</div>
                </div>
              </template>
              <div v-else>暂无裁决</div>
            </div>
          </div>

          <div v-if="showSubmit" class="turtle-submit">
            <input v-model="newTitle" class="config-input" placeholder="题目标题" aria-label="新题标题" />
            <textarea v-model="newSurface" class="config-textarea" placeholder="汤面（公布给玩家的谜面）" aria-label="新题汤面" rows="3"></textarea>
            <textarea v-model="newBottom" class="config-textarea" placeholder="汤底（真相，结算时公布）" aria-label="新题汤底" rows="3"></textarea>
            <div class="row">
              <button class="btn primary" :disabled="busy || !turtleEnabled" @click="submitQuestion">提交题目</button>
              <span class="config-field-hint">提交后进入题库，可按 ID 或随机抽取。</span>
            </div>
          </div>
        </div>
      </div>
    </div>

    <div v-show="tab === 'undercover'" class="page-grid">
      <div class="panel wide">
        <div class="panel-head">
          <h2>谁是卧底</h2>
          <small>{{ undercoverEnabled ? undercoverMeta : '功能未启用' }}</small>
        </div>
        <div class="panel-body">
          <div class="undercover-toolbar">
            <button class="btn primary" :disabled="busy || !undercoverEnabled || undercover.phase !== 'lobby'" @click="startUndercover">开始当前房间</button>
            <button class="btn warn" :disabled="busy || !undercoverEnabled || undercover.phase === 'idle'" @click="endUndercover">结束</button>
          </div>

          <div class="turtle-stats">
            <div class="turtle-stat"><span>状态</span><strong>{{ undercoverPhaseLabel }}</strong></div>
            <div class="turtle-stat"><span>模式</span><strong>{{ undercoverModeLabel }}</strong></div>
            <div class="turtle-stat"><span>轮次</span><strong>{{ numberOrDash(undercover.round) }}</strong></div>
            <div class="turtle-stat"><span>进度</span><strong>{{ undercover.completed || 0 }} / {{ undercover.total || 0 }}</strong></div>
            <div class="turtle-stat"><span>人数</span><strong>{{ undercoverPlayers.length }}</strong></div>
            <div class="turtle-stat"><span>剩余</span><strong>{{ formatSeconds(undercover.remainingSeconds) }}</strong></div>
          </div>

          <div class="undercover-players" style="margin-top:12px">
            <div v-for="(player, index) in undercoverPlayers" :key="`${player.name}-${index}`" class="undercover-player">
              <span class="undercover-position">{{ player.position || index + 1 }}</span>
              <span class="undercover-name">{{ player.name || '未知玩家' }}</span>
              <span class="undercover-state" :class="{ out: !player.alive }">{{ player.alive ? '在场' : '已出局' }}</span>
            </div>
            <div v-if="!undercoverPlayers.length" class="config-field-hint" style="padding:12px">
              {{ undercoverEnabled ? '当前没有报名或进行中的对局' : '谁是卧底功能未启用' }}
            </div>
          </div>
        </div>
      </div>
    </div>

    <p class="config-field-hint" style="margin-top:12px">
      每 5 秒刷新一次对局状态；开始/结束只提交控制意图，实际播报与游戏内流程以值守任务为准。
    </p>
  </div>
</template>

<script setup>
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { getJson, request, requestJson } from '../api.js'
import { toast } from '../store.js'

const tab = ref('turtle')
const loading = ref(false)
const loaded = ref(false)
const busy = ref(false)
const loadError = ref('')
const showSubmit = ref(false)
const puzzleId = ref('')
const newTitle = ref('')
const newSurface = ref('')
const newBottom = ref('')
const turtle = ref({})
const undercover = ref({})

const turtleEnabled = computed(() => turtle.value?.enabled === true)
const turtleActive = computed(() => Boolean(turtle.value?.phase) && turtle.value.phase !== 'idle')
const undercoverEnabled = computed(() => undercover.value?.enabled === true)
const undercoverPlayers = computed(() => (Array.isArray(undercover.value?.players) ? undercover.value.players : []))
const turtleJudgments = computed(() => {
  const items = Array.isArray(turtle.value?.recentJudgments) ? turtle.value.recentJudgments : []
  return items.slice(-30).reverse()
})
const turtleMeta = computed(() => {
  if (!turtleEnabled.value) return '功能未启用'
  const title = turtle.value?.title || turtle.value?.phaseLabel || '空闲'
  return turtle.value?.puzzleId ? `ID ${turtle.value.puzzleId} / ${title}` : title
})
const undercoverMeta = computed(() => {
  if (!undercoverEnabled.value) return '功能未启用'
  return undercoverPhaseLabel.value
})
const undercoverPhaseLabel = computed(() => {
  const labels = {
    idle: '空闲',
    lobby: '报名中',
    delivering: '发词中',
    describing: '描述中',
    voting: '投票中',
    runoff_describing: '平票描述',
    runoff_voting: '平票投票',
  }
  const phase = undercover.value?.phase || 'idle'
  return labels[phase] || phase
})
const undercoverModeLabel = computed(() => {
  const labels = { single: '单卧底', double: '双卧底' }
  const mode = undercover.value?.mode
  return mode ? labels[mode] || mode : '-'
})
const turtleSurfaceLines = computed(() => {
  const value = turtle.value || {}
  const lines = []
  if (turtleActive.value) {
    if (value.starter) lines.push(`开局者：${value.starter}`)
    if (Array.isArray(value.participants) && value.participants.length) lines.push(`参与者：${value.participants.join('、')}`)
    lines.push(`汤面：${value.surface || '等待公布'}`)
  } else {
    lines.push(turtleEnabled.value ? '当前没有进行中的海龟汤' : '海龟汤功能未启用')
  }
  if (value.settlementIncomplete) lines.push('上次结算消息发送不完整')
  if (value.lastError) lines.push(`状态：${value.lastError}`)
  return lines
})

function numberOrDash(value) {
  return value == null ? '—' : String(value)
}
function formatSeconds(value) {
  const total = Math.max(0, Number(value || 0))
  if (total < 60) return `${total}s`
  const minutes = Math.floor(total / 60)
  const seconds = Math.floor(total % 60)
  return `${minutes}m ${String(seconds).padStart(2, '0')}s`
}

async function refresh(manual = false) {
  if (loading.value) return
  loading.value = true
  loadError.value = ''
  try {
    const monitor = await getJson('/monitor')
    turtle.value = monitor?.turtleSoup || {}
    undercover.value = monitor?.undercover || {}
    loaded.value = true
  } catch (error) {
    // /monitor 不可用时退回到两个独立只读接口；仍失败则给出说明而不是白屏。
    const [turtleResult, undercoverResult] = await Promise.allSettled([
      getJson('/turtle-soup'),
      getJson('/undercover'),
    ])
    if (turtleResult.status === 'fulfilled') turtle.value = turtleResult.value || {}
    if (undercoverResult.status === 'fulfilled') undercover.value = undercoverResult.value || {}
    const fallbackOk = turtleResult.status === 'fulfilled' || undercoverResult.status === 'fulfilled'
    loaded.value = loaded.value || fallbackOk
    loadError.value = `读取对局状态失败：${error.message || error}`
  } finally {
    loading.value = false
  }
  if (manual) toast('已刷新对局状态')
}

async function call(label, path, body, confirmText) {
  if (busy.value) return
  if (confirmText && !window.confirm(confirmText)) return
  busy.value = true
  try {
    const response = body === undefined
      ? await request(path, 'POST')
      : JSON.stringify(await requestJson(path, body))
    toast(`${label}：${describeResponse(response)}`)
    if (body !== undefined) {
      newTitle.value = ''
      newSurface.value = ''
      newBottom.value = ''
      showSubmit.value = false
    }
  } catch (error) {
    toast(`${label}失败：${error.message || error}`, 'err')
  } finally {
    busy.value = false
    await refresh()
  }
}

function describeResponse(text) {
  if (!text) return '已提交'
  try {
    const data = JSON.parse(text)
    if (data && typeof data === 'object') {
      if (data.message) return String(data.message)
      if (data.ok === true) return '已提交'
    }
  } catch {}
  return String(text).slice(0, 200)
}

function startTurtle(useId) {
  const id = puzzleId.value.trim()
  if (useId && !id) {
    toast('请输入题目 ID', 'err')
    return
  }
  return call(useId ? '按 ID 开局' : '随机开局', `/turtle-soup/start${useId ? `?id=${encodeURIComponent(id)}` : ''}`)
}
function endTurtle() {
  return call('结束海龟汤', '/turtle-soup/end', undefined, '确定结束当前海龟汤并在大厅公布汤底？')
}
function submitQuestion() {
  const title = newTitle.value.trim()
  const surface = newSurface.value.trim()
  const bottom = newBottom.value.trim()
  if (!title || !surface || !bottom) {
    toast('标题、汤面、汤底都需要填写', 'err')
    return
  }
  return call('提交题目', '/turtle-soup/questions', { title, surface, bottom })
}
function startUndercover() {
  return call('开始谁是卧底', '/undercover/start')
}
function endUndercover() {
  return call('结束谁是卧底', '/undercover/end', undefined, '确定结束当前谁是卧底房间或对局？')
}

let timer = null
onMounted(() => {
  refresh()
  timer = setInterval(() => {
    if (!document.hidden) refresh()
  }, 5000)
})
onBeforeUnmount(() => {
  clearInterval(timer)
})
</script>

<style scoped>
.entertainment-page .wide { grid-column: 1 / -1; }
.entertainment-page .tab-meta { margin-left: 6px; color: var(--text-3); font-size: 12px; }
.entertainment-page .turtle-toolbar .config-input { min-width: 0; }
</style>
