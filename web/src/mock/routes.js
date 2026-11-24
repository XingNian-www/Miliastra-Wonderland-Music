// 面板假数据：非 schema 路由的响应体。
//
// 为什么手写而不是生成：只有 `/config` 与 `/config/schema` 的形状由 Rust 的
// 配置 schema 决定（399 个字段，见 scripts/panel-mock-gen.py 与
// fixtures.config.json）；其余路由是各 handler 里 `json!({...})` 或 struct
// 序列化的结果，数量多但每个只用几个字段，手写更清楚，也方便把「一条看起来
// 合理的样本」写成断言素材。
//
// 形状来源（每个工厂上方都标了定义处）：
//   src/interfaces/http/routes/{system,playback,ai,commands,debug,entertainment,operator}.rs
//   src/runtime/monitor.rs、src/runtime/scheduler.rs、src/features/playback/mod.rs
//   crates/miliastra-playback/src/{model,catalog,cache}*.rs
//
// 约定：字段名一律与真实响应逐字一致（camelCase 与否按后端各自的 serde 配置），
// 缺失即「后端会省略该键」的字段保持省略，而不是填 null——两者在前端是不同的
// 分支（例如 `/status` 的 currentTrack 省略时页面显示「空闲」）。
// 这里只造假数据，不判断登录态、不模拟失败分支。

/** 列表里出现的长中文样本统一放这里，避免工厂里散落魔法字符串。 */
const DEMO = {
  player: '旅行者',
  friend: '小派蒙',
  hall: '千星大厅·测试',
  title: '晴天',
  artist: '周杰伦',
  album: '叶惠美',
  other: '夜曲',
  otherArtist: '周杰伦',
}

/** GET /status —— PlayerStatus（src/features/playback/mod.rs）。 */
export function playerStatus() {
  return {
    status: 'playing',
    currentTrack: {
      trackRef: { key: { provider: 'qqmusic', id: '0039MnYb0qxYhV' }, resolverLocator: 'qqmusic:v2:0039MnYb0qxYhV' },
      metadata: { title: DEMO.title, artists: [DEMO.artist], album: DEMO.album, durationMs: 269000 },
    },
    currentUri: 'qqmusic:0039MnYb0qxYhV',
    name: DEMO.title,
    singer: DEMO.artist,
    albumName: DEMO.album,
    lyricLineText: '故事的小黄花 从出生那年就飘着',
    duration: 269,
    progress: 96.4,
    playbackRate: 44.1,
    volume: 62,
    requester: DEMO.player,
    runtimeIdentity: 'web-panel',
    sessionId: 'mock-session-0001',
    generation: 7,
    endBehavior: 'advance',
    lastEndCause: '',
    failureCode: '',
    failureMessage: '',
    failureRetryable: false,
    failureProvider: '',
    failureRetryAfterMs: 0,
  }
}

/** GET /monitor —— MonitorSnapshot（src/runtime/monitor.rs）。 */
export function monitor() {
  return {
    logs: [
      '[12:04:11] 大厅扫描完成：识别到 3 条聊天',
      `[12:04:12] 收到点歌请求：${DEMO.title}`,
      `[12:04:12] 已排队执行 @点歌 ${DEMO.title}`,
      '[12:04:15] 播放器状态：playing',
      `[12:04:18] ${DEMO.friend} 触发了 AI 聊天，已生成回复`,
    ],
    ocr: {
      markers: 3,
      messages: [`${DEMO.friend}：今天听点什么`, `${DEMO.player}：来首晴天`, `${DEMO.friend}：好耶`],
      markerMs: 12,
      ocrMs: 148,
      totalMs: 160,
      source: 'chat',
      capturedAtMs: 1795000000000,
    },
    queue: [
      { id: 1, keyword: DEMO.title, source: 'qqmusic', preferAccompaniment: false, friendUsername: '', requester: DEMO.player },
      { id: 2, keyword: DEMO.other, source: 'netease', preferAccompaniment: false, friendUsername: DEMO.friend, requester: DEMO.friend },
    ],
    commands: [`@点歌 ${DEMO.title}`, '@状态', `@点歌 ${DEMO.other}`],
    status: '运行中',
    playbackController: {
      state: 'playing',
      pauseReason: 'none',
      activeKeyword: DEMO.title,
      activeUri: 'qqmusic:0039MnYb0qxYhV',
      lastObservationReliability: 'reliable',
      backendStatus: 'playing',
      currentUri: 'qqmusic:0039MnYb0qxYhV',
      title: DEMO.title,
      artist: DEMO.artist,
      requester: DEMO.player,
      progress: 96.4,
      duration: 269,
      observedAtMs: 1795000000000,
    },
    chatListener: {
      mode: '一级值守',
      pendingMode: '',
      temporaryPrimary: false,
      initialUnreadClear: false,
      unreadTaskPending: false,
      hallRoundRequired: false,
    },
    operational: {
      uiState: 'hall',
      scannerPaused: false,
      commandsEnabled: true,
      idleExitRemainingSeconds: 1800,
      hallRemainingMinutes: 42,
      hallRemainingUpdatedAt: 1794999900,
    },
    uiRoutine: null,
    pendingTasks: [],
    webTools: [
      { id: 21, label: 'OCR 全屏', status: 'completed', result: '识别到 3 条聊天' },
      { id: 22, label: '扫描聊天', status: 'running', result: null },
    ],
    tasks: [
      {
        id: 7,
        label: '点歌任务',
        status: 'completed',
        queuedAtMs: 1795000000000,
        startedAtMs: 1795000001000,
        finishedAtMs: 1795000009000,
        elapsedMs: 8000,
        cancellationRequested: false,
        result: `已播放 ${DEMO.title}`,
      },
      {
        id: 8,
        label: '大厅轮询',
        status: 'queued',
        queuedAtMs: 1795000010000,
        startedAtMs: null,
        finishedAtMs: null,
        elapsedMs: 0,
        cancellationRequested: false,
        result: null,
      },
    ],
    decision: { id: 3, label: `是否切换音源：${DEMO.title}`, allowSwitchSource: true, allowAi: true, deadlineAtMs: 1795000030000 },
    turtleSoup: turtleSoup(),
    undercover: undercover(),
  }
}

