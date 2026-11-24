<template>
  <h1 class="page-title">播放与点歌</h1>
  <p class="page-subtitle">当前曲目、播放控制、点歌队列与磁盘缓存</p>

  <div class="page-grid playback-layout">
    <section class="panel playback-player-panel">
      <div class="panel-head">
        <h2>正在播放</h2>
        <small>{{ statusText }}<template v-if="lastUpdated"> · {{ lastUpdated }}</template><template v-if="player?.lyricLineText"> · {{ player.lyricLineText }}</template></small>
      </div>
      <div class="panel-body">
        <div class="current-song">{{ title }}</div>
        <div class="now-playing-sub">{{ subtitle }}</div>

        <div class="playback-progress">
          <div class="progress-track">
            <i :style="{ width: progressPercent + '%' }"></i>
            <input
              v-model.number="seekPercent"
              type="range"
              min="0"
              max="1000"
              step="1"
              aria-label="播放进度"
              :disabled="!duration"
              @input="dragging = true"
              @change="seek"
            />
          </div>
          <div class="progress-row">
            <span>{{ timeText(player?.progress) }} / {{ timeText(duration) }}</span>
            <span>{{ player?.failureCode ? '失败：' + (player.failureMessage || player.failureCode) : (player?.lyricLineText || '') }}</span>
          </div>
        </div>

        <div class="play-mode-row">
          <span class="play-mode-label">播放模式</span>
          <button
            v-for="mode in playModes"
            :key="mode.value"
            class="btn small"
            :class="{ active: playMode === mode.value }"
            :disabled="busy"
            @click="setPlayMode(mode.value)"
          >{{ mode.label }}</button>
        </div>

        <div class="controls">
          <button class="btn good" :disabled="busy" @click="action('/play')">继续</button>
          <button class="btn warn" :disabled="busy" @click="action('/pause')">暂停</button>
          <button class="btn" :disabled="busy" @click="action('/skip-prev')">上一首</button>
          <button class="btn" :disabled="busy" @click="action('/skip-next')">下一首</button>
          <button class="btn" :disabled="busy" @click="refreshAll">刷新状态</button>
        </div>

        <div class="volume-control">
          <div class="volume-row">
            <span aria-hidden="true">🔈</span>
            <input v-model.number="volumeDraft" type="range" min="0" max="100" step="5" aria-label="音量" @change="applyVolume" />
            <span aria-hidden="true">🔊</span>
            <label class="volume-input">
              <input v-model.number="volumeDraft" type="number" min="0" max="100" step="1" aria-label="音量百分比" @change="applyVolume" />
              <span>%</span>
            </label>
          </div>
        </div>

        <div v-if="actionText" class="result">{{ actionText }}</div>
      </div>
    </section>

    <section class="panel playback-request-panel">
      <div class="panel-head">
        <h2>点歌搜索</h2>
        <small>排队执行</small>
      </div>
      <div class="panel-body">
        <input
          v-model.trim="keyword"
          placeholder="歌名 / 歌手 / 关键词"
          aria-label="歌名、歌手或关键词"
          @keydown.enter.prevent="searchCandidates"
        />
        <div class="compact-form">
          <select v-model="source" aria-label="音源平台">
            <option value="">不指定平台</option>
            <option value="qqmusic">QQ音乐</option>
            <option value="netease">网易云音乐</option>
            <option value="bilibili">哔哩哔哩</option>
            <option value="kugou">酷狗音乐</option>
          </select>
          <label class="switch-line"><input v-model="preferAccompaniment" type="checkbox" />伴奏</label>
        </div>
        <div class="controls">
          <button class="btn primary" :disabled="busy || !keyword" @click="requestSong(false)">点歌</button>
          <button class="btn" :disabled="busy || !keyword" @click="requestSong(true)">AI 点歌</button>
          <button class="btn ghost" :disabled="busy || !keyword" @click="enqueue">加入队列</button>
          <button class="btn ghost" :disabled="busy || !keyword" @click="searchCandidates">搜索候选</button>
          <button class="btn ghost" :disabled="busy || !keyword || !source" @click="requestSourceSong">指定源点歌</button>
          <button class="btn ghost" :disabled="busy || !keyword" @click="searchRaw">文本搜索</button>
        </div>

        <div v-if="candidates.length" class="candidate-list playback-mac-candidates">
          <div v-for="(item, index) in candidates" :key="candidateKey(item, index)" class="candidate-item">
            <div>
              <div class="queue-title">{{ item.text || '未命名候选' }}</div>
              <div class="candidate-uri">
                {{ candidateIdentity(item) }} ·
                <span class="candidate-eligibility" :data-eligibility="item.eligibility || 'unknown'">
                  {{ eligibilityLabels[item.eligibility] || item.eligibility || '未知' }}
                </span>
              </div>
            </div>
            <div class="candidate-actions">
              <button class="btn small" :disabled="busy || !item.trackRef || !item.metadata" @click="playCandidate(item)">播放</button>
              <button class="btn small ghost" :disabled="busy || !item.text" @click="testCandidate(item)">测试点歌</button>
            </div>
          </div>
        </div>
        <div v-if="rawSearchResult" class="result">{{ rawSearchResult }}</div>
        <div class="config-field-hint">{{ candidateHint }}</div>
      </div>
    </section>

    <section class="panel playback-queue-panel">
      <div class="panel-head">
        <h2>待播队列</h2>
        <small>{{ queue.length }} 首</small>
      </div>
      <div class="panel-body">
        <div class="queue-list playback-mac-list">
          <div v-for="(item, index) in queue" :key="item.id ?? index" class="queue-item">
            <div class="queue-index">{{ index + 1 }}</div>
            <div class="playback-mac-list-main">
              <div class="queue-title">{{ item.keyword || '未命名歌曲' }}</div>
              <div class="queue-meta">{{ queueMeta(item) }}</div>
            </div>
            <button class="btn small ghost" :disabled="busy" @click="removeQueue(item)">删除</button>
          </div>
          <div v-if="!queue.length" class="empty-hint">队列为空</div>
        </div>
        <div class="controls">
          <button class="btn ghost" :disabled="busy" @click="loadQueue">刷新队列</button>
          <button class="btn warn" :disabled="busy || !queue.length" @click="clearQueue">清空</button>
        </div>
      </div>
    </section>

    <section class="panel playback-cache-stats-panel">
      <div class="panel-head">
        <h2>缓存统计</h2>
        <small>{{ cache?.enabled ? '已启用' : '未启用' }}</small>
      </div>
      <div class="panel-body">
        <div class="login-facts playback-mac-stats">
          <div class="login-fact"><span>音频</span><strong>{{ cache?.completeEntries ?? 0 }} 首</strong></div>
          <div class="login-fact"><span>占用</span><strong>{{ formatBytes(cache?.completeBytes) }} <small>/ {{ formatBytes(cache?.maxBytes) }}</small></strong></div>
          <div class="login-fact"><span>下载中</span><strong>{{ cache?.activeDownloads ?? 0 }}</strong></div>
          <div class="login-fact"><span>播放 / 失败</span><strong>{{ cache?.playCount ?? 0 }} <small>/ {{ cache?.failureCount ?? 0 }}</small></strong></div>
          <div class="login-fact"><span>点歌 / 池</span><strong>{{ cache?.requestedPlayCount ?? 0 }} <small>/ {{ cache?.poolPlayCount ?? 0 }}</small></strong></div>
          <div class="login-fact"><span>命中</span><strong>{{ cache?.cacheHitCount ?? 0 }} <small>次</small></strong></div>
        </div>
        <div class="config-field-hint">目录：{{ cache?.directory || '未知' }}</div>
      </div>
    </section>

    <section class="panel playback-history-panel">
      <div class="panel-head">
        <h2>播放历史</h2>
        <small>最近 {{ history.length }} 首</small>
      </div>
      <div class="panel-body">
        <div class="history playback-mac-log">
          <div v-for="(item, index) in history" :key="index" class="history-item">
            <div class="playback-mac-log-head">
              <span class="history-time">{{ formatDate(item.startedAtMs) }}</span>
              <b>{{ historyTitle(item) }}</b>
            </div>
            <div class="history-result">
              {{ sourceLabel(item.source) }} · {{ item.cached ? '已缓存' : '未缓存' }}
              · 播放 {{ item.playCount ?? 0 }}（点歌 {{ item.requestedPlayCount ?? 0 }} / 池 {{ item.poolPlayCount ?? 0 }}）
              · 命中 {{ item.cacheHitCount ?? 0 }} · 失败 {{ item.failureCount ?? 0 }}
              <template v-if="item.requester"> · {{ item.requester }}</template>
            </div>
          </div>
          <div v-if="!history.length" class="empty-hint">暂无播放历史</div>
        </div>
      </div>
    </section>

    <section class="panel playback-leaderboard-panel">
      <div class="panel-head">
        <h2>点歌榜</h2>
        <small>{{ leaderboardRangeLabel }}</small>
      </div>
      <div class="panel-body">
        <div class="controls">
          <select v-model.number="leaderboardDays" aria-label="点歌榜时间范围" @change="loadLeaderboard">
            <option :value="0">全部</option>
            <option :value="7">最近 7 天</option>
            <option :value="1">最近 24 小时</option>
            <option :value="30">最近 30 天</option>
          </select>
          <button class="btn ghost" :disabled="busy" @click="loadLeaderboard">刷新</button>
        </div>
        <div class="history playback-mac-log">
          <div v-for="(item, index) in leaderboard" :key="index" class="history-item">
            <div class="playback-mac-log-head">
              <span class="history-time">#{{ index + 1 }}</span>
              <b>{{ item.requester || '匿名' }}</b>
              <span class="playback-mac-count">{{ item.count ?? 0 }} 次</span>
            </div>
            <div class="history-result">最近播放确认 {{ formatDate(item.lastRequestedAtMs) }}</div>
          </div>
          <div v-if="!leaderboard.length" class="empty-hint">该时间范围内暂无成功点歌记录</div>
        </div>
      </div>
    </section>

    <section class="panel playback-cache-tracks-panel">
      <div class="panel-head">
        <h2>磁盘缓存歌曲</h2>
        <small>{{ cacheTotal }} 首</small>
      </div>
      <div class="panel-body">
        <select v-model="cacheSort" aria-label="缓存歌曲排序" @change="resetCachePage">
          <option value="last_used:desc">最近使用（新→旧）</option>
          <option value="last_used:asc">最近使用（旧→新）</option>
          <option value="requested_play_count:desc">点歌次数（多→少）</option>
          <option value="play_count:desc">播放次数（多→少）</option>
          <option value="cache_hit_count:desc">缓存命中（多→少）</option>
          <option value="bytes:desc">文件大小（大→小）</option>
        </select>
        <div class="cache-track-list playback-mac-cache-list">
          <div
            v-for="(track, index) in cacheTracks"
            :key="(track.source || 'unknown') + '/' + (track.id || index)"
            class="cache-track-item"
            :class="{ orphan: !track.source }"
          >
            <div class="playback-mac-list-main">
              <b>{{ cacheTitle(track) }}</b>
              <span class="cache-track-source">{{ sourceLabel(track.source) }}</span>
              <div class="controls playback-mac-track-actions">
                <button class="btn small ghost" :disabled="busy || !track.source || !track.id" @click="playCached(track)">播放</button>
                <button class="btn small ghost" :disabled="busy || !track.source || !track.id" @click="invalidateCached(track)">仅清缓存</button>
                <button class="btn small ghost" :disabled="busy || !track.source || !track.id" @click="deleteCached(track)">删除歌曲</button>
                <button class="btn small ghost" :disabled="busy || !track.source || !track.id" @click="resetTrackStats(track)">清零统计</button>
              </div>
            </div>
            <div class="cache-track-meta">
              {{ formatBytes(track.bytes) }} · 最近使用 {{ formatDate(track.lastUsedAtMs) }} · {{ track.complete ? '已缓存' : '未完整' }}
              · 播放 {{ track.playCount ?? 0 }}（点歌 {{ track.requestedPlayCount ?? 0 }} / 池 {{ track.poolPlayCount ?? 0 }}）
              · 命中 {{ track.cacheHitCount ?? 0 }} · 失败 {{ track.failureCount ?? 0 }}
              <template v-if="track.lastFailureCode"> · 最近失败 {{ track.lastFailureCode }}</template>
            </div>
          </div>
          <div v-if="!cacheTracks.length" class="empty-hint">暂无磁盘缓存歌曲</div>
        </div>
        <div class="controls">
          <button class="btn ghost" :disabled="busy || cacheOffset <= 0" @click="changeCachePage(-1)">上一页</button>
          <button class="btn ghost" :disabled="busy || cacheOffset + cacheLimit >= cacheTotal" @click="changeCachePage(1)">下一页</button>
        </div>
      </div>
    </section>

  </div>
