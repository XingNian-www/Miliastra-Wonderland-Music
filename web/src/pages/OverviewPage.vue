<template>
  <h1 class="page-title">总览</h1>
  <p class="page-subtitle">实时状态、正在播放与最近命令</p>
  <div v-if="state.health === null" class="loading-card">正在连接后端…</div>
  <div class="page-grid overview-layout">
    <div class="panel overview-now-playing">
      <div class="panel-head"><h2>正在播放</h2><small>{{ player?.status || '未知' }}</small></div>
      <div class="panel-body">
        <div class="now-playing-title">{{ track?.name || track?.metadata?.title || '无歌曲' }}</div>
        <div class="config-field-hint">{{ artists }}{{ track?.albumName ? ' · ' + track.albumName : '' }}</div>
        <div class="now-playing-progress">
          <div class="progress-track"><div class="progress-fill" :style="{ width: percent + '%' }"></div></div>
          <div class="config-field-hint">{{ timeText(progress) }} / {{ timeText(duration) }}</div>
        </div>
        <div class="config-field-hint">点歌人：{{ player?.requester || '—' }}</div>
        <div style="margin-top:12px;display:flex;gap:8px;flex-wrap:wrap">
          <button class="btn small" @click="refresh">刷新</button>
          <button class="btn small ghost" @click="go('playback')">播放控制</button>
        </div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-head"><h2>值守状态</h2></div>
      <div class="panel-body">
        <div class="overview-stat"><span>聊天监听</span><b>{{ listenerMode }}</b></div>
        <div class="overview-stat"><span>大厅</span><b>{{ listener?.temporaryPrimary ? '临时一级' : listener?.hallRoundRequired ? '需要轮询' : '正常' }}</b></div>
        <div class="overview-stat"><span>未读任务</span><b>{{ listener?.unreadTaskPending ? '有挂起' : '无' }}</b></div>
        <div class="overview-stat"><span>最近命令</span><b>{{ commands.length }} 条</b></div>
        <div class="config-field-hint" style="margin-top:8px">最近一次：{{ commands[commands.length - 1] || '—' }}</div>
      </div>
    </div>

    <div class="panel overview-quick">
      <div class="panel-head"><h2>快捷入口</h2></div>
      <div class="panel-body" style="display:flex;flex-wrap:wrap;gap:8px">
        <button v-for="item in quick" :key="item.route" class="btn" @click="go(item.route)">{{ item.label }}</button>
      </div>
    </div>
  </div>
</template>

<script setup>
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { getJson } from '../api.js'
import { navigateTo } from '../router.js'
import { state } from '../store.js'

const player = ref(null)
const monitor = ref(null)
const track = computed(() => player.value?.currentTrack || null)
const progress = computed(() => Number(player.value?.progress || 0))
const duration = computed(() => Number(player.value?.duration || 0))
const percent = computed(() => (duration.value > 0 ? Math.min(100, (progress.value / duration.value) * 100) : 0))
const artists = computed(() => {
  const list = track.value?.metadata?.artists || []
  return Array.isArray(list) && list.length ? list.join(' / ') : (player.value?.singer || '')
})
const listener = computed(() => monitor.value?.chatListener || null)
const listenerMode = computed(() => listener.value?.mode || '—')
const commands = computed(() => (monitor.value?.commands || []).filter(Boolean))

const quick = [
  { route: 'playback', label: '播放与点歌' },
  { route: 'game', label: '游戏与聊天' },
  { route: 'aiconfig', label: 'AI 配置' },
  { route: 'config', label: '配置中心' },
  { route: 'tasks', label: '任务与日志' },
]

function timeText(seconds) {
  const total = Math.max(0, Math.floor(Number(seconds) || 0))
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, '0')}`
}
function go(route) {
  navigateTo(route)
}
async function refresh() {
  try {
    const [status, mon] = await Promise.allSettled([getJson('/status'), getJson('/monitor')])
    if (status.status === 'fulfilled') player.value = status.value
    if (mon.status === 'fulfilled') monitor.value = mon.value
  } catch {}
}

let timer = null
onMounted(() => {
  refresh()
  timer = setInterval(refresh, 10000)
})
onBeforeUnmount(() => clearInterval(timer))
</script>