/** GET /turtle-soup —— TurtleSoupSnapshot（src/features/turtle_soup/service.rs）。 */
export function turtleSoup() {
  return {
    enabled: true,
    phase: 'active',
    phaseLabel: '进行中',
    generation: 3,
    puzzleId: 'p-012',
    title: '午夜的门铃',
    surface: '深夜有人按门铃，第二天邻居死了。为什么？',
    starter: DEMO.player,
    elapsedSeconds: 128,
    participantCount: 4,
    participants: [DEMO.player, DEMO.friend, '阿贝多', '可莉'],
    questionCount: 6,
    pendingAi: 0,
    remainingPuzzles: 12,
    // /monitor 里的副本按后端约定会把 question 抹成空串（redacted_for_monitor）。
    recentJudgments: [
      { requestId: 1, player: DEMO.friend, question: '', judgment: 'yes', judgmentLabel: '是', elapsedMs: 1200, retries: 0, completedAtMs: 1795000000000 },
      { requestId: 2, player: '可莉', question: '', judgment: 'irrelevant', judgmentLabel: '无关', elapsedMs: 2100, retries: 0, completedAtMs: 1795000020000 },
    ],
    settlementIncomplete: false,
    lastError: null,
  }
}

/** GET /undercover —— UndercoverSnapshot（src/features/undercover/mod.rs）。 */
export function undercover() {
  return {
    enabled: true,
    phase: 'describing',
    mode: '单卧底',
    round: 2,
    players: [
      { position: 'A', name: DEMO.player, alive: true },
      { position: 'B', name: DEMO.friend, alive: true },
      { position: 'C', name: '阿贝多', alive: false },
    ],
    completed: 1,
    total: 9,
    remainingSeconds: 30,
    // 规则说明、开局者、存活数、阶段提示、阶段进度、出局名单：
    // 与 32c960c 扩展后的 UndercoverSnapshot 对齐；身份（谁是卧底/平民）绝不出现，
    // 开局者也只给展示名，不给已登记身份下的 UUID 比较键。
    modeHint: '1 名卧底，平民找出他即可获胜',
    creator: DEMO.player,
    aliveCount: 2,
    phaseHint: '描述阶段：1/2 人已提交',
    phaseCompleted: 1,
    phaseTotal: 2,
    eliminated: ['阿贝多'],
  }
}

/** GET /idiom-chain —— IdiomChainSnapshot（src/features/idiom_chain.rs）。 */
export function idiomChain() {
  return {
    enabled: true,
    phase: 'active',
    mode: '成语接龙',
    starter: DEMO.player,
    expected_first: '足',
    total: 7,
    history: ['画蛇添足', '足智多谋', '谋定后动', '动人心弦'],
    lexicon_len: 30892,
  }
}

/** GET /number-bomb —— NumberBombSnapshot（src/features/number_bomb.rs）。 */
export function numberBomb() {
  return {
    enabled: true,
    phase: 'playing',
    // 快照只给展示名：内部比较键（已登记身份下是 UUID）不进结构体。
    players: [{ display: DEMO.player }, { display: DEMO.friend }, { display: '阿贝多' }],
    lower: 1,
    upper: 64,
    guessed: [12, 33, 47],
    turn: 1,
    elapsed_seconds: 17,
    turn_timeout_seconds: 60,
    last_result: { loser: 2, bomb: 58 },
  }
}

/**
 * GET /card-game —— CardGameSnapshot（src/features/card_games/service.rs）。
 *
 * 一个端点同时覆盖斗地主与跑得快，靠 `variant` 区分；这里给斗地主。
 * `hand_count` 是张数而非牌面——面板拿不到具体牌，否则读面板等于作弊。
 */
export function cardGame() {
  return {
    enabled: true,
    phase: 'playing',
    variant: '斗地主',
    players: [
      { name: DEMO.player, hand_count: 17, trustee: false, timeouts: 0 },
      { name: DEMO.friend, hand_count: 17, trustee: false, timeouts: 1 },
      { name: '阿贝多', hand_count: 16, trustee: true, timeouts: 0 },
    ],
    current: 1,
    landlord: 0,
    elapsed_seconds: 42,
    lobby_timeout_seconds: 120,
    turn_timeout_seconds: 45,
  }
}

