import { ref } from 'vue'
import { state } from './store.js'

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

/* ============================================================================
 * 未保存修改的离开保护
 *
 * 页面有三种方式让守卫知道「我有未保存修改」，按优先级：
 *   1. 主动注册：registerDirtySource(id, { label, dirty(), blocksNavigation? })。
 *      站内切页（导航按钮 / 前进后退 / 手改 hash）由 confirmLeave() 先 confirm；
 *      关标签刷新由 App.vue 的 beforeunload 读 hasUnsavedChanges()。
 *      页面自己缓存了草稿、切页不会丢时可以传 blocksNavigation: false：
 *      仍参与 beforeunload（刷新确实会丢），但不用假警告拦站内切页。
 *   2. 写全局 state.dirty：只有当前路由在 STORE_DIRTY_ROUTES 里才采用，
 *      避免别的页面留下的脏键把导航永久锁死（AI 配置页不需要改代码）。
 *   3. 只在组件里挂 beforeunload（身份映射）：confirmLeave 派发一次合成事件探底，
 *      见 pageProtectsUnload。
 * 注意：第 3 种只能覆盖 beforeunload 已在管的那部分状态；页面里没进 beforeunload
 * 判据的脏数据（例如身份页的 jsonDirty）仍然看不见，需要改那个页面。
 * ========================================================================== */
const dirtySources = new Map()

/**
 * 会把脏标记写进 store.state.dirty 的页面。
 * 只有当前路由在这个集合里时，才拿 state.dirty 当兜底判据——否则「AI 配置页改完
 * 没保存就离开」留下的脏键会让每一页都误报「有未保存修改」，把导航锁死。
 */
const STORE_DIRTY_ROUTES = new Set(['aiconfig', 'config'])

/** 注册一个页面级脏标记来源；返回注销函数（组件卸载时调用）。 */
export function registerDirtySource(id, source) {
  if (!id || !source || typeof source.dirty !== 'function') return () => {}
  dirtySources.set(id, source)
  return () => {
    if (dirtySources.get(id) === source) dirtySources.delete(id)
  }
}

/** 有未保存修改的来源列表：`[{ id, label, count, blocksNavigation }]`；空数组表示干净。 */
export function unsavedSources() {
  const sources = []
  for (const [id, source] of dirtySources) {
    let count = 0
    try {
      count = Math.max(0, Number(source.dirty()) || 0)
    } catch (error) {
      // 页面状态异常不应该把用户锁死在当前页：当作干净处理，交给该页面自己的提示。
      count = 0
    }
    if (count > 0) {
      sources.push({
        id,
        label: source.label || id,
        count,
        blocksNavigation: source.blocksNavigation !== false,
      })
    }
  }
  if (sources.length) return sources
  if (!STORE_DIRTY_ROUTES.has(currentRoute.value)) return []
  const stored = Object.keys(state.dirty).length
  if (!stored) return []
  const route = ROUTES.find((item) => item.key === currentRoute.value)
  return [{ id: 'store.dirty', label: route ? route.label : '当前页面', count: stored, blocksNavigation: true }]
}

export function hasUnsavedChanges() {
  return unsavedSources().length > 0
}

/**
 * 兜底探测：给「只在自己组件里注册 beforeunload」的页面（如身份映射）用。
 *
 * 不读 DOM、也不要求那些页面改代码：派发一个**合成的** beforeunload 事件，
 * 有脏数据的页面会在自己的处理器里 `preventDefault()`，于是 defaultPrevented 为真。
 * 合成事件不会弹出浏览器对话框，也不会真的卸载页面；处理器里只做 preventDefault。
 * 递归保护：App.vue 的全局 beforeunload 也会走到这里，用 probing 短路避免无限递归。
 */
let probing = false

function pageProtectsUnload() {
  if (probing) return false
  if (typeof window === 'undefined' || typeof window.dispatchEvent !== 'function') return false
  if (typeof Event !== 'function') return false
  probing = true
  try {
    const event = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(event)
    return event.defaultPrevented === true
  } catch (error) {
    return false
  } finally {
    probing = false
  }
}

/** 离开当前页前的确认；返回 true 表示可以走。 */
export function confirmLeave(nextRoute) {
  const all = unsavedSources()
  const sources = all.filter((source) => source.blocksNavigation)
  const target = ROUTES.find((item) => item.key === nextRoute)
  const where = target ? `「${target.label}」` : '其他页面'
  if (sources.length) {
    const labels = sources.map((source) => `「${source.label}」`).join('、')
    return window.confirm(`${labels}有未保存的修改，离开到${where}后可能丢失。\n\n确定离开？`)
  }
  // 没有任何注册来源时才探底：注册过（哪怕不拦切页）的页面自己的状态已经问过了，
  // 再去派发事件只会把「切页本来不会丢」的草稿也判成脏。
  if (all.length || !pageProtectsUnload()) return true
  const current = ROUTES.find((item) => item.key === currentRoute.value)
  const label = current ? `「${current.label}」` : '当前页面'
  return window.confirm(`${label}有未保存的修改，离开到${where}后可能丢失。\n\n确定离开？`)
}

export function routeFromHash() {
  const raw = String(location.hash || '').replace(/^#\/?/, '').split('?')[0]
  return ROUTES.some((route) => route.key === raw) ? raw : 'config'
}

export function applyRoute(route) {
  currentRoute.value = route
  const hash = `#${route}`
  if (location.hash !== hash) history.replaceState(null, '', hash)
}

/**
 * 程序内切页的唯一入口。
 * 目标页有未保存修改时先 confirm；取消则保持原页（返回 false）。
 * `options.force` 只给「确定要丢弃修改」的调用方用，常规页面不要传。
 */
export function navigateTo(route, options = {}) {
  if (!ROUTES.some((item) => item.key === route)) return false
  if (!options.force && route !== currentRoute.value && !confirmLeave(route)) return false
  currentRoute.value = route
  if (location.hash !== `#${route}`) location.hash = `#${route}`
  return true
}
