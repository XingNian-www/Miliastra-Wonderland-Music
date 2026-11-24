// 面板假数据的形状契约。
//
// 这些用例钉死的不是「假数据长什么样」，而是「它与真实后端是否同形」——
// 核验工具的价值完全取决于这一点：形状错一个字段，截出来的页面就是错的，
// 而错的地方看起来仍然像正常页面，比白屏更难发现。
//
// 参照物：
//   1. Rust 的配置 schema 快照 + 声明点（scripts/panel-mock-gen.py 的输入）；
//   2. scripts/panel-api-coverage.py 能解析到的路由表（protocol.rs 的 RouteSpec）；
//   3. 各 handler 的响应构造函数（见 web/src/mock/routes.js 上方注释）。
//
// 这里不启动浏览器：Node 直接 import mock 模块即可，跑得快、失败信息也具体。

import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync, readdirSync } from 'node:fs'
import { execFileSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { dirname, relative, resolve } from 'node:path'

import fixtures from './fixtures.config.js'
import { MOCK_ROUTES, mockResponse, parseQuery } from './index.js'

const HERE = dirname(fileURLToPath(import.meta.url))
const ROOT = resolve(HERE, '../../..')

/** 解析一次假响应；顺带钉住「必须是 JSON 而不是纯文本」这类路由级差异。 */
function get(path, method = 'GET', body) {
  const resolved = mockResponse(path, method, body)
  assert.ok(resolved, `假数据未覆盖 ${method} ${path}（新增路由后要同步 web/src/mock/index.js）`)
  return resolved
}

function getJson(path, method = 'GET', body) {
  const resolved = get(path, method, body)
  assert.equal(resolved.contentType || 'application/json; charset=utf-8', 'application/json; charset=utf-8')
  return JSON.parse(resolved.body)
}

test('Rust 配置 schema 快照：段/字段数与声明一致', () => {
  const snapshot = JSON.parse(readFileSync(resolve(ROOT, 'src/config/schema/schema.snapshot.json'), 'utf8'))
  assert.equal(fixtures.sections.length, snapshot.sections.length)
  let expected = 0
  for (const [index, section] of snapshot.sections.entries()) {
    const produced = fixtures.sections[index]
    assert.equal(produced.name, section.name)
    assert.equal(produced.label, section.label)
    assert.equal(produced.domain, section.domain)
    assert.equal(produced.order, section.order)
    assert.equal(produced.fields.length, section.fields.length)
    expected += section.fields.length
    for (const [fieldIndex, field] of section.fields.entries()) {
      const out = produced.fields[fieldIndex]
      // 路径与顺序必须逐字一致：前端按数组顺序渲染，错位会让分组标题与字段对不上。
      assert.equal(out.path, field.path)
      assert.equal(out.effect, field.effect)
      assert.equal(out.source, field.source)
      assert.equal(out.nullable, field.nullable)
      assert.equal(out.optionalParent, field.optionalParent)
      assert.equal(out.group, field.group)
      assert.equal(out.kind.type, field.kind.type)
    }
  }
  assert.equal(
    fixtures.sections.reduce((sum, section) => sum + section.fields.length, 0),
    expected,
  )
})

test('fixtures 与生成脚本同步（改 Rust schema 后必须重跑 panel-mock-gen.py）', () => {
  // --check 在产物过期时返回非 0；这里把它变成一条可读的失败信息。
  const result = execFileSync('python3', [resolve(ROOT, 'scripts/panel-mock-gen.py'), '--check'], {
    cwd: ROOT,
    encoding: 'utf8',
    // 过期是预期的失败路径，别让非 0 退出码直接抛出来。
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  assert.match(result, /同步/)
})

test('每个字段都带中文 label / hint，且没有跨段串味', () => {
  const fields = fixtures.sections.flatMap((section) => section.fields)
  const labels = fields.filter((field) => !field.label || field.label === field.path.split('.').pop())
  assert.deepEqual(labels, [], `以下字段退回了段内路径，说明扫描器取不到中文名：${labels.map((f) => f.path)}`)

  // 同名段内路径在多段重复出现（model/enabled/backend_priority…）。
  // 曾经只按段内路径归并，导致 ai.model 的提示词显示成「海龟汤专用模型」。
  const byPath = new Map(fields.map((field) => [field.path, field]))
  assert.match(byPath.get('ai.model').hint, /所有 AI 功能/)
  assert.match(byPath.get('turtle_soup.model').hint, /海龟汤/)
  assert.match(byPath.get('song_ai.model').hint, /点歌/)
  assert.match(byPath.get('tts.model').hint, /HTTP model/)
  assert.notEqual(byPath.get('ai.model').hint, byPath.get('turtle_soup.model').hint)
})

test('enum 选项与真实 /config/schema 同形：{value,label}，label 不是裸值', () => {
  const enums = fixtures.sections.flatMap((section) => section.fields).filter((field) => field.kind.type === 'enum')
  assert.ok(enums.length > 0, 'schema 里本该有 enum 字段')
  const rawValues = []
  for (const field of enums) {
    assert.ok(Array.isArray(field.kind.values) && field.kind.values.length > 0, `${field.path} 缺少 enum 选项`)
    for (const option of field.kind.values) {
      assert.equal(typeof option, 'object', `${field.path} 的选项必须是 {value,label}，不能是裸字符串`)
      assert.equal(typeof option.value, 'string')
      assert.equal(typeof option.label, 'string')
      if (option.label === option.value) rawValues.push(`${field.path}=${option.value}`)
    }
  }
  assert.deepEqual(rawValues, [], `这些选项没有显示名，下拉框会显示成「(未知 值)」：${rawValues}`)
})

test('secret 字段的默认值与真实 /config 一样是掩码，不是明文', () => {
  const secrets = fixtures.sections.flatMap((section) => section.fields).filter((field) => field.kind.type === 'secret')
  assert.ok(secrets.length > 0)
  for (const field of secrets) {
    assert.equal(field.default, '••••••', `${field.path} 必须用 SECRET_MASK`)
  }
  const masked = new Set(secrets.map((field) => field.path))
  const walk = (node, prefix = '') => {
    for (const [key, value] of Object.entries(node || {})) {
      const path = prefix ? `${prefix}.${key}` : key
      if (value && typeof value === 'object' && !Array.isArray(value)) walk(value, path)
      else if (masked.has(path)) assert.equal(value, '••••••', `/config 的 ${path} 必须是掩码`)
    }
  }
  walk(fixtures.values)
})

test('视觉档案接管的字段带 managedBy=visual（与 Rust managed_paths 一致）', () => {
  const managed = fixtures.sections
    .flatMap((section) => section.fields)
    .filter((field) => field.managedBy === 'visual')
    .map((field) => field.path)
  // 真实后端按 visual.rs 的 managed_paths() 逐字段标注，数量少一个都会让
  // 配置中心与视觉档案页出现「同一字段两处可改」或「哪都不能改」。
  for (const path of ['visual.name', 'window.content_width', 'screen.chat_rect', 'templates.blue_marker', 'ocr.min_confidence']) {
    assert.ok(managed.includes(path), `${path} 应由视觉档案接管`)
  }
  assert.ok(managed.length > 90, `托管字段太少（${managed.length}），managed_paths 清单可能漏抄`)
})

test('/config 与 /config/schema 的段数据可以互相回填', () => {
  const schema = getJson('/config/schema')
  const config = getJson('/config')
  assert.equal(schema.ok, true)
  assert.equal(config.ok, true)
  assert.equal(config.schemaVersion, fixtures.schemaVersion)
  assert.equal(typeof config.revision, 'number')
  assert.equal(config.loadMode, 'lenient')

  const sectionNames = new Set(schema.sections.map((section) => section.name))
  assert.equal(Object.keys(config.sections).length, sectionNames.size, '/config 少段或多段')
  for (const name of Object.keys(config.sections)) {
    assert.ok(sectionNames.has(name), `/config 出现了 schema 里没有的段 ${name}`)
  }
  // 每个字段的点路径都必须能在 sections 树里解析到：缺层的表现是页面显示 undefined。
  for (const section of schema.sections) {
    for (const field of section.fields) {
      const segments = field.path.split('.')
      let node = config.sections
      for (const segment of segments.slice(0, -1)) {
        assert.ok(node && typeof node === 'object', `${field.path} 的父级缺失`)
        node = node[segment]
      }
      const value = node[segments[segments.length - 1]]
      assert.notEqual(value, undefined, `${field.path} 在 /config 里解析不到`)
    }
  }
  // 容错加载状态：与 util/loadStatus.js 的判据一致（读失败时可为 null）。
  assert.ok('quarantinedCount' in config && 'quarantined' in config)
})

test('/config/section 返回「schema 段 + 当前值 + 版本号」三件套', () => {
  for (const name of ['identity', 'ai', 'ai_chat', 'tts', 'queue']) {
    const payload = getJson(`/config/section?name=${name}`)
    assert.equal(payload.ok, true)
    assert.equal(payload.name, name)
    assert.equal(typeof payload.label, 'string')
    assert.equal(typeof payload.domainLabel, 'string')
    assert.equal(typeof payload.revision, 'number')
    assert.ok(Array.isArray(payload.fields) && payload.fields.length > 0)
    assert.ok(payload.values && typeof payload.values === 'object', `${name} 缺少 values`)
  }
})

test('身份映射：values.mappings 的每个键都能被 util/identityRow.js 读出来', () => {
  // 换算逻辑在 web/src/util/identityRow.js（有独立单测）；这里只钉「键名对得上」。
  const payload = getJson('/config/section?name=identity')
  const mappings = payload.values.mappings
  assert.ok(Array.isArray(mappings) && mappings.length > 0, '身份映射页需要至少一条样本才能看出表格布局')
  for (const row of mappings) {
    for (const key of ['id', 'role', 'nickname', 'note', 'real_nickname', 'accounts']) {
      assert.ok(key in row, `身份映射缺 ${key}`)
    }
    assert.ok(['friend', 'admin', 'owner'].includes(row.role))
    for (const account of row.accounts) {
      assert.deepEqual(Object.keys(account).sort(), ['game', 'uid'])
    }
  }
})

test('顶层数组路由：/queue /search/candidates /player/providers /history /operator/workflows /tools/templates', () => {
  // 这几个路由后端直接序列化 Vec<T>，不是 {items: [...]}。
  // 包错一层会让页面显示「暂无数据」而不是报错，最难发现。
  for (const path of ['/queue', '/history', '/operator/workflows', '/tools/templates', '/player/providers', '/search/candidates']) {
    const payload = getJson(path)
    assert.ok(Array.isArray(payload), `${path} 必须是顶层数组`)
    assert.ok(payload.length > 0, `${path} 需要至少一条样本`)
  }
  const queue = getJson('/queue')
  for (const item of queue) {
    for (const key of ['id', 'keyword', 'source', 'preferAccompaniment', 'requester', 'friendUsername']) {
      assert.ok(key in item, `/queue 项缺 ${key}`)
    }
    // track 是 skip_serializing_if 的 Option：没有时后端**省略该键**而不是写 null。
    if ('track' in item && item.track !== undefined) {
      assert.ok(item.track.trackRef.key.provider)
    }
  }
  const history = getJson('/history')
  for (const item of history) assert.deepEqual(Object.keys(item).sort(), ['command', 'ok', 'query', 'result', 'time'])
})

test('/search 是纯文本而不是 JSON（页面按 JSON 解析会报错，假数据必须与真实一致）', () => {
  const resolved = get('/search?keyword=晴天')
  assert.equal(resolved.contentType, 'text/plain; charset=utf-8')
  assert.throws(() => JSON.parse(resolved.body), '纯文本不该能被 JSON.parse')
  assert.match(resolved.body, /^1\. /)
})

test('/health 是纯文本 OK（面板只判真假，SettingsPage 用 /config/schema 验令牌）', () => {
  const resolved = get('/health')
  assert.equal(resolved.body, 'OK')
  assert.equal(resolved.contentType, 'text/plain; charset=utf-8')
})

test('/monitor：所有 Option 字段都显式为 null，不省略键', () => {
  const monitor = getJson('/monitor')
  for (const key of ['logs', 'ocr', 'queue', 'commands', 'status', 'playbackController', 'chatListener', 'operational', 'uiRoutine', 'pendingTasks', 'webTools', 'tasks', 'decision', 'turtleSoup', 'undercover']) {
    assert.ok(key in monitor, `/monitor 缺 ${key}`)
  }
  // Rust 侧 MonitorSnapshot 没有任何 skip_serializing_if：nullable 一律显式 null。
  assert.equal(monitor.uiRoutine, null)
  assert.ok(monitor.operational.hallRemainingMinutes !== undefined)
  assert.equal(monitor.chatListener.pendingMode, '', 'pendingMode 为空时是空串而不是 null')
  for (const task of monitor.tasks) {
    assert.ok('startedAtMs' in task && 'finishedAtMs' in task, '任务快照的时间戳键必须存在（可为 null）')
  }
  // 大厅里展示的是中文值守标签，不是 wire 枚举 primary/secondary。
  assert.match(monitor.chatListener.mode, /值守/)
})

test('/status：currentTrack 省略而不是 null（缺键时页面显示「空闲」）', () => {
  const status = getJson('/status')
  assert.equal(status.status, 'playing')
  assert.ok(status.currentTrack, '示例数据应当有正在播放的曲目')
  assert.ok(status.currentTrack.trackRef.key.provider)
  assert.ok(Array.isArray(status.currentTrack.metadata.artists))
  for (const key of ['duration', 'progress', 'volume', 'requester', 'lyricLineText', 'failureCode']) {
    assert.ok(key in status, `/status 缺 ${key}`)
  }
})

test('/state：playback 里没有 previousRequests（backend 明确移除）', () => {
  const state = getJson('/state')
  assert.ok(!('previousRequests' in state.playback), '/state 不应包含 previousRequests')
  for (const key of ['hallRemainingMinutes', 'hallRemainingUpdatedAt', 'hallExpiringWarningSent', 'hallRemainingMinutesNow']) {
    assert.ok(key in state, `/state 缺 ${key}`)
  }
  assert.equal(typeof state.playback.volume, 'number')
})

test('/playback/mode：GET 只回 mode，POST 才带回执', () => {
  assert.deepEqual(Object.keys(getJson('/playback/mode')).sort(), ['mode'])
  const post = getJson('/playback/mode?mode=repeat_one', 'POST')
  assert.equal(post.ok, true)
  assert.equal(post.mode, 'repeat_one')
})

test('/playback/insights：cache 可为 null，但键必须存在', () => {
  const insights = getJson('/playback/insights')
  assert.ok('cache' in insights && 'history' in insights)
  assert.ok(insights.cache && typeof insights.cache.completeEntries === 'number')
  for (const item of insights.history) {
    for (const key of ['keyword', 'source', 'title', 'artist', 'requester', 'startedAtMs', 'cached', 'playCount', 'failureCount']) {
      assert.ok(key in item, `/playback/insights 历史项缺 ${key}`)
    }
  }
  // CachedTrackInfo 没有 skip_serializing_if：nullable 键必须存在（可为 null）。
  const page = getJson('/playback/cache/tracks')
  assert.equal(typeof page.total, 'number')
  for (const track of page.tracks) {
    for (const key of ['hash', 'source', 'id', 'title', 'artists', 'bytes', 'complete', 'lastUsedAtMs', 'failureCount']) {
      assert.ok(key in track, `/playback/cache/tracks 缺 ${key}（后端不会省略，只会写 null）`)
    }
  }
  // 孤儿文件（没有身份）是真实存在的分支：页面据此加 orphan 样式。
  assert.ok(page.tracks.some((track) => track.source === null))
})

test('/player/providers：字段表是 BTreeMap<String,bool>，四个平台齐全', () => {
  const providers = getJson('/player/providers')
  assert.deepEqual(providers.map((item) => item.provider), ['qqmusic', 'netease', 'bilibili', 'kugou'])
  for (const item of providers) {
    assert.equal(typeof item.configured, 'boolean')
    assert.equal(typeof item.refreshSupported, 'boolean')
    assert.equal(typeof item.manualRefreshSupported, 'boolean')
    assert.equal(typeof item.fields, 'object')
    for (const [key, value] of Object.entries(item.fields)) {
      assert.equal(typeof value, 'boolean', `${item.provider}.${key} 必须是布尔`)
    }
    // lastRefreshAtMs / lastRefreshError 没有 skip 属性：键存在，值为 null。
    for (const key of ['lastRefreshAtMs', 'nextRefreshCheckAtMs', 'lastRefreshError']) {
      assert.ok(key in item, `/player/providers 缺 ${key}`)
    }
  }
})

test('/player/login/status：非活动会话省略 sessionId/provider/qrCode/lastError', () => {
  const active = getJson('/player/login/status')
  assert.equal(active.active, true)
  assert.equal(typeof active.sessionId, 'string')
  assert.equal(active.lastError.code, 'provider_auth_required')
})

test('/player/account/status：未知平台回落到样本，loginMethod 缺省时省略该键', () => {
  const qq = getJson('/player/account/status?provider=qqmusic')
  assert.equal(qq.loginMethod, 'qq')
  const netease = getJson('/player/account/status?provider=netease')
  assert.ok(!('loginMethod' in netease), 'loginMethod 为 None 时后端省略该键')
  assert.ok('userId' in netease, 'userId 是 null 而不是省略')
})

test('命令面板：/commands/catalog 与 /commands/replies 的键能被前端直接消费', () => {
  const catalog = getJson('/commands/catalog')
  assert.equal(typeof catalog.commandsEnabled, 'boolean')
  assert.ok(Array.isArray(catalog.groups) && catalog.groups.length > 0)
  assert.equal(typeof catalog.revision, 'number')
  for (const group of catalog.groups) {
    for (const command of group.commands) {
      for (const key of ['name', 'id', 'prefix', 'authorities', 'editable', 'minimumRole', 'minimumFloor']) {
        assert.ok(key in command, `命令目录项缺 ${key}`)
      }
    }
  }
  assert.ok(catalog.songPlatforms.some((item) => item.value === catalog.defaultSongSource))

  const replies = getJson('/commands/replies')
  assert.ok(Array.isArray(replies.commands) && Array.isArray(replies.variables))
  assert.equal(typeof replies.limits.maxReplyChars, 'number')
  for (const entry of replies.commands) {
    assert.ok(Array.isArray(entry.replies) && entry.replies.length > 0)
    for (const row of entry.replies) {
      assert.equal(typeof row.key, 'string')
      assert.equal(typeof row.text, 'string')
    }
  }
})

test('AI 面板：模型/追踪/记忆三条链路的字段名与后端一致', () => {
  const models = getJson('/ai/models', 'POST', {})
  assert.ok(Array.isArray(models.models) && models.models.every((item) => typeof item === 'string'))

  const trace = getJson('/ai/trace')
  assert.equal(typeof trace.capacity, 'number')
  assert.ok(Array.isArray(trace.entries) && trace.entries.length > 0)
  assert.equal(typeof trace.stats.requests, 'number')
  assert.ok('cacheHitRatio' in trace.stats, 'cacheHitRatio 可为 null，但键必须存在')
  const entry = getJson('/ai/trace/entry?id=12')
  for (const key of ['id', 'wireApi', 'messages', 'tools', 'parameters', 'usage', 'promptTokens']) {
    assert.ok(key in entry, `/ai/trace/entry 缺 ${key}`)
  }

  const actors = getJson('/ai/memory/actors')
  assert.ok(Array.isArray(actors.actors) && actors.actors.length > 0)
  for (const actor of actors.actors) {
    for (const key of ['key', 'actor', 'facts', 'turns', 'tokens']) assert.ok(key in actor)
  }
  const hall = getJson('/ai/memory/hall')
  for (const key of ['summary', 'lines', 'count', 'tokens']) assert.ok(key in hall)
  for (const line of hall.lines) {
    assert.ok(['player', 'bot'].includes(line.role), 'EmojiCatalog/AiMemoryPanel 依赖 role 枚举')
  }
  const detail = getJson('/ai/memory/actor?name=旅行者')
  assert.equal(detail.found, true)
  for (const key of ['key', 'actor', 'facts', 'summary', 'turns']) assert.ok(key in detail)

  const vision = getJson('/ai/vision/status', 'POST', {})
  for (const key of ['enabled', 'disabled', 'supports_image', 'capability', 'source', 'capture_scope']) {
    assert.ok(key in vision, `/ai/vision/status 缺 ${key}`)
  }
  const tts = getJson('/tts/models', 'POST', {})
  assert.ok(Array.isArray(tts.models) && Array.isArray(tts.voices))
  assert.ok(['api', 'fallback'].includes(tts.source))
})

test('表情目录：allowed 的 pack 是数字，catalog 是预格式化字符串', () => {
  const payload = getJson('/emoji/catalog')
  assert.equal(payload.ok, true)
  for (const item of payload.allowed) {
    assert.equal(typeof item.pack, 'number')
    assert.equal(typeof item.text, 'string')
  }
  assert.equal(typeof payload.catalog, 'string')
  // EmojiCatalog.vue 也接受 scanned 目录 {tabs:[{pack,labels}]}。
  const scanned = getJson('/emoji/scanned')
  for (const tab of scanned.tabs) {
    assert.equal(typeof tab.pack, 'number')
    assert.ok(Array.isArray(tab.labels))
  }
})

test('娱乐玩法：/turtle-soup 与 /undercover 的 phase 是字符串枚举', () => {
  const soup = getJson('/turtle-soup')
  assert.ok(['idle', 'announcing', 'active', 'settling'].includes(soup.phase))
  for (const key of ['enabled', 'phaseLabel', 'elapsedSeconds', 'questionCount', 'pendingAi', 'remainingPuzzles', 'recentJudgments']) {
    assert.ok(key in soup, `/turtle-soup 缺 ${key}`)
  }
  for (const judgment of soup.recentJudgments) {
    assert.equal(judgment.question, '', '/monitor 副本按后端约定抹掉 question')
  }
  const spy = getJson('/undercover')
  assert.ok(['idle', 'lobby', 'delivering', 'describing', 'voting', 'runoff_describing', 'runoff_voting'].includes(spy.phase))
  for (const player of spy.players) {
    assert.equal(typeof player.name, 'string')
    assert.equal(typeof player.alive, 'boolean')
  }
})

test('调试沙盒：只读模式与状态路由的形状（ToolsPage/DebugPage 直接渲染整包）', () => {
  const mode = getJson('/debug/read-only/mode')
  assert.equal(mode.ok, true)
  assert.equal(typeof mode.mode.enabled, 'boolean')
  assert.equal(typeof mode.mode.state, 'string')
  for (const key of ['instance', 'revision', 'latestSequence', 'memory', 'limits', 'loss']) {
    assert.ok(key in mode.mode, `/debug/read-only/mode 缺 ${key}`)
  }
  const summary = getJson('/debug/read-only/summary')
  assert.equal(summary.ok, true)
  assert.ok('ui' in summary.summary && 'queue' in summary.summary)
  // 真实路由把分页放在 entries.events 下（ToolsPage 目前读不到，属于既有 bug，
  // 但假数据必须与后端一致，否则这个 bug 会被掩盖成「没数据」）。
  const entries = getJson('/debug/read-only/entries?limit=20')
  assert.ok(Array.isArray(entries.entries.events))
  const state = getJson('/debug/state')
  assert.ok(Array.isArray(state.commands) && state.commands.includes('capture'))
  const tasks = getJson('/debug/tasks')
  assert.ok(Array.isArray(tasks.tasks))
  for (const task of tasks.tasks) {
    for (const key of ['id', 'version', 'description', 'commands', 'captureAfterEach']) assert.ok(key in task)
  }
})

test('配置写路径：/config/validate 通过、/config/save 与回滚回执同形', () => {
  const validation = getJson('/config/validate', 'POST', { sections: {} })
  assert.equal(validation.ok, true)
  assert.deepEqual(validation.errors, [])
  for (const path of ['/config/save', '/config/rollback']) {
    const receipt = getJson(path, 'POST', { baseRevision: 42, revision: 41, sections: {} })
    assert.equal(receipt.ok, true)
    assert.equal(typeof receipt.revision, 'number')
    for (const key of ['changedFields', 'restartRequired', 'reloadScheduled', 'appliedLiveFields']) {
      assert.ok(key in receipt, `${path} 缺 ${key}`)
    }
  }
  // saveSections() 会先读 validation.errors，再读 response.message / ok。
  const commands = getJson('/commands/replies', 'POST', { baseRevision: 42, commands: [] })
  assert.equal(commands.ok, true)
})

test('视觉档案：VisualProfile 是 snake_case，且 profile 能交给 VisualEditor 渲染', () => {
  const payload = getJson('/config/visual')
  assert.equal(payload.ok, true)
  assert.equal(typeof payload.revision, 'number')
  const profile = payload.profile
  // src/config/visual.rs 的 VisualProfile 没有 rename_all，键就是 snake_case。
  for (const key of ['name', 'schema_version', 'canvas', 'templates', 'thresholds', 'regions', 'points', 'chat_geometry', 'capture', 'matching']) {
    assert.ok(key in profile, `/config/visual 的 profile 缺 ${key}`)
  }
  assert.equal(typeof profile.canvas.width, 'number')
  assert.equal(typeof profile.canvas.height, 'number')
  for (const key of ['templates', 'thresholds', 'regions', 'points', 'chat_geometry', 'capture', 'matching']) {
    assert.equal(typeof profile[key], 'object', `${key} 必须是对象（空对象也不能是 null）`)
  }
  for (const [name, path] of Object.entries(profile.templates)) {
    assert.equal(typeof path, 'string', `templates.${name} 必须是路径字符串`)
  }
  for (const [name, region] of Object.entries(profile.regions)) {
    assert.ok(region && ['x', 'y', 'width', 'height'].every((axis) => typeof region[axis] === 'number'), `regions.${name} 必须是矩形`)
  }
  assert.ok(Array.isArray(payload.pendingReloadFields))
  assert.ok(Array.isArray(payload.profileDifferences))
  assert.ok(payload.defaultProfile && payload.effectiveProfile)
})

test('视觉档案的组键清单与 src/config/visual.rs 逐条一致', () => {
  // 只断言「有 profile」不够：VisualEditor 是遍历这些 map 渲染输入框的，
  // 少一个键就少一个输入框，而页面看起来仍然完全正常——最容易被漏掉的一类。
  const source = readFileSync(resolve(ROOT, 'src/config/visual.rs'), 'utf8')
  const body = source.slice(source.indexOf('fn fields()'), source.indexOf('pub fn managed_paths()'))
  // 取 fields() 里每个 `for key in [...` 的分组清单（模板 / 区域 / 聊天几何）。
  const groups = []
  for (const match of body.matchAll(/for key in \[/g)) {
    const start = body.indexOf('[', match.index)
    let depth = 0
    let index = start
    while (index < body.length) {
      if (body[index] === '[') depth += 1
      else if (body[index] === ']') {
        depth -= 1
        if (depth === 0) break
      }
      index += 1
    }
    groups.push([...body.slice(start, index).matchAll(/"([^"]+)"/g)].map((item) => item[1]))
  }
  assert.ok(groups.length >= 3, '没能从 visual.rs 解析出分组清单')

  const profile = getJson('/config/visual').profile
  const templateKeys = groups.find((group) => group.includes('blue_marker'))
  assert.ok(templateKeys, '模板键清单解析失败')
  for (const key of templateKeys) {
    assert.ok(key in profile.templates, `视觉档案缺模板 ${key}（VisualEditor 会少一行输入框）`)
  }
  // 两个 startup.* 模板键带前缀，单独钉住。
  assert.ok('startup.paimon_menu' in profile.templates)
  assert.ok('startup.wonderland_map_star' in profile.templates)

  const regionKeys = groups.find((group) => group.includes('hall_member_list'))
  assert.ok(regionKeys, '区域键清单解析失败')
  for (const key of regionKeys) {
    assert.ok(key in profile.regions, `视觉档案缺区域 ${key}`)
  }

  const geometryKeys = groups.find((group) => group.includes('marker_search_width'))
  assert.ok(geometryKeys, '聊天几何键清单解析失败')
  for (const key of geometryKeys) {
    assert.ok(key in profile.chat_geometry, `视觉档案缺聊天几何 ${key}`)
  }

  const pointKeys = [...body.matchAll(/\(\s*"([^"]+)"\s*,\s*"(\w+\.\w+)"\s*\)\s*,/g)].map((item) => item[1])
  for (const key of pointKeys.filter((value) => value.includes('.'))) {
    assert.ok(key in profile.points, `视觉档案缺点位 ${key}`)
  }
})

test('工具模板：region 允许为 null（自定义流程模板没有区域）', () => {
  const templates = getJson('/tools/templates')
  for (const item of templates) {
    assert.equal(typeof item.name, 'string')
    assert.equal(typeof item.threshold, 'number')
    assert.ok('region' in item, 'region 键必须存在（可为 null）')
    if (item.region) {
      for (const axis of ['x', 'y', 'width', 'height']) assert.equal(typeof item.region[axis], 'number')
    }
  }
  assert.ok(templates.some((item) => item.region === null), '需要一条 region=null 的样本')
})

test('查询串解析与后端 query_value 一致（重复键取最后一个）', () => {
  assert.deepEqual(parseQuery('?a=1&b=%E6%99%B4%E5%A4%A9'), { a: '1', b: '晴天' })
  assert.deepEqual(parseQuery('a=1&a=2'), { a: '2' })
  assert.deepEqual(parseQuery(''), {})
  // 非法编码不能让整条请求炸掉：退回原文，与 api.js 的 decodeCookieValue 同一取舍。
  assert.deepEqual(parseQuery('?x=%E0%A4%A'), { x: '%E0%A4%A' })
})

test('未覆盖的路由必须显式失败，而不是静默返回空对象', () => {
  // 静默返回 {} 会让页面显示「暂无数据」，看起来像后端真的没数据；
  // 显式 404 才能让核验者立刻发现「假数据漏了这条路由」。
  assert.equal(mockResponse('/definitely-not-a-route'), null)
  assert.equal(mockResponse('/tools/not-covered'), null)
})

test('web/src 实际调用的每条路由都有假数据（覆盖范围以扫描结果为准）', () => {
  // 参照物不是手写清单，而是 scripts/panel-api-coverage.py 同一个正则扫出来的
  // 前端调用集合：新增一处 getJson('/x') 而忘了加假数据时，这里直接红，
  // 而不是等核验时看到「正在连接后端…」才发现。
  const pattern = /[`'"](\/[A-Za-z][A-Za-z0-9_/-]*)[`'"?#$}]/g
  const roots = [resolve(ROOT, 'web/src')]
  const missing = []
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const path = resolve(dir, entry.name)
      if (entry.isDirectory()) {
        if (entry.name !== 'mock') walk(path)
        continue
      }
      if (!/\.(vue|js)$/.test(entry.name) || /\.test\.mjs$/.test(entry.name)) continue
      const text = readFileSync(path, 'utf8')
      for (const match of text.matchAll(pattern)) {
        const route = match[1]
        // 假数据覆盖判定用 POST：POST 分支最全（GET 分支是它的子集）。
        if (!mockResponse(route, 'POST')) missing.push(`${route}（${relative(ROOT, path)}）`)
      }
    }
  }
  for (const root of roots) walk(root)
  assert.deepEqual([...new Set(missing)], [], `以下前端调用的路由没有假数据：${missing}`)
})

test('玩法状态快照的字段名与 Rust 侧一致（改名字这里先红）', () => {
  // 这些字段名不是随便起的：面板直接按名取值，写错一个字段的代价是
  // 「页面不报错、但那格永远是空/永远是默认值」——纯看截图很难发现。
  // 例：数字炸弹一度按 `phaseLabel` 取值，而后端只给 `phase`，
  // 于是无论是否在玩都显示「空闲」。
  const phaseIn = (allowed) => (value) => assert.ok(allowed.includes(value.phase), `未知 phase: ${value.phase}`)

  // 卧底在 32c960c 扩了 7 个字段（规则/开局者/存活数/阶段提示/阶段进度/出局名单），
  // 假数据漏同步时面板会退回占位符——字段集合这里钉死。
  const spy = JSON.parse(mockResponse('/undercover').body)
  assert.deepEqual(Object.keys(spy).sort(), [
    'aliveCount', 'completed', 'creator', 'eliminated', 'enabled', 'mode', 'modeHint',
    'phase', 'phaseCompleted', 'phaseHint', 'phaseTotal', 'players', 'remainingSeconds',
    'round', 'total',
  ])
  phaseIn(['idle', 'lobby', 'delivering', 'describing', 'voting', 'runoff_describing', 'runoff_voting'])(spy)
  // 身份红线：玩家快照只有位置/展示名/存活，没有角色。
  for (const player of spy.players) {
    assert.deepEqual(Object.keys(player).sort(), ['alive', 'name', 'position'])
  }

  const idiom = JSON.parse(mockResponse('/idiom-chain').body)
  assert.deepEqual(Object.keys(idiom).sort(), [
    'enabled', 'expected_first', 'history', 'lexicon_len', 'mode', 'phase', 'starter', 'total',
  ])
  phaseIn(['idle', 'active'])(idiom)

  const bomb = JSON.parse(mockResponse('/number-bomb').body)
  assert.deepEqual(Object.keys(bomb).sort(), [
    'elapsed_seconds', 'enabled', 'guessed', 'last_result', 'lower', 'phase',
    'players', 'turn', 'turn_timeout_seconds', 'upper',
  ])
  phaseIn(['idle', 'lobby', 'playing'])(bomb)
  // 身份红线：只给展示名，不给内部比较键。
  for (const player of bomb.players) assert.deepEqual(Object.keys(player), ['display'])

  const card = JSON.parse(mockResponse('/card-game').body)
  assert.deepEqual(Object.keys(card).sort(), [
    'current', 'elapsed_seconds', 'enabled', 'landlord', 'lobby_timeout_seconds',
    'phase', 'players', 'turn_timeout_seconds', 'variant',
  ])
  phaseIn(['idle', 'lobby', 'bidding', 'playing'])(card)
  // 手牌只给张数不给牌面：读面板等于作弊，这里钉住字段集合。
  for (const player of card.players) {
    assert.deepEqual(Object.keys(player).sort(), ['hand_count', 'name', 'timeouts', 'trustee'])
  }
})

test('MOCK_ROUTES 与实际分派表一致（防止清单腐烂成盲区）', () => {
  const covered = []
  for (const path of MOCK_ROUTES.json) {
    assert.ok(mockResponse(path), `MOCK_ROUTES 声明了 ${path}，但分派里没有`)
    covered.push(path)
  }
  for (const path of MOCK_ROUTES.text) {
    const resolved = mockResponse(path)
    assert.ok(resolved, `MOCK_ROUTES 声明了 ${path}，但分派里没有`)
    assert.equal(resolved.contentType, 'text/plain; charset=utf-8', `${path} 应声明在 text 列表里`)
  }
  assert.ok(covered.length > 30, '覆盖的路由数太少')
})
