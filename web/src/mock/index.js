// 面板假数据：把「路径 + 方法」投影成一份与真实后端同形的响应。
//
// 为什么需要这一层：`api.js` 是所有请求的唯一出网口，它只认
// `request/requestJson/getJson/fetchBlob/fetchRaw` 这五个函数。假数据只要在
// 这一个出口上替换 `fetch`，页面代码就完全不知道自己在跟假后端说话——
// 不能在任何页面里写 `if (mock)`，否则截出来的页面与真实路径不是同一份代码。
//
// 形状来源见 routes.js 每个工厂上方的注释；`/config`、`/config/schema` 来自
// scripts/panel-mock-gen.py 生成的 fixtures.config.json。

import fixtures from './fixtures.config.js'
import * as demo from './routes.js'

/** 假数据的固定时间基准：让「最近刷新时间」「历史时间」每次都一样，截图可比。 */
export const MOCK_NOW = 1795000000000

/** 参与假数据的路由清单，供入口脚本与单测断言「面板调用的路由都被覆盖」。 */
export const MOCK_ROUTES = Object.freeze({
  json: [
    '/health',
    '/status',
    '/monitor',
    '/state',
    '/history',
    '/queue',
    '/config',
    '/config/schema',
    '/config/section',
    '/config/revisions',
    '/config/visual',
    '/player/providers',
    '/player/login/status',
    '/player/kugou/status',
    '/player/kugou/phone/code',
    '/player/kugou/phone/login',
    '/player/account/status',
    '/playback/insights',
    '/playback/leaderboard',
    '/playback/cache/tracks',
    '/playback/mode',
    '/ai/models',
    '/ai/vision/status',
    '/ai/trace',
    '/ai/trace/entry',
    '/ai/memory/actors',
    '/ai/memory/hall',
    '/ai/memory/actor',
    '/ai/chat/preview',
    '/tts/models',
    '/emoji/catalog',
    '/emoji/scanned',
    '/idiom-chain',
    '/number-bomb',
    '/card-game',
    '/commands/catalog',
    '/commands/replies',
    '/debug/read-only/mode',
    '/debug/read-only/summary',
    '/debug/read-only/entries',
    '/debug/state',
    '/debug/tasks',
    '/tools/task',
    '/tools/templates',
    '/search/candidates',
    '/operator/workflows',
    // 一次性诊断入口（真实后端统一回任务回执）。
    '/tools/ocr',
    '/tools/scan-chat',
    '/tools/ui-state',
    '/tools/hall-name',
    '/tools/ocr-backends',
    '/tools/capture-benchmark',
    '/tools/chat-change-samples',
    '/tools/panel-benchmark',
    '/tools/ai-preview',
    '/tools/template',
    '/tools/click',
    '/tools/key',
    '/debug/ui-catalogue',
    '/debug/command',
    '/debug/mode',
    '/debug/tasks',
    '/debug/runs',
    '/debug/window',
    '/debug/read-only/read',
    '/debug/read-only/evidence',
    '/debug/run',
    '/debug/run/all',
    '/debug/run/step',
    '/debug/run/cancel',
    '/debug/run/status',
    '/debug/run/evidence',
    '/debug/run/replay',
    '/debug/simulate-chat',
    '/debug/ui-state',
    '/playback/seek',
    '/playback/cache/invalidate',
    '/playback/song/delete',
    '/playback/statistics/reset',
    '/startup/game',
    '/startup/wonderland',
    '/startup/enter-wonderland',
  ],
  text: ['/search', '/debug/events'],
})

/**
 * 假截图：320x180 的纯色 PNG（461 字节，内联 base64）。
 *
 * 为什么必须有真图：`/screenshot` 与 `/debug/screenshot` 走的是 `fetchBlob`
 * → `URL.createObjectURL` → `<img>`，页面在图片加载失败时会显示空白框。给一段
 * 空 body 或 JSON 会让截图区看起来像布局坏了，与真实核验目的相反。
 * 这里刻意做成纯色：它只证明「链路通、占位框尺寸正确」，不冒充游戏画面。
 */