</template>

<script setup>
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { getJson, requestJson } from '../api.js'
import { loadStatus as refreshStoreStatus, state as storeState } from '../store.js'
import { handleAuth, state, toast } from '../store.js'

const player = ref(state.player || null)
const queue = ref([])
const keyword = ref('')
const source = ref('')
const preferAccompaniment = ref(false)
const candidates = ref([])
const candidateHint = ref('输入关键词后点「搜索候选」，再播放或测试点歌。')
const rawSearchResult = ref('')
const searchVersion = ref(0)
const busy = ref(false)
const searching = ref(false)
const actionText = ref('')
const playMode = ref('shuffle')
const volumeDraft = ref(Number(player.value?.volume ?? 80))
const seekPercent = ref(0)
const dragging = ref(false)
const cache = ref(null)
const history = ref([])
const cacheTracks = ref([])
const cacheTotal = ref(0)
const cacheOffset = ref(0)
const cacheLimit = 100
const cacheSort = ref('last_used:desc')
const leaderboard = ref([])
const leaderboardDays = ref(7)
const lastUpdated = ref('')
let timer = null
let refreshing = false

const playModes = [
  { value: 'sequential', label: '顺序' },
  { value: 'repeat_one', label: '单曲循环' },
  { value: 'shuffle', label: '随机' },
]
const eligibilityLabels = {
  eligible: '可播放',
  vip: '需要 VIP',
  no_copyright: '无版权',
  paid: '需购买',
  ineligible: '不可播放',
  unknown: '未知',
}
const sourceNames = { qqmusic: 'QQ音乐', netease: '网易云音乐', bilibili: '哔哩哔哩', kugou: '酷狗音乐' }
const statusNames = { playing: '播放中', paused: '已暂停', stopped: '已停止' }