/** GET /state —— src/interfaces/http/routes/system.rs 的 state_json。 */
export function runtimeState() {
  return {
    playback: {
      state: 'requested_song_playing',
      pauseReason: 'none',
      volume: 62,
      useTranslation: true,
      activeRequest: {
        keyword: DEMO.title,
        source: 'qqmusic',
        preferAccompaniment: false,
        track: {
          trackRef: { key: { provider: 'qqmusic', id: '0039MnYb0qxYhV' }, resolverLocator: 'qqmusic:v2:0039MnYb0qxYhV' },
          metadata: { title: DEMO.title, artists: [DEMO.artist], album: DEMO.album, durationMs: 269000 },
        },
        song: DEMO.title,
        title: DEMO.title,
        artist: DEMO.artist,
        requester: DEMO.player,
        startedAtMs: 1795000000000,
      },
      lastObservation: {
        status: 'playing',
        track: {
          trackRef: { key: { provider: 'qqmusic', id: '0039MnYb0qxYhV' }, resolverLocator: 'qqmusic:v2:0039MnYb0qxYhV' },
          metadata: { title: DEMO.title, artists: [DEMO.artist], album: DEMO.album, durationMs: 269000 },
        },
        title: DEMO.title,
        artist: DEMO.artist,
        progress: 96.4,
        duration: 269,
        capturedAtMs: 1795000000000,
        reliability: 'reliable',
      },
    },
    hallRemainingMinutes: 42,
    hallRemainingUpdatedAt: 1794999900,
    hallExpiringWarningSent: false,
    hallRemainingMinutesNow: 42,
  }
}

/** GET /history —— 顶层数组（src/interfaces/http/protocol.rs 的 HistoryItem）。 */
export function history() {
  return [
    { time: '11-23 12:04:12', command: '/queue/add', query: { keyword: DEMO.title }, result: '已加入点歌队列', ok: true },
    { time: '11-23 12:03:58', command: '/playback/mode', query: { mode: 'shuffle' }, result: '{"ok":true,"mode":"shuffle"}', ok: true },
    { time: '11-23 12:02:31', command: '/chat/send', query: { text: '@状态' }, result: '已发送', ok: true },
    { time: '11-23 12:01:07', command: '/volume', query: { volume: '62' }, result: '音源未就绪', ok: false },
  ]
}

/** GET /operator/workflows —— 顶层数组（src/interfaces/http/facade.rs 的 list_workflows）。 */
export function workflows() {
  return [
    { name: '每日签到', commands: ['签到'], allowArgs: false, confirmBeforeRun: false },
    { name: '清理大厅', commands: ['清场'], allowArgs: true, confirmBeforeRun: true },
  ]
}

/** GET /queue —— 顶层数组（src/features/playback/queue.rs 的 QueueItem）。 */
export function queue() {
  return [
    {
      id: 1,
      keyword: DEMO.title,
      source: 'qqmusic',
      preferAccompaniment: false,
      aiOriginalText: '',
      track: {
        trackRef: { key: { provider: 'qqmusic', id: '0039MnYb0qxYhV' }, resolverLocator: 'qqmusic:v2:0039MnYb0qxYhV' },
        metadata: { title: DEMO.title, artists: [DEMO.artist], album: DEMO.album, durationMs: 269000 },
      },
      friendUsername: '',
      requester: DEMO.player,
      dedupBypass: false,
      candidateSnapshot: [],
      playlistAuto: false,
    },
    {
      id: 2,
      keyword: DEMO.other,
      source: 'netease',
      preferAccompaniment: true,
      aiOriginalText: '想听点安静的',
      friendUsername: DEMO.friend,
      requester: DEMO.friend,
      dedupBypass: false,
      candidateSnapshot: [],
      playlistAuto: false,
    },
  ]
}

/** GET /playback/insights —— routes/playback.rs 的 playback_insights_route。 */
export function playbackInsights() {
  return {
    history: [
      {
        keyword: DEMO.title,
        source: 'qqmusic',
        title: DEMO.title,
        artist: DEMO.artist,
        requester: DEMO.player,
        startedAtMs: 1795000000000,
        track: {
          trackRef: { key: { provider: 'qqmusic', id: '0039MnYb0qxYhV' }, resolverLocator: 'qqmusic:v2:0039MnYb0qxYhV' },
          metadata: { title: DEMO.title, artists: [DEMO.artist], album: DEMO.album, durationMs: 269000 },
        },
        cached: true,
        cacheBytes: 10485760,
        playCount: 4,
        requestedPlayCount: 3,
        poolPlayCount: 1,
        cacheHitCount: 3,
        failureCount: 0,
        lastPlayedAtMs: 1795000100000,
        lastFailureCode: null,
      },
      {
        keyword: DEMO.other,
        source: 'netease',
        title: DEMO.other,
        artist: DEMO.otherArtist,
        requester: DEMO.friend,
        startedAtMs: 1794990000000,
        track: null,
        cached: false,
        cacheBytes: null,
        playCount: 2,
        requestedPlayCount: 1,
        poolPlayCount: 1,
        cacheHitCount: 0,
        failureCount: 1,
        lastPlayedAtMs: null,
        lastFailureCode: 'decode_failure',
      },
    ],
    cache: {
      enabled: true,
      directory: 'deps/cache/audio',
      completeEntries: 12,
      completeBytes: 73400320,
      maxBytes: 21474836480,
      activeDownloads: 1,
      lyricsEntries: 5,
      lyricsBytes: 204800,
      playCount: 37,
      requestedPlayCount: 28,
      poolPlayCount: 9,
      cacheHitCount: 21,
      failureCount: 2,
    },
  }
}