const SCREENSHOT_PNG_BASE64 = 'iVBORw0KGgoAAAANSUhEUgAAAUAAAAC0CAIAAABqhmJGAAABlElEQVR42u3TMQkAAAzEwJfToWP9a6uOh4NTEEhmDygVCcDAgIEBA4OBAQMDBgYMDAYGDAwYGAwMGBgwMGBgMDBgYMDAgIHBwICBAQODgQEDAwYGDAwGBgwMGBgwMBgYMDBgYDAwYGDAwICBwcCAgQEDg4FVAAMDBgYMDAYGDAwYGDAwGBgwMGBgMDBgYMDAgIHBwICBAQMDBgYDAwYGDAwGBgwMGBgwMBgYMDBgYMDAYGDAwICBwcCAgQEDAwYGAwMGBgwMBgYMDBgYMDAYGDAwYGDAwGBgwMCAgcHAgIEBAwMGBgMDBgYMDBgYDAwYGDAwGBgwMGBgwMBgYMDAgIEBA4OBAQMDBgYDAwYGDAwYGAwMGBgwMBgYMDBgYMDAYGDAwICBAQODgQEDAwYGAwMGBgwMGBgMDBgYMDBgYDAwYGDAwGBgwMCAgQEDg4EBAwMGBgOrAAYGDAwYGAwMGBgwMGBgMDBgYMDAYGDAwICBAQODgQEDAwYGDAwGBgwMGBgMDBgYMDBgYDAwYGDAwICBodsDryyO+s8HFnoAAAAASUVORK5CYII='

/** 假的二进制响应：把 base64 解成字节，交给真实的 Response。 */
function pngResponse() {
  const binary = atob(SCREENSHOT_PNG_BASE64)
  const bytes = new Uint8Array(binary.length)
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index)
  return { body: bytes, contentType: 'image/png' }
}

/** 把 `?a=1&b=2` 解析成普通对象；值一律当字符串（与后端 query_value 一致）。 */
export function parseQuery(search) {
  const out = {}
  const text = String(search || '').replace(/^\?/, '')
  if (!text) return out
  for (const pair of text.split('&')) {
    if (!pair) continue
    const index = pair.indexOf('=')
    const key = index < 0 ? pair : pair.slice(0, index)
    const raw = index < 0 ? '' : pair.slice(index + 1)
    try {
      out[decodeURIComponent(key)] = decodeURIComponent(raw)
    } catch {
      out[key] = raw
    }
  }
  return out
}

/** 拆出「路径」与「查询串」；路径统一去掉尾斜杠，`/` 单独保留。 */
function splitPath(input) {
  const raw = String(input || '')
  const index = raw.indexOf('?')
  const path = (index < 0 ? raw : raw.slice(0, index)).replace(/\/+$/, '') || '/'
  return { path, query: index < 0 ? '' : raw.slice(index) }
}

/**
 * 把一条请求投影成响应体对象。
 *
 * 返回值约定：`{ body, contentType }`；`contentType` 省略时为 JSON。
 * `body` 为字符串时按原文返回（`/search` 这类纯文本路由）。
 * 未覆盖的路由返回 `null`，由调用方决定回退（入口脚本据此报错，而不是白屏）。
 */