const title = computed(() => {
  const p = player.value
  if (!p) return '无歌曲'
  return [p.name, p.singer].filter(Boolean).join(' - ')
    || p.currentTrack?.metadata?.title
    || p.currentTrack?.name
    || '无歌曲'
})
const subtitle = computed(() => {
  const p = player.value
  if (!p) return '等待播放器状态'
  return [
    statusNames[p.status] || p.status || '未知状态',
    p.requester ? `点歌人：${p.requester}` : '',
    p.source ? sourceLabel(p.source) : '',
  ].filter(Boolean).join(' · ')
})
const statusText = computed(() => statusNames[player.value?.status] || player.value?.status || '未知')
const duration = computed(() => Number(player.value?.duration || 0))
const progressPercent = computed(() => {
  if (!duration.value) return 0
  return Math.max(0, Math.min(100, (Number(player.value?.progress || 0) / duration.value) * 100))
})

const leaderboardRangeLabel = computed(() => {
  if (leaderboardDays.value === 1) return '最近 24 小时'
  return leaderboardDays.value > 0 ? `最近 ${leaderboardDays.value} 天` : '全部'
})

watch(player, (value) => {
  if (!value) return
  if (!dragging.value) seekPercent.value = Math.round(progressPercent.value * 10)
  if (value.volume != null && document.activeElement?.tagName !== 'INPUT') volumeDraft.value = Number(value.volume)
}, { deep: true })