/** GET /playback/leaderboard —— routes/playback.rs 的 playback_leaderboard_route。 */
export function leaderboard() {
  return {
    entries: [
      { requester: DEMO.player, count: 9, lastRequestedAtMs: 1795000000000 },
      { requester: DEMO.friend, count: 6, lastRequestedAtMs: 1794999000000 },
      { requester: '可莉', count: 2, lastRequestedAtMs: 1794990000000 },
    ],
    days: 7,
    limit: 20,
  }
}

/** GET /playback/cache/tracks —— crates/miliastra-playback/src/cache/mod.rs 的 CachedTrackPage。 */
export function cachedTracks() {
  return {
    total: 3,
    offset: 0,
    limit: 100,
    tracks: [
      {
        hash: '0f1e2d3c4b5a69788796a5b4c3d2e1f0',
        source: 'qqmusic',
        id: '0039MnYb0qxYhV',
        title: DEMO.title,
        artists: [DEMO.artist],
        album: DEMO.album,
        durationMs: 269000,
        bytes: 10485760,
        complete: true,
        cachedAtMs: 1795000200000,
        lastUsedAtMs: 1795000300000,
        playCount: 2,
        requestedPlayCount: 1,
        poolPlayCount: 1,
        cacheHitCount: 1,
        failureCount: 0,
        lastPlayedAtMs: 1795000290000,
        lastFailureCode: null,
        downloadedAtMs: 1795000200000,
      },
      {
        hash: '1a2b3c4d5e6f708192a3b4c5d6e7f809',
        source: 'netease',
        id: 'song-b',
        title: DEMO.other,
        artists: null,
        album: null,
        durationMs: null,
        bytes: 0,
        complete: false,
        cachedAtMs: 0,
        lastUsedAtMs: 0,
        playCount: 0,
        requestedPlayCount: 0,
        poolPlayCount: 0,
        cacheHitCount: 0,
        failureCount: 1,
        lastPlayedAtMs: null,
        lastFailureCode: 'decode_failure',
        downloadedAtMs: null,
      },
      {
        hash: '2b3c4d5e6f708192a3b4c5d6e7f8091a',
        source: null,
        id: null,
        title: null,
        artists: null,
        album: null,
        durationMs: null,
        bytes: 2048,
        complete: false,
        cachedAtMs: 0,
        lastUsedAtMs: 0,
        playCount: 0,
        requestedPlayCount: 0,
        poolPlayCount: 0,
        cacheHitCount: 0,
        failureCount: 0,
        lastPlayedAtMs: null,
        lastFailureCode: null,
        downloadedAtMs: null,
      },
    ],
  }
}

/** GET /player/providers —— 顶层数组（src/interfaces/http/ports.rs 的 HttpProviderView）。 */
export function playerProviders() {
  return [
    {
      provider: 'qqmusic',
      configured: true,
      fields: { accessToken: true, musicKey: true, openId: true, refreshKey: false, refreshToken: true, uin: true },
      refreshSupported: true,
      manualRefreshSupported: true,
      refreshReady: true,
      refreshState: 'idle',
      lastRefreshAtMs: 1795000000000,
      nextRefreshCheckAtMs: 1795003600000,
      lastRefreshError: null,
    },
    {
      provider: 'netease',
      configured: true,
      fields: { csrf: true, musicU: true },
      refreshSupported: true,
      manualRefreshSupported: true,
      refreshReady: true,
      refreshState: 'refreshing',
      lastRefreshAtMs: 1794990000000,
      nextRefreshCheckAtMs: null,
      lastRefreshError: null,
    },
    {
      provider: 'bilibili',
      configured: false,
      fields: { buvid3: false, csrf: false, refreshToken: false, sessdata: false, userId: false },
      refreshSupported: true,
      manualRefreshSupported: false,
      refreshReady: false,
      refreshState: 'unavailable',
      lastRefreshAtMs: null,
      nextRefreshCheckAtMs: null,
      lastRefreshError: '未配置任何凭据字段',
    },
    {
      provider: 'kugou',
      configured: true,
      fields: { dfid: true, token: true, userId: true },
      refreshSupported: false,
      manualRefreshSupported: false,
      refreshState: 'idle',
      refreshReady: false,
      lastRefreshAtMs: null,
      nextRefreshCheckAtMs: null,
      lastRefreshError: '手动刷新失败：登录态已过期',
    },
  ]
}

/** GET /player/login/status —— HttpLoginStatus（src/interfaces/http/ports.rs）。 */
export function loginStatus() {
  return {
    active: true,
    sessionId: '7c9e6679-7425-40de-944b-e07fc1f90ae7',
    provider: 'qqmusic',
    lastError: { code: 'provider_auth_required', message: '需要重新扫码登录', provider: 'qqmusic' },
  }
}