export function mockResponse(pathOrUrl, method = 'GET', body) {
  const { path, query } = splitPath(pathOrUrl)
  const params = parseQuery(query)
  const action = String(method || 'GET').toUpperCase()
  const payload = body == null ? {} : typeof body === 'string' ? safeJson(body) : body

  switch (path) {
    // ---- 探活与整体状态 ----
    case '/health':
      // 真实响应是纯文本 `OK`，不是 JSON；面板只判真假。
      return text('OK')
    case '/status':
      return json(demo.playerStatus())
    case '/monitor':
      return json(demo.monitor())
    case '/state':
      return json(demo.runtimeState())
    case '/history':
      return json(demo.history())
    case '/operator/workflows':
      return json(demo.workflows())

    // ---- 配置（形状由 Rust schema 生成，见 fixtures.config.json）----
    case '/config':
      return json(configPayload())
    case '/config/schema':
      return json({ ok: true, sections: fixtures.sections })
    case '/config/section':
      return json(configSection(params.name || 'identity'))
    case '/config/revisions':
      return json(demo.configRevisions())
    case '/config/validate':
      // 校验通过：errors 为空数组（真实响应也是 200 + ok:true）。
      return json({ ok: true, errors: [] })
    case '/config/save':
      return json(demo.configSaved())
    case '/config/rollback':
      return json(demo.configSaved())
    case '/config/visual':
      return json(configVisual())
    case '/config/visual/preview':
      return json({ ok: true, revision: 42, differences: [], errors: [] })
    case '/config/visual/import':
      return json(demo.configSaved())

    // ---- 命令面板 ----
    case '/commands/catalog':
      return json(demo.commandCatalog())
    case '/commands/replies':
      return json(action === 'POST' ? demo.configSaved() : demo.commandReplies())
    case '/commands/policy':
      return json(demo.configSaved())

    // ---- 播放器 ----
    case '/queue':
      return json(demo.queue())
    case '/playback/mode':
      // GET 只回 mode；POST 才带回执（真实路由对 GET 走同一个 handler）。
      return action === 'POST' ? json({ ok: true, mode: params.mode || 'shuffle' }) : json({ mode: 'shuffle' })
    case '/playback/insights':
      return json(demo.playbackInsights())
    case '/playback/leaderboard':
      return json(demo.leaderboard())
    case '/playback/cache/tracks':
      return json(demo.cachedTracks())
    case '/search/candidates':
      return json(demo.searchCandidates())
    case '/search':
      // 纯文本路由：current 面板按 JSON 解析会报错，假数据必须与真实一致。
      return text(demo.searchText())
    case '/player/providers':
      return json(demo.playerProviders())
    case '/player/login/status':
      return json(demo.loginStatus())
    case '/player/kugou/status':
      return json(demo.kugouStatus())
    case '/player/kugou/phone/code':
      return json(demo.kugouPhoneCodeSent(payload?.mobile))
    case '/player/kugou/phone/login':
      return json(demo.kugouPhoneLoginStatus())
    case '/player/account/status':
      return json(demo.accountStatus(params.provider || 'netease'))
    case '/player/play-track':
      return json(demo.actionReceipt('play-track'))
    case '/playback/seek':
    case '/playback/cache/invalidate':
    case '/playback/song/delete':
    case '/playback/statistics/reset':
      return json(demo.actionReceipt(path))
    case '/player/login/start':
      return json({ ok: true, sessionId: demo.loginStatus().sessionId, provider: params.provider || 'qqmusic' })
    case '/player/login/cancel':
    case '/player/login/refresh':
    case '/player/logout':
    case '/player/account/refresh':
    case '/player/kugou/claim-vip':
    case '/player/kugou/upgrade-vip':
      return json({ ok: true, message: '已提交' })

    // ---- 点歌与播放控制（统一回执形状）----
    case '/searchPlay':
    case '/searchSource':
    case '/ai/search':
    case '/play':
    case '/pause':
    case '/skip-next':
    case '/skip-prev':
    case '/volume':
    case '/queue/add':
    case '/queue/remove':
    case '/queue/clear':
    case '/startup/game':
    case '/startup/wonderland':
    case '/startup/enter-wonderland':
    case '/operator/lyrics':
    case '/operator/microphone':
    case '/operator/hall-detect':
    case '/operator/hall-time':
    case '/operator/commands':
    case '/operator/idle-exit':
    case '/operator/workflows/run':
    case '/chat/send':
    case '/chat-listener/mode':
    case '/state/save':
    case '/tasks/cancel':
    case '/decisions/submit':
    case '/clear-history':
      return json(demo.actionReceipt(path))

    // ---- AI ----
    case '/ai/models':
      return json(demo.aiModels())
    case '/ai/vision/status':
      return json(demo.aiVisionStatus())
    case '/ai/trace':
      return json(demo.aiTrace())
    case '/ai/trace/entry':
      return json(demo.aiTraceEntry(params.id))
    case '/ai/memory/actors':
      return json(demo.aiMemoryActors())
    case '/ai/memory/hall':
      return json(demo.aiMemoryHall())
    case '/ai/memory/actor':
      return json(demo.aiMemoryActor(params.name))
    case '/ai/memory/forget-actor':
    case '/ai/memory/delete-fact':
    case '/ai/memory/rebuild':
    case '/ai/memory/forget-hall':
      return json(demo.configSaved())
    case '/ai/chat/preview':
      return json(demo.aiChatPreview())
    case '/ai/recognize':
      return json({ ok: true, songs: [{ name: '晴天', artist: '周杰伦', confidence: 0.93 }] })
    case '/ai/match':
      return json({ ok: true, matched: true, songName: '晴天', songSinger: '周杰伦', confidence: 0.88 })
    case '/ai/pick':
      return json({ ok: true, index: 0, track: demo.searchCandidates()[0] })
    case '/tts/models':
      return json(demo.ttsCatalog())

    // ---- 表情 ----
    case '/emoji/catalog':
      return json(demo.emojiCatalog())
    case '/emoji/scanned':
      return json(demo.emojiScanned())
    case '/emoji/scan':
      return json(demo.emojiScanned())

    // ---- 娱乐玩法 ----
    case '/turtle-soup':
      return json(demo.turtleSoup())
    case '/undercover':
      return json(demo.undercover())
    case '/idiom-chain':
      return json(demo.idiomChain())
    case '/number-bomb':
      return json(demo.numberBomb())
    case '/card-game':
      return json(demo.cardGame())
    case '/turtle-soup/start':
    case '/turtle-soup/end':
    case '/turtle-soup/questions':
    case '/undercover/start':
    case '/undercover/end':
      return json(demo.actionReceipt(path))

    // ---- 截图（二进制）----
    case '/screenshot':
    case '/hall-screenshot':
    case '/debug/screenshot':
    case '/debug/read-only/image':
      return pngResponse()

    // ---- 工具与调试 ----
    case '/tools/templates':
      return json(demo.toolTemplates())
    case '/tools/task':
      return json(demo.toolTask(params.id))
    case '/debug/read-only/mode':
      return json(demo.readOnlyMode())
    case '/debug/read-only/summary':
      return json(demo.readOnlySummary())
    case '/debug/read-only/entries':
      return json(demo.readOnlyEntries())
    case '/debug/state':
      return json(demo.debugState())
    case '/debug/tasks':
      return json(action === 'POST' ? { ok: true, taskId: 'demo-click' } : demo.debugTasks())
    case '/debug/runs':
      return json({ ok: true, runs: [] })
    case '/debug/ui-catalogue':
      return json({ ok: true, link: 'D:/mock/panel-ui-catalogue.json', items: [], saved: false })
    case '/debug/command':
      return json({ ok: true, command: payload.command || 'capture', receipt: { accepted: true, note: '假数据：未真实执行' } })
    case '/debug/mode':
      return json({ ok: true, enabled: Boolean(payload.enabled), note: '假数据：调试模式未真实切换' })
    case '/debug/events':
      return { body: '{"event":"mock","sequence":0}\n', contentType: 'text/plain; charset=utf-8' }
    case '/debug/run':
    case '/debug/run/replay':
      return json({ ok: true, run: { id: 'run-1', status: 'queued', steps: [], sourceRunId: null } })
    case '/debug/run/all':
    case '/debug/run/step':
    case '/debug/run/cancel':
    case '/debug/run/status':
      return json({ ok: true, run: { id: params.runId || params.id || 'run-1', status: 'running', steps: [], cursor: 0 } })
    case '/debug/run/evidence':
      return json({ ok: true, evidence: { id: params.id || 'run-1', frames: [], assertions: [] } })
    case '/debug/window':
      return json({ ok: true, note: '只读窗口状态', available: true, enabled: true })
    case '/debug/read-only/read':
      return json({ ok: true, evidence: { id: '9f2c-0001', label: 'debug', ocr: { text: '示例文本' } } })
    case '/debug/read-only/evidence':
      return json({ ok: true, evidence: { id: params.id || '9f2c-0001', items: [] } })
    case '/debug/run/replay':
      return json({ ok: true, sourceRunId: params.runId || params.id || '', run: { id: 'run-1', status: 'queued', steps: [] } })
    case '/debug/simulate-chat':
      return json({ ok: true })
    case '/debug/ui-state':
      return json({ ok: true, kind: 'Hall', image: 'deps/diagnostics/mock-ui.png', confidence: 0.98 })
    // 一次性诊断入口：真实后端返回任务回执（DiagnosticTaskSnapshot），形状统一。
    case '/tools/ocr':
    case '/tools/scan-chat':
    case '/tools/ui-state':
    case '/tools/hall-name':
    case '/tools/ocr-backends':
    case '/tools/capture-benchmark':
    case '/tools/chat-change-samples':
    case '/tools/panel-benchmark':
    case '/tools/ai-preview':
    case '/tools/template':
    case '/tools/click':
    case '/tools/key':
      return json(demo.toolTask(31))
    default:
      // 注意这里**不做前缀兜底**：`path.startsWith('/tools/')` 会让任何拼错的路由
      // 都拿到一份看起来正常的任务回执，页面显示「已完成」而实际什么都没测。
      // 只有上面显式列出的路由才算覆盖，其余一律 null（由 mockFetch 回 404）。
      return null
  }
}

