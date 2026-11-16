<template>
  <nav class="app-nav" aria-label="功能导航">
    <div class="nav-brand">Miliastra<span>远程监控</span></div>
    <div class="nav-links">
      <button
        v-for="route in routes"
        :key="route.key"
        class="nav-link"
        :class="{ active: currentRoute === route.key }"
        @click="navigateTo(route.key)"
      >
        {{ route.label }}
      </button>
    </div>
  </nav>
  <div class="shell">
    <header class="topbar">
      <div class="brand"><h1>Miliastra <span>远程监控</span></h1></div>
      <div class="status-chips">
        <span class="chip"><i class="dot"></i><span>服务</span><em>{{ serviceState }}</em></span>
        <span class="chip"><i class="dot"></i><span>播放</span><em>{{ playerState }}</em></span>
        <span class="chip"><i class="dot"></i><span>队列</span><em>{{ queueState }}</em></span>
        <span class="chip"><i class="dot"></i><span>监听</span><em>一级值守</em></span>
        <span class="chip"><i class="dot"></i><span>大厅</span><em>—</em></span>
      </div>
      <div class="top-actions">
        <span class="last-refresh">{{ state.lastRefresh || '等待刷新' }}</span>
        <span class="sep"></span>
        <button class="btn theme-toggle" title="切换日间 / 夜间模式" @click="toggleTheme">◐</button>
        <button class="btn" @click="toggleRefresh">{{ state.autoRefresh ? '暂停刷新' : '继续刷新' }}</button>
      </div>
    </header>

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
import { ROUTES, applyRoute, currentRoute, navigateTo, routeFromHash } from './router.js'
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

const serviceState = computed(() => (state.health ? '运行中' : '未知'))
const playerState = computed(() => {
  const track = state.player?.currentTrack?.metadata?.title
  return track || (state.player ? '空闲' : '未知')
})
const queueState = computed(() => {
  const queue = state.player?.queue
  if (Array.isArray(queue)) return `${queue.length} 首`
  return '—'
})

let timer = null
function tick() {
  if (!state.autoRefresh) return
  loadStatus()
}
function toggleRefresh() {
  state.autoRefresh = !state.autoRefresh
}
function toggleTheme() {
  document.body.classList.toggle('light')
  localStorage.setItem('miliastraTheme', document.body.classList.contains('light') ? 'light' : 'dark')
}

function onHashChange() {
  applyRoute(routeFromHash())
}

onMounted(() => {
  if (localStorage.getItem('miliastraTheme') === 'light') document.body.classList.add('light')
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