/** GET /player/kugou/status —— KugouAccountStatus（crates/miliastra-playback/src/catalog/kugou.rs）。 */
export function kugouStatus() {
  return {
    loggedIn: true,
    userId: '123456789',
    nickname: '听歌的旅行者',
    vip: true,
    vipType: 'svip',
    vipExpireAtMs: 1796000000000,
    listenReportAvailable: true,
  }
}

/** GET /player/account/status —— ProviderAccountStatus（crates/miliastra-playback/src/catalog/mod.rs）。 */
export function accountStatus(provider) {
  const table = {
    qqmusic: { loggedIn: true, userId: '12345678', nickname: DEMO.player, vip: true, vipType: 'vip', loginMethod: 'qq' },
    netease: { loggedIn: true, userId: '87654321', nickname: DEMO.friend, vip: false, vipType: null, loginMethod: null },
    bilibili: { loggedIn: false, userId: null, nickname: null, vip: false, vipType: null, loginMethod: null },
    kugou: { loggedIn: true, userId: '123456789', nickname: '听歌的旅行者', vip: true, vipType: 'svip', loginMethod: null },
  }
  const row = table[provider] || table.qqmusic
  return {
    provider,
    loggedIn: row.loggedIn,
    userId: row.userId,
    nickname: row.nickname,
    vipKnown: provider !== 'bilibili',
    vip: row.vip,
    vipType: row.vipType,
    vipExpireAtMs: row.vip ? 1796000000000 : null,
    ...(row.loginMethod ? { loginMethod: row.loginMethod } : {}),
    checkedAtMs: 1795000300000,
    stale: false,
  }
}

/** POST /ai/models、GET /ai/models —— routes/ai.rs 的 ai_models_route。 */
export function aiModels() {
  return { models: ['gpt-5.6-mini', 'gpt-5.6', 'deepseek-chat', 'deepseek-reasoner'] }
}

/** GET /ai/vision/status —— routes/ai.rs 的 vision_status_payload。 */
export function aiVisionStatus() {
  return {
    enabled: true,
    disabled: false,
    model: 'gpt-5.6-mini',
    supports_image: true,
    capability: 'supported',
    source: 'api',
    reason: '模型 API 声明支持图片',
    capture_scope: 'primary_screen',
    allow_full_screen: false,
  }
}

/** GET /tts/models —— crates 里的 TtsCatalog（src/features/tts.rs）。 */
export function ttsCatalog() {
  return {
    models: ['s1', 's2-pro', 's2.1-pro', 's2.1-pro-free', 'drama-3'],
    voices: [
      { id: 'voice_demo_a', title: '温和女声', state: 'enabled', visibility: 'public' },
      { id: 'voice_demo_b', title: '低沉男声', state: 'enabled', visibility: 'private' },
    ],
    source: 'api',
    modelSource: 'fallback',
    truncated: false,
  }
}

/** GET /ai/trace —— src/ai_trace.rs 的 list_json。 */
export function aiTrace() {
  const entries = [
    {
      id: 12,
      startedAt: '11-23 12:03:44',
      startedAtMs: 1795000000000,
      wireApi: 'responses',
      model: 'gpt-5.6-mini',
      reasoning: null,
      ok: true,
      durationMs: 812,
      promptTokens: 1200,
      completionTokens: 210,
      cachedTokens: 890,
      cacheHitRatio: 0.7417,
      prefixFingerprint: '9f2c1a0b77d31e55',
      stablePrefixBytes: 5120,
      requestBytes: 7401,
      messageCount: 6,
      tailTokens: 180,
      error: null,
    },
    {
      id: 11,
      startedAt: '11-23 12:01:02',
      startedAtMs: 1794999000000,
      wireApi: 'chat_completions',
      model: 'deepseek-chat',
      reasoning: 'low',
      ok: false,
      durationMs: 30012,
      promptTokens: null,
      completionTokens: null,
      cachedTokens: null,
      cacheHitRatio: null,
      prefixFingerprint: '11aa22bb33cc44dd',
      stablePrefixBytes: 0,
      requestBytes: 2048,
      messageCount: 3,
      tailTokens: 120,
      error: 'upstream timeout',
    },
  ]
  return {
    capacity: 24,
    count: entries.length,
    entries,
    stats: {
      requests: 2,
      promptTokens: 1200,
      cachedTokens: 890,
      cacheHitRatio: 0.7417,
      cacheMeasuredRequests: 1,
      distinctPrefixes: 2,
    },
  }
}

/** GET /ai/trace/entry —— src/ai_trace.rs 的 AiTraceEntry。 */
export function aiTraceEntry(id) {
  return {
    id: Number(id) || 12,
    startedAtMs: 1795000000000,
    startedAt: '11-23 12:03:44',
    wireApi: 'responses',
    model: 'gpt-5.6-mini',
    reasoning: null,
    durationMs: 812,
    ok: true,
    error: null,
    system: '你是一个游戏大厅里的聊天伙伴。',
    messages: [
      { role: 'user', content: `${DEMO.friend}：今天听点什么` },
      { role: 'assistant', content: '来首晴天怎么样？' },
    ],
    tools: [],
    parameters: { temperature: 0.7, max_tokens: 1024, stream: true, store: false },
    usage: { prompt_tokens: 1200, completion_tokens: 210, prompt_tokens_details: { cached_tokens: 890 } },
    promptTokens: 1200,
    completionTokens: 210,
    cachedTokens: 890,
    cacheHitRatio: 0.7417,
    prefixFingerprint: '9f2c1a0b77d31e55',
    stablePrefixBytes: 5120,
    requestBytes: 7401,
    messageCount: 2,
    tailTokens: 180,
  }
}