/** `fetch()` 的假实现：签名与真实 fetch 一致，返回 Response 形状的对象。 */
export async function mockFetch(input, init = {}) {
  const url = typeof input === 'string' ? input : input && input.url ? input.url : String(input)
  const method = (init && init.method) || (input && input.method) || 'GET'
  const resolved = mockResponse(url, method, init && init.body)
  if (!resolved) {
    // 未覆盖的路由显式 404：宁可让页面显示一条明确错误，也不要静默返回空对象
    // 让用户以为「后端没数据」。入口脚本会先对着 MOCK_ROUTES 做覆盖自检。
    return new Response(JSON.stringify({ message: `假数据未覆盖该接口：${url}` }), {
      status: 404,
      headers: { 'Content-Type': 'application/json; charset=utf-8' },
    })
  }
  return new Response(resolved.body, {
    status: 200,
    headers: { 'Content-Type': resolved.contentType || 'application/json; charset=utf-8' },
  })
}

function json(value) {
  return { body: JSON.stringify(value) }
}

function text(value) {
  return { body: String(value), contentType: 'text/plain; charset=utf-8' }
}

function safeJson(value) {
  try {
    return JSON.parse(value)
  } catch {
    return {}
  }
}

/**
 * 段值的**样本覆盖**：只补「schema 默认是空、但页面没有内容就测不出布局」的段。
 *
 * 为什么不能改 fixtures.config.js：那个文件是 scripts/panel-mock-gen.py 的产物，
 * 逐字段等于 Rust 的 schema 默认值，手改会在下次生成时被冲掉。样本数据属于
 * 「核验用的演示内容」，放在这一层，与形状生成分开维护。
 *
 * 键名必须与 Rust 的一致（src/features/identity.rs 的 IdentityMapping：
 * `remark`/`display_name` 在 JSON 里叫 `nickname`/`note`，字段名不可改）。
 */
