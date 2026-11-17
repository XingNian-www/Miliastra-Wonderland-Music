import { ref } from 'vue'

export const ROUTES = [
  { key: 'overview', label: '总览', group: '监控', icon: '<rect x="3" y="3" width="7" height="9" rx="1.6"/><rect x="14" y="3" width="7" height="5" rx="1.6"/><rect x="14" y="12" width="7" height="9" rx="1.6"/><rect x="3" y="16" width="7" height="5" rx="1.6"/>' },
  { key: 'playback', label: '播放与点歌', group: '监控', icon: '<path d="M9 18V5l12-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="18" cy="16" r="3"/>' },
  { key: 'login', label: '账号登录', group: '监控', icon: '<rect x="3" y="5" width="18" height="14" rx="2"/><path d="M3 10h18"/><path d="M7 15h4"/>' },
  { key: 'game', label: '游戏与聊天', group: '游戏', icon: '<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>' },
  { key: 'entertainment', label: '娱乐玩法', group: '游戏', icon: '<circle cx="12" cy="12" r="9"/><path d="M8.5 14.5c1 1.4 2.2 2 3.5 2s2.5-.6 3.5-2"/><path d="M9 9.5h.01M15 9.5h.01"/>' },
  { key: 'commands', label: '命令面板', group: '游戏', icon: '<rect x="2.5" y="3.5" width="19" height="17" rx="2.5"/><path d="m6.5 8.5 3 3-3 3"/><path d="M12.5 15h4.5"/>' },
  { key: 'aiconfig', label: 'AI 配置', group: '智能', icon: '<path d="M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M5.6 18.4l2.1-2.1M16.3 7.7l2.1-2.1"/><circle cx="12" cy="12" r="3.5"/>' },
  { key: 'config', label: '配置中心', group: '系统', icon: '<path d="M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3"/><path d="M1 14h6M9 8h6M17 16h6"/>' },
  { key: 'visual', label: '视觉档案', group: '系统', icon: '<rect x="3" y="4" width="18" height="16" rx="2.5"/><circle cx="9" cy="10" r="2"/><path d="m4 18 5-4 3 2 4-4 4 4"/>' },
  { key: 'tools', label: '高级控制', group: '系统', icon: '<path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/>' },
  { key: 'debug', label: '调试沙盒', group: '系统', icon: '<path d="M8 9h8M8 13h5"/><path d="M6 3v3M18 3v3"/><rect x="4" y="6" width="16" height="15" rx="2"/><path d="M12 18h.01"/>' },
  { key: 'tasks', label: '任务与日志', group: '系统', icon: '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/><path d="M9 15l2 2 4-4"/>' },
  { key: 'identity', label: '身份映射', group: '系统', icon: '<rect x="3" y="4" width="18" height="16" rx="2.5"/><circle cx="9" cy="10" r="2.2"/><path d="M5.5 17c.6-2 2-3 3.5-3s2.9 1 3.5 3"/><path d="M15 9h4M15 13h4"/>' },
  { key: 'settings', label: '设置', group: '系统', icon: '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09a1.65 1.65 0 0 0-1-1.51 1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09a1.65 1.65 0 0 0 1.51-1 1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>' },
]

export const NAV_GROUPS = ['监控', '游戏', '智能', '系统']

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