/** GET /ai/memory/actors —— src/interfaces/http/facade.rs 的 memory_actors。 */
export function aiMemoryActors() {
  return {
    actors: [
      { key: 'actor-uuid-0001', actor: DEMO.player, facts: 12, turns: 34, tokens: 5678 },
      { key: 'actor-uuid-0002', actor: DEMO.friend, facts: 4, turns: 9, tokens: 1200 },
    ],
  }
}

/** GET /ai/memory/hall —— src/interfaces/http/facade.rs 的 memory_hall。 */
export function aiMemoryHall() {
  return {
    summary: '大厅最近主要在聊点歌与每日签到，气氛轻松。',
    lines: [
      { speaker: DEMO.player, text: '今天听点什么', role: 'player' },
      { speaker: 'BOT', text: '来首晴天怎么样？', role: 'bot' },
      { speaker: DEMO.friend, text: '好耶', role: 'player' },
    ],
    count: 128,
    tokens: 5230,
  }
}

/** GET /ai/memory/actor —— src/interfaces/http/facade.rs 的 memory_actor。 */
export function aiMemoryActor(name) {
  return {
    key: name || 'actor-uuid-0001',
    actor: name === DEMO.friend ? DEMO.friend : DEMO.player,
    found: true,
    facts: ['喜欢周杰伦', '常在工作日晚上出现'],
    summary: '近期主要聊音乐与游戏活动。',
    turns: [{ asker: DEMO.player, question: '今天听点什么', answer: '来首晴天怎么样？' }],
  }
}

/** POST /ai/chat/preview —— 供 AI 配置页的提示词预览渲染。 */
export function aiChatPreview() {
  return {
    warnings: [],
    totals: { systemChars: 420, historyChars: 180, tailChars: 90, totalChars: 690 },
    blocks: [
      { name: '系统提示词', chars: 420 },
      { name: '个人历史', chars: 180 },
      { name: '本轮输入', chars: 90 },
    ],
    system: '你是一个游戏大厅里的聊天伙伴。\n只返回合法 json 对象。',
    history: `${DEMO.friend}：今天听点什么`,
    tail: '参照记忆回答，保持简短。',
  }
}

/** GET /emoji/catalog —— routes/config.rs 的 emoji_catalog_route。 */
export function emojiCatalog() {
  return {
    ok: true,
    enabled: true,
    aiEnabled: true,
    allowed: [
      { pack: 1, text: '哈哈' },
      { pack: 1, text: '微笑' },
      { pack: 2, text: '哭' },
    ],
    catalog: '可用表情（只能使用这些标签，且不要改写）：{表情包:1:哈哈}、{表情包:1:微笑}、{表情包:2:哭}',
  }
}

/** GET /emoji/scanned —— deps/data/emoji-catalog.json 的内容。 */
export function emojiScanned() {
  return {
    ok: true,
    scannedAtMs: 1795000000000,
    tabs: [
      { pack: 1, labels: ['哈哈', '微笑'] },
      { pack: 2, labels: ['哭'] },
    ],
    entries: ['1|哈哈', '1|微笑', '2|哭'],
  }
}

/** GET /commands/catalog —— src/features/command_catalog.rs 的 catalog_view。 */
export function commandCatalog() {
  const command = (name, id, prefix, authorities, summary, extra = {}) => ({
    name,
    aliases: [],
    prefix,
    authorities,
    summary,
    usage: [`${prefix}${name}`],
    examples: [`${prefix}${name} 示例`],
    openEnded: false,
    hidden: false,
    enabled: true,
    id,
    minimumRole: 'user',
    editable: true,
    minimumFloor: 'user',
    lockedReason: null,
    ...extra,
  })
  return {
    commandsEnabled: true,
    groups: [
      {
        name: '点歌',
        commands: [
          command('点歌', '@QQ点歌', '@', ['hall'], '点一首歌'),
          command('B站点歌', '@B站点歌', '@', ['hall', 'friend'], '从 B 站搜索并点歌'),
        ],
      },
      {
        name: '播放',
        commands: [command('播放', '@播放', '@', ['hall'], '继续播放'), command('暂停', '@暂停', '@', ['hall'], '暂停播放')],
      },
      {
        name: '娱乐',
        commands: [
          command('海龟汤', '@海龟汤', '@', ['hall'], '开始一局海龟汤'),
          command('谁是卧底', '@谁是卧底', '@', ['hall'], '开始谁是卧底', { hidden: true }),
        ],
      },
      {
        name: '管理',
        commands: [command('清场', '@清场', '@', ['hall'], '清理聊天记录', { minimumRole: 'admin', minimumFloor: 'admin' })],
      },
    ],
    revision: 42,
    defaultSongSource: 'qqmusic',
    defaultSongSourceSelectable: true,
    songPlatforms: [
      { value: 'qqmusic', label: 'QQ音乐' },
      { value: 'netease', label: '网易云' },
      { value: 'kugou', label: '酷狗' },
    ],
  }
}