const SECTION_SAMPLES = {
  identity: {
    mappings: [
      {
        id: 'mapping-mock-0001',
        role: 'owner',
        nickname: '旅行者',
        note: '群主',
        real_nickname: 'Traveler',
        remark_sourced: true,
        accounts: [{ game: 'genshin', uid: '100000001' }],
      },
      {
        id: 'mapping-mock-0002',
        role: 'admin',
        nickname: '小派蒙',
        note: '',
        real_nickname: 'Paimon',
        remark_sourced: false,
        accounts: [
          { game: 'genshin', uid: '100000002' },
          { game: 'starrail', uid: '200000002' },
        ],
      },
      {
        id: 'mapping-mock-0003',
        role: 'friend',
        nickname: '可莉',
        note: '小朋友',
        real_nickname: 'Klee',
        remark_sourced: true,
        accounts: [],
      },
    ],
  },
  command_access: {
    overrides: {
      '@QQ点歌': { enabled: true, minimum_role: 'user' },
      '@清场': { enabled: true, minimum_role: 'admin' },
    },
  },
  custom_workflows: {
    workflows: [
      { name: '每日签到', commands: ['签到'], allow_args: false, confirm_before_run: false },
      { name: '清理大厅', commands: ['清场'], allow_args: true, confirm_before_run: true },
    ],
  },
}

