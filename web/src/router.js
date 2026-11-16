import { ref } from 'vue'

export const ROUTES = [
  { key: 'overview', label: '总览' },
  { key: 'playback', label: '播放与点歌' },
  { key: 'login', label: '账号登录' },
  { key: 'game', label: '游戏与聊天' },
  { key: 'entertainment', label: '娱乐玩法' },
  { key: 'commands', label: '命令面板' },
  { key: 'aiconfig', label: 'AI 配置', native: true },
  { key: 'tasks', label: '任务与日志' },
  { key: 'tools', label: '高级控制' },
  { key: 'visual', label: '视觉档案' },
  { key: 'debug', label: '调试沙盒' },
  { key: 'config', label: '配置中心', native: true },
  { key: 'identity', label: '身份映射', native: true },
  { key: 'settings', label: '设置', native: true },
]

export const currentRoute = ref('config')

export function routeFromHash() {
  const raw = String(location.hash || '').replace(/^#\/?/, '').split('?')[0]
  return ROUTES.some((route) => route.key === raw) ? raw : 'config'
}

export function applyRoute(route) {
  currentRoute.value = route
  const hash = `#${route}`
  if (location.hash !== hash) history.replaceState(null, '', hash)
}

export function navigateTo(route) {
  if (!ROUTES.some((item) => item.key === route)) return
  currentRoute.value = route
  if (location.hash !== `#${route}`) location.hash = `#${route}`
}