/** GET /commands/replies —— routes/commands.rs 的 commands_replies_route。 */
export function commandReplies() {
  const row = (key, text) => ({ key, text, defaultText: text, editable: true, active: true })
  return {
    revision: 42,
    commands: [
      {
        id: '@QQ点歌',
        name: '点歌',
        category: '点歌',
        replies: [
          row('song.request.queued', '已点歌 {歌名}'),
          row('song.request.duplicate', '这首歌已经在队列里了'),
          row('song.request.failed', '点歌失败：{原因}'),
        ],
        defaultReplies: [row('song.request.queued', '已点歌 {歌名}')],
        allowedVariables: ['歌名', '玩家', 'AI回复'],
      },
      {
        id: '@播放',
        name: '播放',
        category: '播放',
        replies: [row('playback.resumed', '继续播放')],
        defaultReplies: [row('playback.resumed', '继续播放')],
        allowedVariables: ['玩家'],
      },
    ],
    variables: [
      { name: '歌名', label: '歌名', example: DEMO.title },
      { name: '玩家', label: '玩家', example: DEMO.player },
      { name: 'AI回复', label: 'AI 回复', example: '来首晴天怎么样？', global: true },
    ],
    limits: { maxReplyChars: 300, maxRows: 65 },
    aliases: [{ id: '@搜索', owner: '@QQ点歌' }],
    customKeys: [{ key: 'custom.workflow.greeting', namespace: 'workflow', text: '你好' }],
    customKeyRules: {
      prefix: 'custom.',
      maxChars: 96,
      maxNamespaceChars: 32,
      pattern: 'custom.<命名空间>.<名字>：命名空间只允许小写字母/数字/下划线，名字还允许点；3–5 段',
    },
  }
}

/** GET /config/revisions —— routes/config.rs 的 config_revisions_route。 */
export function configRevisions() {
  return {
    ok: true,
    revisions: [
      { revision: 42, createdAtMs: 1795000000000 },
      { revision: 41, createdAtMs: 1794900000000 },
      { revision: 40, createdAtMs: 1794800000000 },
    ],
  }
}

/** GET /debug/read-only/mode —— src/debug/read_only 的 describe()。 */
export function readOnlyMode() {
  return {
    ok: true,
    mode: {
      enabled: true,
      state: 'active',
      instance: '00000000-0000-4000-8000-000000000001',
      revision: 12,
      schemaVersion: 1,
      appVersion: 'mock',
      latestSequence: 340,
      memory: { frames: 0, frameBytes: 0, entries: 4, entryBytes: 9000, evidenceImages: 1, evidenceImageBytes: 204800 },
      limits: {
        frames: 8,
        frameBytes: 67108864,
        entries: 256,
        entryBytes: 16777216,
        evidenceImages: 8,
        evidenceImageBytes: 33554432,
        statuses: 1000,
        statusBytesEach: 16384,
        history: 32,
      },
      droppedFrames: 0,
      droppedEntries: 0,
      rejectedFrames: 0,
      droppedImages: 0,
      loss: {
        metadataEvicted: 0,
        unfinishedMetadataEvicted: 0,
        imageEvicted: 0,
        recent: [],
        droppedFormalStatuses: 0,
        preReceiptStatusesOmitted: 0,
        oversizedStatuses: 0,
      },
    },
  }
}

/** GET /debug/read-only/summary —— src/debug_http_service.rs 的 read_only_summary。 */
export function readOnlySummary() {
  return {
    ok: true,
    summary: {
      instance: '00000000-0000-4000-8000-000000000001',
      revision: 12,
      latestSequence: 340,
      taskCount: 2,
      tasks: [
        { id: 'task-1', kind: 'capture', atMs: 1795000000000 },
        { id: 'task-2', kind: 'ocr', atMs: 1795000010000 },
      ],
      loss: {
        metadataEvicted: 0,
        unfinishedMetadataEvicted: 0,
        imageEvicted: 0,
        recent: [],
        droppedFormalStatuses: 0,
        preReceiptStatusesOmitted: 0,
        oversizedStatuses: 0,
      },
      retention: { entries: 256, entryBytes: 16777216, evidenceImages: 8, evidenceImageBytes: 33554432 },
      ui: {
        kind: 'Hall',
        current: 'Hall',
        stable: true,
        lastStableKind: 'Hall',
        frameId: 'frame-0001',
        atMs: null,
        ageMillis: null,
        timeMissingReason: 'ui_observation_does_not_store_capture_time',
      },
      queue: { length: 2 },
    },
  }
}