/** 取某段的值：样本覆盖优先，其余用 schema 默认值铺出来的那份。 */
function sectionValues(name) {
  const base = fixtures.values[name] || {}
  const sample = SECTION_SAMPLES[name]
  return sample ? { ...base, ...deepClone(sample) } : base
}

/** `/config` 的全量段值。 */
function allSectionValues() {
  const out = {}
  for (const section of fixtures.sections) out[section.name] = sectionValues(section.name)
  return out
}

function deepClone(value) {
  return JSON.parse(JSON.stringify(value))
}

/**
 * `GET /config` 的响应：形状照 routes/config.rs 的 config_response，
 * 段数据来自 fixtures.config.js 的 `values` + SECTION_SAMPLES。
 */
function configPayload() {
  return {
    ok: true,
    revision: 42,
    schemaVersion: fixtures.schemaVersion,
    sections: allSectionValues(),
    loadMode: 'lenient',
    quarantinedCount: 1,
    quarantined: [
      {
        section: 'identity',
        elementRef: 'identity.mappings[1]',
        reason: '账号重复（示例数据，用于核验隔离详情表格）',
        createdAtMs: MOCK_NOW,
      },
    ],
  }
}

/** `GET /config/section`：schema 段 + 当前值 + 版本号（routes/config.rs）。 */
function configSection(name) {
  const section = fixtures.sections.find((item) => item.name === name) || fixtures.sections[0]
  return {
    ok: true,
    name: section.name,
    label: section.label,
    domain: section.domain,
    domainLabel: section.domainLabel,
    order: section.order,
    fields: section.fields,
    visualFields: section.fields.filter((field) => field.managedBy === 'visual').length,
    values: sectionValues(section.name),
    revision: 42,
  }
}

/**
 * `GET /config/visual`：视觉档案（src/config/visual.rs 的 VisualProfile）。
 * 键全是 snake_case——这个结构体**没有** rename_all，照抄即可。
 */
