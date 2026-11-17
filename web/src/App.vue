<template>
  <nav class="app-nav" aria-label="功能导航">
    <div class="nav-brand">Miliastra<span>远程监控</span></div>
    <div class="nav-links" title="窄屏可左右滑动查看更多入口">
      <template v-for="group in navGroups" :key="group">
        <div class="nav-group">{{ group }}</div>
        <button
          v-for="route in routes.filter((item) => item.group === group)"
          :key="route.key"
          class="nav-link"
          :class="{ active: currentRoute === route.key }"
          @click="navigateTo(route.key)"
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" v-html="route.icon"></svg>
          {{ route.label }}
        </button>
      </template>
    </div>
  </nav>
  <div class="shell">
    <header class="topbar">
      <div class="brand" style="display:flex;align-items:baseline">
        <h1>Miliastra <span>远程监控</span></h1>
        <span class="page-crumb">{{ currentLabel }}</span>
      </div>
      <div class="status-chips">
        <span class="chip" :data-state="serviceTone"><i class="dot"></i><span>服务</span><em>{{ serviceState }}</em></span>
        <span class="chip" :data-state="playerTone"><i class="dot"></i><span>播放</span><em>{{ playerState }}</em></span>
        <span class="chip"><i class="dot"></i><span>队列</span><em>{{ queueState }}</em></span>
        <span class="chip" :data-state="listenerTone"><i class="dot"></i><span>监听</span><em>{{ listenerMode }}</em></span>
        <span class="chip" :data-state="hallTone"><i class="dot"></i><span>大厅</span><em>{{ hallState }}</em></span>
      </div>
      <div class="top-actions">
        <span class="last-refresh">{{ state.lastRefresh || '等待刷新' }}</span>
        <span class="sep"></span>
        <button class="btn theme-toggle" title="切换日间 / 夜间模式" @click="toggleTheme">
          <svg class="moon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/></svg>
          <svg class="sun" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M4.93 19.07l1.41-1.41M17.66 6.34l1.41-1.41"/></svg>
        </button>
        <button class="btn" @click="toggleRefresh">{{ state.autoRefresh ? '暂停刷新' : '继续刷新' }}</button>
      </div>
    </header>

    <div v-if="state.needsToken" class="token-banner" role="alert">
      <span>访问令牌无效或缺失，接口暂时不可用。</span>
      <button class="btn small" @click="navigateTo('settings')">前往设置页填入令牌</button>
    </div>

    <div v-if="state.message.text" class="config-message" :class="state.message.kind" style="margin: 8px 24px 0">
      {{ state.message.text }}
    </div>

    <main id="pageHost" class="page-host">
      <section class="page-section active">
        <component :is="pageComponent" :key="currentRoute" />
      </section>
    </main>
  </div>
</template>

<script setup>
import { computed, onBeforeUnmount, onMounted } from 'vue'
import { NAV_GROUPS, ROUTES, applyRoute, currentRoute, navigateTo, routeFromHash } from './router.js'
import { loadStatus, state } from './store.js'
import ConfigPage from './pages/ConfigPage.vue'
import AiConfigPage from './pages/AiConfigPage.vue'
import SettingsPage from './pages/SettingsPage.vue'
import IdentityPage from './pages/IdentityPage.vue'
import OverviewPage from './pages/OverviewPage.vue'
import PlaybackPage from './pages/PlaybackPage.vue'
import LoginPage from './pages/LoginPage.vue'
import GamePage from './pages/GamePage.vue'
import EntertainmentPage from './pages/EntertainmentPage.vue'
import CommandsPage from './pages/CommandsPage.vue'
import TasksPage from './pages/TasksPage.vue'
import ToolsPage from './pages/ToolsPage.vue'
import VisualPage from './pages/VisualPage.vue'
import DebugPage from './pages/DebugPage.vue'

const routes = ROUTES
const navGroups = NAV_GROUPS
const currentLabel = computed(() => ROUTES.find((item) => item.key === currentRoute.value)?.label || '')
const serviceTone = computed(() => (state.needsToken ? 'err' : state.connectionError ? 'err' : state.health ? 'ok' : 'warn'))
const playerTone = computed(() => (state.player?.status === 'playing' ? 'ok' : state.player ? 'warn' : ''))
const listenerTone = computed(() => (state.monitor ? 'ok' : ''))
const hallTone = computed(() => { const listener = state.monitor?.chatListener; if (!listener) return ''; return listener.unreadTaskPending || listener.hallRoundRequired ? 'warn' : 'ok' })
const pageComponent = computed(() => {
  switch (currentRoute.value) {
    case 'config': return ConfigPage
    case 'aiconfig': return AiConfigPage
    case 'settings': return SettingsPage
    case 'identity': return IdentityPage
    case 'overview': return OverviewPage
    case 'playback': return PlaybackPage
    case 'login': return LoginPage
    case 'game': return GamePage
    case 'entertainment': return EntertainmentPage
    case 'commands': return CommandsPage
    case 'tasks': return TasksPage
    case 'tools': return ToolsPage
    case 'visual': return VisualPage
    case 'debug': return DebugPage
    default: return OverviewPage
  }
})

const serviceState = computed(() => {
  if (state.needsToken) return '未授权'
  if (state.connectionError) return '离线'
  return state.health ? '运行中' : '未知'
})
const playerState = computed(() => {
  const track = state.player?.currentTrack?.metadata?.title
  return track || (state.player ? '空闲' : '未知')
})
const queueState = computed(() => {
  const queue = state.monitor?.queue
  if (Array.isArray(queue)) return `${queue.length} 首`
  return '—'
})
const listenerMode = computed(() => state.monitor?.chatListener?.mode || '—')
const hallState = computed(() => {
  const listener = state.monitor?.chatListener
  if (!listener) return '—'
  if (listener.unreadTaskPending) return '有挂起任务'
  return listener.hallRoundRequired ? '需要轮询' : '正常'
})

let timer = null
function tick() {
  if (!state.autoRefresh || state.needsToken) return
  loadStatus()
}
function toggleRefresh() {
  state.autoRefresh = !state.autoRefresh
}
function applyTheme(theme) {
  const isLight = theme === 'light'
  document.documentElement.dataset.theme = isLight ? 'light' : 'dark'
  document.body.classList.toggle('light', isLight)
  localStorage.setItem('miliastraTheme', isLight ? 'light' : 'dark')
}

function toggleTheme() {
  applyTheme(document.documentElement.dataset.theme === 'light' ? 'dark' : 'light')
}

function onHashChange() {
  applyRoute(routeFromHash())
}

onMounted(() => {
  applyTheme(localStorage.getItem('miliastraTheme') === 'light' ? 'light' : 'dark')
  applyRoute(routeFromHash())
  window.addEventListener('hashchange', onHashChange)
  tick()
  timer = setInterval(tick, 15000)
})
onBeforeUnmount(() => {
  window.removeEventListener('hashchange', onHashChange)
  clearInterval(timer)
})
</script>