function sourceLabel(value) {
  return sourceNames[value] || value || '未知平台'
}
function timeText(value) {
  const total = Math.max(0, Math.floor(Number(value) || 0))
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, '0')}`
}
function formatDate(ms) {
  if (!ms) return '未知'
  const date = new Date(Number(ms))
  return Number.isNaN(date.getTime()) ? '未知' : date.toLocaleString('zh-CN', { hour12: false })
}
function formatBytes(value) {
  let number = Number(value)
  if (!Number.isFinite(number) || number < 0) return '未知'
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB']
  let index = 0
  while (number >= 1024 && index < units.length - 1) { number /= 1024; index += 1 }
  return `${index === 0 ? Math.round(number) : number.toFixed(number >= 10 ? 1 : 2)} ${units[index]}`
}
function queueMeta(item) {
  return [item.requester || item.friendUsername, sourceLabel(item.source), item.preferAccompaniment ? '伴奏' : '']
    .filter(Boolean).join(' / ') || '普通点歌'
}
function cacheTitle(track) {
  return [track.title, Array.isArray(track.artists) ? track.artists.join('/') : track.artists].filter(Boolean).join(' - ')
    || '未知缓存歌曲'
}
function historyTitle(item) {
  return [item.title, item.artist].filter(Boolean).join(' - ') || item.keyword || '未知歌曲'
}
function candidateIdentity(item) {
  const key = item.trackRef?.key || {}
  return key.provider && key.id ? `${key.provider}/${key.id}` : '未知曲目'
}
function candidateKey(item, index) {
  const key = item.trackRef?.key || {}
  return key.provider && key.id ? `${key.provider}/${key.id}` : `${item.text || 'candidate'}-${index}`
}

let lastReport = { at: 0, text: '' }
function report(error) {
  if (handleAuth(error)) return
  const text = `错误：${error?.message || error}`
  const now = Date.now()
  if (text === lastReport.text && now - lastReport.at < 30000) return
  lastReport = { at: now, text }
  actionText.value = text
  toast(text, 'err')
}
function setBusy(value) {
  busy.value = value
}
async function action(path) {
  if (busy.value) return false
  setBusy(true)
  actionText.value = '请求中…'
  try {
    const result = await requestJson(path, {})
    actionText.value = typeof result === 'string' ? result : (result?.message || '操作成功')
    await refreshAll()
    return true
  } catch (error) {
    report(error)
    return false
  } finally {
    setBusy(false)
  }
}
let playerRefreshing = false
async function refreshPlayer() {
  if (playerRefreshing) return
  playerRefreshing = true
  try {
    const value = await getJson('/status')
    player.value = value
    state.player = value
    lastUpdated.value = new Date().toLocaleTimeString('zh-CN', { hour12: false })
  } catch (error) {
    report(error)
  } finally {
    playerRefreshing = false
  }
}

async function loadStatus() {
  try {
    await refreshStoreStatus(true)
    const value = storeState.player
    player.value = value
    state.player = value
    lastUpdated.value = new Date().toLocaleTimeString()
  } catch (error) {
    report(error)
  }
}
async function loadQueue() {
  try {
    const value = await getJson('/queue')
    queue.value = Array.isArray(value)
      ? value
      : (Array.isArray(value?.queue) ? value.queue : (Array.isArray(value?.items) ? value.items : []))
  } catch (error) {
    report(error)
  }
}
async function loadPlayMode() {
  try {
    const value = await getJson('/playback/mode')
    if (value?.mode) playMode.value = value.mode
  } catch (error) {
    report(error)
  }
}
async function loadInsights() {
  try {
    const value = await getJson('/playback/insights')
    cache.value = value?.cache || null
    history.value = Array.isArray(value?.history) ? value.history : []
  } catch (error) {
    report(error)
  }
}
async function loadCacheTracks() {
  try {
    const params = new URLSearchParams({
      offset: String(cacheOffset.value),
      limit: String(cacheLimit),
      sort: cacheSort.value.split(':')[0] || 'last_used',
      order: cacheSort.value.split(':')[1] || 'desc',
    })
    const value = await getJson(`/playback/cache/tracks?${params}`)
    cacheTotal.value = Number(value?.total || 0)
    const tracks = Array.isArray(value?.tracks) ? value.tracks : []
    if (cacheTotal.value > 0 && cacheOffset.value >= cacheTotal.value) {
      cacheOffset.value = Math.max(0, Math.floor((cacheTotal.value - 1) / cacheLimit) * cacheLimit)
      return loadCacheTracks()
    }
    cacheTracks.value = tracks
  } catch (error) {
    report(error)
  }
}
async function loadLeaderboard() {
  try {
    const suffix = leaderboardDays.value > 0 ? `?days=${encodeURIComponent(leaderboardDays.value)}` : ''
    const value = await getJson(`/playback/leaderboard${suffix}`)
    leaderboard.value = Array.isArray(value?.entries) ? value.entries : []
  } catch (error) {
    report(error)
  }
}
async function refreshAll() {
  if (refreshing) return
  refreshing = true
  try {
    await Promise.allSettled([
      loadStatus(),
      loadQueue(),
      loadInsights(),
      loadPlayMode(),
      loadCacheTracks(),
    ])
  } finally {
    refreshing = false
  }
}
async function setPlayMode(mode) {
  if (busy.value) return
  setBusy(true)
  actionText.value = '切换播放模式…'
  try {
    await requestJson(`/playback/mode?mode=${encodeURIComponent(mode)}`, {})
    playMode.value = mode
    actionText.value = `播放模式：${playModes.find((item) => item.value === mode)?.label || mode}`
    await loadPlayMode()
  } catch (error) {
    report(error)
  } finally {
    setBusy(false)
  }
}
async function seek() {
  if (!duration.value || busy.value) return
  const position = duration.value * Number(seekPercent.value) / 100
  setBusy(true)
  try {
    await requestJson(`/playback/seek?position=${position.toFixed(1)}`, {})
    actionText.value = `已定位到 ${timeText(position)}`
    await loadStatus()
  } catch (error) {
    report(error)
  } finally {
    dragging.value = false
    setBusy(false)
  }
}
async function applyVolume() {
  if (busy.value) return
  const value = Math.max(0, Math.min(100, Math.round(Number(volumeDraft.value) || 0)))
  volumeDraft.value = value
  setBusy(true)
  try {
    await requestJson(`/volume?volume=${encodeURIComponent(value)}`, {})
    actionText.value = `音量：${value}%`
    await loadStatus()
  } catch (error) {
    report(error)
  } finally {
    setBusy(false)
  }
}
function querySuffix() {
  const params = new URLSearchParams({ keyword: keyword.value })
  if (source.value) params.set('source', source.value)
  if (preferAccompaniment.value) params.set('preferAccompaniment', '1')
  return params.toString()
}
async function requestSong(ai) {
  if (!keyword.value || busy.value) return
  setBusy(true)
  actionText.value = ai ? 'AI 点歌中…' : '点歌中…'
  try {
    const result = await requestJson(`${ai ? '/ai/search' : '/searchPlay'}?${querySuffix()}`, {})
    actionText.value = typeof result === 'string' ? result : (result?.message || '点歌请求已提交')
  } catch (error) {
    report(error)
  } finally {
    setBusy(false)
  }
}
async function requestSourceSong() {
  if (!keyword.value || !source.value || busy.value) return
  setBusy(true)
  actionText.value = '指定音源点歌中…'
  try {
    const params = new URLSearchParams({ keyword: keyword.value, source: source.value })
    if (preferAccompaniment.value) params.set('preferAccompaniment', '1')
    const result = await requestJson(`/searchSource?${params}`, {})
    actionText.value = typeof result === 'string' ? result : (result?.message || '指定音源点歌已提交')
  } catch (error) {
    report(error)
  } finally {
    setBusy(false)
  }
}
async function searchRaw() {
  if (!keyword.value || busy.value) return
  setBusy(true)
  rawSearchResult.value = '搜索中…'
  try {
    const params = new URLSearchParams({ keyword: keyword.value })
    if (source.value) params.set('source', source.value)
    const result = await getJson(`/search?${params}`)
    rawSearchResult.value = typeof result === 'string' ? result : JSON.stringify(result, null, 2)
  } catch (error) {
    rawSearchResult.value = ''
    report(error)
  } finally {
    setBusy(false)
  }
}
async function enqueue() {
  if (!keyword.value || busy.value) return
  setBusy(true)
  actionText.value = '加入队列中…'
  try {
    const params = new URLSearchParams({ keyword: keyword.value })
    if (source.value) params.set('source', source.value)
    if (preferAccompaniment.value) params.set('preferAccompaniment', '1')
    const result = await requestJson(`/queue/add?${params}`, {})
    actionText.value = typeof result === 'string' ? result : (result?.message || '已加入队列')
    await loadQueue()
  } catch (error) {
    report(error)
  } finally {
    setBusy(false)
  }
}
async function searchCandidates() {
  if (!keyword.value || searching.value) return
  const version = ++searchVersion.value
  searching.value = true
  candidateHint.value = '搜索中…'
  try {
    const params = new URLSearchParams({ keyword: keyword.value })
    if (source.value) params.set('source', source.value)
    const value = await getJson(`/search/candidates?${params}`)
    if (version !== searchVersion.value) return
    candidates.value = Array.isArray(value) ? value : []
    candidateHint.value = candidates.value.length ? `找到 ${candidates.value.length} 个候选` : '没有找到候选'
  } catch (error) {
    if (version === searchVersion.value) {
      candidates.value = []
      candidateHint.value = '搜索失败'
      report(error)
    }
  } finally {
    searching.value = false
  }
}
async function playCandidate(item) {
  if (busy.value || !item?.trackRef || !item?.metadata) return
  setBusy(true)
  actionText.value = '播放候选中…'
  try {
    await requestJson('/player/play-track', { trackRef: item.trackRef, metadata: item.metadata })
    actionText.value = `开始播放：${item.text || '候选曲目'}`
    await loadStatus()
  } catch (error) {
    report(error)
  } finally {
    setBusy(false)
  }
}
async function testCandidate(item) {
  if (busy.value || !item?.text) return
  setBusy(true)
  actionText.value = '测试点歌中…'
  try {
    const params = new URLSearchParams({ keyword: item.text })
    const provider = item.trackRef?.key?.provider
    if (provider) params.set('source', provider)
    await requestJson(`/searchPlay?${params}`, {})
    actionText.value = `已测试点歌：${item.text}`
  } catch (error) {
    report(error)
  } finally {
    setBusy(false)
  }
}
async function removeQueue(item) {
  if (busy.value || item?.id == null) return
  if (!window.confirm(`从待播队列删除「${item.keyword || item.id}」？`)) return
  setBusy(true)
  try {
    await requestJson(`/queue/remove?id=${encodeURIComponent(item.id)}`, {})
    actionText.value = `已删除队列歌曲：${item.keyword || item.id}`
    await loadQueue()
  } catch (error) {
    report(error)
  } finally {
    setBusy(false)
  }
}
async function clearQueue() {
  if (busy.value || !queue.value.length) return
  if (!window.confirm('确定清空全部待播歌曲？')) return
  await action('/queue/clear')
}
function resetCachePage() {
  cacheOffset.value = 0
  loadCacheTracks()
}
function changeCachePage(direction) {
  cacheOffset.value = Math.max(0, cacheOffset.value + direction * cacheLimit)
  loadCacheTracks()
}
async function playCached(track) {
  if (busy.value || !track?.source || !track?.id) return
  setBusy(true)
  try {
    await requestJson('/player/play-track', {
      trackRef: { key: { provider: track.source, id: track.id } },
      metadata: { title: track.title || '缓存歌曲', artists: Array.isArray(track.artists) ? track.artists : [] },
    })
    actionText.value = `播放缓存：${cacheTitle(track)}`
    await loadStatus()
  } catch (error) {
    report(error)
  } finally {
    setBusy(false)
  }
}
async function invalidateCached(track) {
  if (busy.value || !track?.source || !track?.id) return
  if (!window.confirm(`仅清除「${cacheTitle(track)}」的磁盘缓存？曲目会保留在池中。`)) return
  await cacheAction(`/playback/cache/invalidate?provider=${encodeURIComponent(track.source)}&id=${encodeURIComponent(track.id)}`)
}
async function deleteCached(track) {
  if (busy.value || !track?.source || !track?.id) return
  if (!window.confirm(`删除歌曲「${cacheTitle(track)}」？会从歌曲池移除并删除缓存。`)) return
  await cacheAction(`/playback/song/delete?provider=${encodeURIComponent(track.source)}&id=${encodeURIComponent(track.id)}`)
}
async function resetTrackStats(track) {
  if (busy.value || !track?.source || !track?.id) return
  if (!window.confirm(`清零「${cacheTitle(track)}」的播放统计？缓存与元数据保留。`)) return
  await cacheAction(`/playback/statistics/reset?provider=${encodeURIComponent(track.source)}&id=${encodeURIComponent(track.id)}`)
}
async function cacheAction(path) {
  setBusy(true)
  try {
    await requestJson(path, {})
    actionText.value = '缓存操作完成'
    await Promise.allSettled([loadInsights(), loadCacheTracks()])
  } catch (error) {
    report(error)
  } finally {
    setBusy(false)
  }
}

let playerTimer = null

onMounted(() => {
  refreshAll()
  loadLeaderboard()
  playerTimer = setInterval(refreshPlayer, 1000)
  timer = setInterval(refreshAll, 5000)
})
onBeforeUnmount(() => {
  clearInterval(timer)
  clearInterval(playerTimer)
})
</script>

<style scoped>
/* =============================================================================
 * 播放与点歌：macOS 数据密集面板
 *
 * 只作用于本页（scoped）与 `.playback-mac-*` 前缀，不改 components.css 的共享规则。
 * 要点：
 *   - 队列 / 缓存 / 候选 / 历史都收进「一圈圆角 + 1px 极淡横线」的 grouped 容器，
 *     不再是每项一张独立小卡（macOS 用 grouped list，不用卡片墙）。
 *   - 历史与点歌榜是日志块：深色圆角面板（--bg-0）+ 等宽字体 + 12px 内边距。
 *   - 统计卡强调「标签小、数值大」：标签 11px 灰、数值 15px 半粗等宽数字。
 * ========================================================================== */

/* ---------- 统计：标签小、数值大 ---------- */

.playback-mac-stats {
  gap: var(--sp-3) var(--sp-4);
  padding: var(--sp-3);
  border: 1px solid var(--sep-2);
  border-radius: var(--radius-m);
  background: var(--bg-input);
}

.playback-mac-stats .login-fact span {
  color: var(--text-3);
  font-size: var(--fs-tiny);
  line-height: var(--lh-tiny);
  letter-spacing: 0.04em;
}

.playback-mac-stats .login-fact strong {
  margin-top: 2px;
  color: var(--text-1);
  font-size: 15px;
  line-height: 22px;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
}

.playback-mac-stats .login-fact strong small {
  color: var(--text-3);
  font-size: var(--fs-meta);
  font-weight: 500;
}

/* ---------- grouped 列表：队列 / 缓存 / 候选 ---------- */

.playback-mac-list,
.playback-mac-cache-list,
.playback-mac-candidates {
  display: flex;
  flex-direction: column;
  gap: 0;
  border: 1px solid var(--sep-2);
  border-radius: var(--radius-m);
  background: var(--bg-input);
  padding: 2px;
  overflow: auto;
}

/* 队列恢复单列：grouped 列表不该是双列卡片墙（窄屏原本就是单列）。 */
.playback-queue-panel .playback-mac-list {
  grid-template-columns: none;
  display: flex;
}

.playback-mac-list .queue-item,
.playback-mac-cache-list .cache-track-item,
.playback-mac-candidates .candidate-item {
  min-height: 32px;
  background: transparent;
  border: 0;
  border-radius: var(--radius-s);
  padding: 5px var(--sp-2);
}

.playback-mac-list .queue-item + .queue-item,
.playback-mac-cache-list .cache-track-item + .cache-track-item,
.playback-mac-candidates .candidate-item + .candidate-item {
  border-top: 1px solid var(--sep-2);
  border-top-left-radius: 0;
  border-top-right-radius: 0;
}

.playback-mac-list .queue-item:hover,
.playback-mac-cache-list .cache-track-item:hover,
.playback-mac-candidates .candidate-item:hover {
  background: var(--fill-1);
}

.playback-mac-list .queue-item .queue-title,
.playback-mac-candidates .candidate-item .queue-title {
  font-size: var(--fs-item);
  line-height: var(--lh-item);
  color: var(--text-1);
}

.playback-mac-list .queue-index {
  color: var(--text-3);
  font-variant-numeric: tabular-nums;
}

.playback-mac-cache-list {
  max-height: 360px;
}

/* 缓存行：左侧信息 + 右侧元信息，按钮只在 hover / 聚焦时显形，
 * 免得每一行都挂着四个按钮把列表读成工具栏。 */
.playback-mac-cache-list .cache-track-item {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 320px);
  gap: var(--sp-3);
  align-items: center;
  padding: 6px var(--sp-3);
}

.playback-mac-list-main {
  min-width: 0;
}

/* 缓存行操作常驻（这些是操作主入口，不能靠 hover 才出现），
 * 但降一档视觉权重：ghost 按钮本身已经是 --text-2。 */
.playback-mac-track-actions {
  display: flex;
  gap: var(--sp-1);
  flex-wrap: wrap;
  margin-top: 2px;
}

.playback-mac-cache-list .cache-track-meta {
  color: var(--text-3);
  font-size: var(--fs-tiny);
  line-height: 1.5;
  word-break: break-word;
}

@media (max-width: 900px) {
  .playback-mac-cache-list .cache-track-item {
    grid-template-columns: minmax(0, 1fr);
  }
}

/* ---------- 历史 / 点歌榜：深色日志块 ---------- */

.playback-mac-log {
  gap: 0;
  padding: 2px;
  border: 1px solid var(--sep-2);
  border-radius: var(--radius-m);
  background: var(--bg-0);
  max-height: 360px;
  overflow: auto;
}

.playback-mac-log .history-item {
  border: 0;
  border-left: 0;
  border-radius: 0;
  background: transparent;
  padding: 6px var(--sp-3);
  transition: background 0.12s ease;
}

.playback-mac-log .history-item + .history-item {
  border-top: 1px solid var(--sep-2);
}

.playback-mac-log .history-item:hover {
  background: var(--fill-1);
}

.playback-mac-log-head {
  display: flex;
  align-items: baseline;
  gap: var(--sp-2);
  flex-wrap: wrap;
}

.playback-mac-log .history-time {
  color: var(--text-3);
  font-family: var(--font-mono);
  font-size: var(--fs-tiny);
  font-variant-numeric: tabular-nums;
}

.playback-mac-log .history-item b {
  font-size: var(--fs-item);
  font-weight: 600;
  color: var(--text-1);
}

.playback-mac-log .history-result {
  margin-top: 2px;
  color: var(--text-2);
  font-family: var(--font-mono);
  font-size: var(--fs-tiny);
  line-height: 1.55;
  word-break: break-word;
}

.playback-mac-count {
  color: var(--text-3);
  font-size: var(--fs-tiny);
  font-variant-numeric: tabular-nums;
}

/* ---------- 空状态：居中、灰色、小字 ---------- */

.playback-mac-list .empty-hint,
.playback-mac-cache-list .empty-hint,
.playback-mac-candidates .empty-hint,
.playback-mac-log .empty-hint {
  padding: 20px 12px;
  color: var(--text-3);
  font-size: var(--fs-meta);
  line-height: var(--lh-meta);
  text-align: center;
}

/* ---------- 正在播放：标题与进度条的层级 ---------- */

.current-song {
  font-size: 17px;
  line-height: 23px;
  font-weight: 600;
  letter-spacing: -0.01em;
}

.now-playing-sub {
  color: var(--text-2);
  font-size: var(--fs-meta);
}

.playback-progress {
  margin-top: var(--sp-3);
}

.playback-progress .progress-row {
  font-size: var(--fs-tiny);
  font-variant-numeric: tabular-nums;
}
</style>