function configVisual() {
  const profile = {
    name: fixtures.values.visual && fixtures.values.visual.name ? fixtures.values.visual.name : '1080P',
    schema_version: 1,
    canvas: { width: 1920, height: 1080 },
    templates: {
      // 32 + 2 个内置模板键，顺序与 src/config/visual.rs 的 fields() 声明一致；
      // VisualEditor 直接遍历这里，少一个就少一个输入框（截图会静默缺行）。
      blue_marker: 'deps/assets/markers/blue.png',
      yellow_marker: 'deps/assets/markers/yellow.png',
      pink_marker: 'deps/assets/markers/pink.png',
      secondary_back: 'deps/assets/ui/secondary-back.png',
      secondary_hall: 'deps/assets/ui/secondary-hall.png',
      secondary_chat_submit: 'deps/assets/ui/secondary-chat-submit.png',
      qianxing_f2_hall_list: 'deps/assets/qianxing/f2-hall-list.png',
      qianxing_f2_invite_player: 'deps/assets/qianxing/f2-invite-player.png',
      qianxing_exit_menu: 'deps/assets/qianxing/exit-menu.png',
      qianxing_map_label: 'deps/assets/qianxing/map-label.png',
      qianxing_profile_page_wishlist: 'deps/assets/qianxing/profile-wishlist.png',
      qianxing_profile_page_wardrobe: 'deps/assets/qianxing/profile-wardrobe.png',
      teyvat_profile_adventure_rank: 'deps/assets/teyvat/adventure-rank.png',
      teyvat_profile_world_level: 'deps/assets/teyvat/world-level.png',
      teyvat_profile_add_friend: 'deps/assets/teyvat/add-friend.png',
      qianxing_profile_domain_level: 'deps/assets/qianxing/domain-level.png',
      qianxing_profile_add_friend: 'deps/assets/qianxing/add-friend.png',
      qianxing_profile_view_profile: 'deps/assets/qianxing/view-profile.png',
      main_menu: 'deps/assets/ui/main-menu.png',
      world_wish: 'deps/assets/ui/world-wish.png',
      invite_view_star: 'deps/assets/invite/view-star.png',
      invite_goto_hall: 'deps/assets/invite/goto-hall.png',
      invite_enter_hall: 'deps/assets/invite/enter-hall.png',
      friend_panel: 'deps/assets/friend/panel.png',
      friend_search_panel: 'deps/assets/friend/search-panel.png',
      friend_more_settings: 'deps/assets/friend/more-settings.png',
      friend_block_chat: 'deps/assets/friend/block-chat.png',
      friend_blacklist: 'deps/assets/friend/blacklist.png',
      confirm_button: 'deps/assets/ui/confirm.png',
      confirm_message_random_hall: 'deps/assets/ui/confirm-random-hall.png',
      confirm_message_return_teyvat: 'deps/assets/ui/confirm-return-teyvat.png',
      confirm_message_block: 'deps/assets/ui/confirm-block.png',
      'startup.wonderland_map_star': 'deps/assets/startup/wonderland-map-star.png',
      'startup.paimon_menu': 'deps/assets/startup/paimon-menu.png',
    },
    thresholds: { builtin: 0.9, startup: 0.78, startup_confirm: 0.82 },
    regions: {
      // 13 个 screen.*_rect + moderation/invite/startup 的独立区域。
      chat: { x: 39, y: 879, width: 416, height: 143 },
      friend: { x: 170, y: 1018, width: 50, height: 40 },
      secondary_back: { x: 15, y: 15, width: 65, height: 65 },
      secondary_hall: { x: 10, y: 190, width: 65, height: 55 },
      secondary_chat_submit: { x: 930, y: 950, width: 150, height: 124 },
      main_menu: { x: 1668, y: 998, width: 153, height: 44 },
      hall_bar: { x: 0, y: 985, width: 600, height: 95 },
      qianxing_f2_invite: { x: 1650, y: 993, width: 140, height: 54 },
      world_wish: { x: 1300, y: 0, width: 620, height: 110 },
      hall_name: { x: 75, y: 425, width: 325, height: 40 },
      hall_member_count: { x: 75, y: 470, width: 450, height: 50 },
      hall_time: { x: 430, y: 520, width: 110, height: 40 },
      hall_member_list: { x: 1280, y: 110, width: 560, height: 850 },
      'moderation.friend_panel': { x: 60, y: 120, width: 320, height: 640 },
      'moderation.search_panel': { x: 60, y: 80, width: 320, height: 40 },
      'moderation.confirm_message': { x: 700, y: 460, width: 520, height: 160 },
      'invite.friend_list': { x: 100, y: 160, width: 480, height: 760 },
      'invite.goto_hall': { x: 1650, y: 993, width: 140, height: 54 },
      'startup.main_ui': { x: 0, y: 0, width: 1920, height: 1080 },
      'startup.wonderland_map_star': { x: 1700, y: 960, width: 180, height: 100 },
    },
    points: {
      // 5 个点位，键名抄自 src/config/visual.rs 的 `("输出键", "配置路径")` 清单。
      'window.focus': { x: 1919, y: 1000 },
      'output.focus': { x: 960, y: 1000 },
      'output.chat_click': { x: 320, y: 960 },
      'moderation.search_input': { x: 400, y: 120 },
      'moderation.search_button': { x: 760, y: 120 },
    },
    chat_geometry: {
      // 14 个 ocr.* 数值项；VisualEditor 按类型渲染成单个输入框。
      marker_search_width: 60,
      marker_dedupe_min_x: 22,
      marker_dedupe_min_y: 14,
      marker_dedupe_x: 8,
      marker_dedupe_y: 8,
      text_left_gap: 8,
      block_top_padding: 2,
      block_bottom_padding: 2,
      max_block_height: 120,
      same_line_y_tolerance: 10,
      next_marker_min_gap: 12,
      right_padding: 4,
      change_mean_threshold: 6,
      change_pixel_threshold: 0.03,
    },
    capture: { capture_method: 'gdi', warn_on_size_mismatch: true },
    matching: {
      'ocr.min_confidence': 0.9,
      'ocr.det_max_side_len': 960,
      'ocr.det_score_threshold': 0.3,
      'ocr.det_unclip_ratio': 2,
      'ocr.det_min_area': 9,
      'ocr.det_box_border': 0,
      'stability.default_count': 2,
      'stability.ui_state_count': 2,
      'stability.secondary_hall_count': 2,
      'startup.stable_mean_threshold': 4,
      'startup.stable_changed_ratio_threshold': 0.02,
      'moderation.stable_vote_samples': 6,
      'moderation.required_vote_margin': 2,
      panel_downsample_factors: {
        friend_panel: 1,
        friend_search_panel: 1,
        friend_block_chat: 4,
        confirm_button: 4,
        invite_enter_hall: 4,
        invite_goto_hall: 4,
        invite_view_star: 4,
      },
    },
  }
  return {
    ok: true,
    revision: 42,
    profile,
    effectiveProfile: profile,
    defaultProfile: profile,
    // 待重载字段：真实响应来自 live_configs.pending_reload_fields()；
    // 面板按「条数」展示，示例给两条让 chip 有数字可读。
    pendingReloadFields: ['screen.chat_rect', 'ocr.min_confidence'],
    catalog: {
      algorithms: { chat_markers: 'rgb_sad', other_templates: 'gray_sad' },
      panelDownsampleFactors: { friend_panel: 1, friend_search_panel: 1, friend_block_chat: 4 },
      f2HallButtonAnchors: { hall_list: 'screen.hall_bar_rect', invite_player: 'screen.qianxing_f2_invite_rect', downscale: 4, min_score: 0.95 },
      structuralClassification: { hall_bar: 'screen.hall_bar_rect' },
      derivedRegions: { 'friend_delivery.secondary_hall_search': { operation: 'bounded_union', sources: ['secondary_hall', 'invite.friend_list'] } },
      notes: ['示例档案用于面板视觉核验，不代表真实匹配参数。'],
    },
    statusNote: '保存值路径保持原样；生效值路径可能已解析。待重载字段以列表为准。',
    profileDifferences: [
      { path: '/templates/blue_marker', before: 'deps/assets/markers/blue.png', after: 'C:/mock/deps/assets/markers/blue.png' },
    ],
  }
}