/** GET /debug/read-only/entries —— src/debug/read_only/paging.rs 的 list_page。 */
export function readOnlyEntries() {
  return {
    ok: true,
    entries: {
      events: [
        { sequence: 339, kind: 'evidence.recorded', atMs: 1795000000000, payload: { taskId: 21, evidenceId: '9f2c-0001' } },
        { sequence: 340, kind: 'task.finished', atMs: 1795000010000, payload: { taskId: 21, ok: true } },
      ],
      cursor: 0,
      nextCursor: 340,
      hasMore: false,
      instance: '00000000-0000-4000-8000-000000000001',
      revision: 12,
      latestSequence: 340,
      oldestSequence: 1,
      gap: false,
      reset: false,
      taskId: null,
      loss: {
        metadataEvicted: 0,
        unfinishedMetadataEvicted: 0,
        imageEvicted: 0,
        droppedFormalStatuses: 0,
        preReceiptStatusesOmitted: 0,
        oversizedStatuses: 0,
      },
    },
  }
}

/** GET /debug/state —— routes/debug.rs 的 debug_state_route。 */
export function debugState() {
  return {
    ok: true,
    enabled: false,
    gameInputAllowed: true,
    readOnly: readOnlyMode().mode,
    commands: [
      'capture',
      'ensure_window',
      'ensure_foreground',
      'ensure_ready',
      'activate',
      'focus',
      'input_text',
      'close_window',
      'press_key',
      'key_down',
      'key_up',
      'mouse_down',
      'mouse_up',
      'click',
      'scroll',
      'drag',
      'paste',
      'playback_queue_clear',
      'playback_queue_remove_front',
      'playback_play',
      'playback_pause',
      'playback_skip_next',
      'playback_skip_previous',
      'playback_set_volume',
      'request_song',
      'chat_send',
      'startup_task',
      'task_queue_cancel',
      'wait',
    ],
  }
}

/** GET /debug/tasks —— src/debug/sandbox.rs 的 SandboxTask 列表。 */
export function debugTasks() {
  return {
    ok: true,
    tasks: [
      {
        id: 'demo-click',
        version: 1,
        description: '点击主界面按钮',
        commands: ['capture', { click: { x: 100, y: 200 } }, { wait: { milliseconds: 1000 } }],
        captureAfterEach: true,
      },
      {
        id: 'demo-ocr',
        version: 2,
        description: '扫描聊天区域',
        commands: [{ input_text: { text: '测试', settle_ms: 200 } }],
        captureAfterEach: false,
      },
    ],
  }
}

/** GET /tools/templates —— routes/system.rs 的 tool_templates_route（顶层数组）。 */
export function toolTemplates() {
  return [
    { name: 'blue-marker', label: '蓝色聊天标志', region: { x: 0, y: 0, width: 0, height: 0 }, threshold: 0.9 },
    { name: 'yellow-marker', label: '黄色聊天标志', region: { x: 0, y: 879, width: 416, height: 143 }, threshold: 0.9 },
    { name: 'paimon-menu', label: '派蒙主界面', region: { x: 1668, y: 998, width: 153, height: 44 }, threshold: 0.75 },
    { name: 'custom.myFlow', label: '自定义: myFlow', region: null, threshold: 0.7 },
  ]
}

/** GET /tools/task 与诊断任务的入队回执 —— DiagnosticTaskSnapshot（src/runtime/scheduler.rs）。 */
export function toolTask(id) {
  return { id: Number(id) || 31, label: 'OCR 全屏', status: 'completed', result: '识别到 3 条聊天：今天听点什么 / 来首晴天 / 好耶' }
}

/** GET /search —— 纯文本（不是 JSON），见 routes/playback.rs 的 search_route。 */
export function searchText() {
  return '1. 晴天 - 周杰伦 [QQ 04:29]\n2. 夜曲 - 周杰伦 [网易 04:03]'
}

/** GET /search/candidates —— 顶层数组（crates/miliastra-playback/src/model.rs 的 SearchCandidate）。 */
export function searchCandidates() {
  return [
    {
      trackRef: { key: { provider: 'qqmusic', id: '0039MnYb0qxYhV' }, resolverLocator: 'qqmusic:v2:0039MnYb0qxYhV' },
      metadata: { title: DEMO.title, artists: [DEMO.artist], album: DEMO.album, durationMs: 269000 },
      eligibility: 'eligible',
      text: `${DEMO.title} - ${DEMO.artist} [QQ 04:29]`,
    },
    {
      trackRef: { key: { provider: 'netease', id: 'song-b' } },
      metadata: { title: DEMO.other, artists: [DEMO.otherArtist] },
      eligibility: 'vip',
      text: `${DEMO.other} - ${DEMO.otherArtist} [网易 04:03]`,
    },
  ]
}

/** 写操作统一回执：真实后端各不相同，这里给最小可渲染的「已入队」形状。 */
export function actionReceipt(path) {
  return {
    ok: true,
    queued: true,
    duplicate: false,
    taskId: 42,
    position: 0,
    command: `[控制台] ${path}`,
    message: '已提交，排队执行',
  }
}

/** 配置保存/回滚的回执 —— routes/config.rs 的 save_outcome_response。 */
export function configSaved(revision = 43) {
  return {
    ok: true,
    revision,
    changedFields: [],
    restartRequired: false,
    restartFields: [],
    reloadScheduled: false,
    reloadFields: [],
    runtimeReloadFields: [],
    playbackReloadFields: [],
    appliedLiveFields: [],
    message: '配置已保存',
  }
}
