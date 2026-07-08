import plugin from "../../../lib/plugins/plugin.js"
import config from "../config/config.js"

const ACTIONS = ["状态", "发送", "启动", "截图", "列表"]
const SELECTOR_TTL_MS = 60_000
const pendingSelections = new Map()

export class qianxing extends plugin {
  constructor() {
    super({
      name: "千星控制",
      dsc: "Miliastra Wonderland Music 配套控制插件",
      event: "message",
      priority: 5000,
      rule: [
        {
          reg: "^#千星.*$",
          fnc: "handleQianxing"
        },
        {
          reg: "^[1-9][0-9]*$",
          fnc: "handleSelection"
        }
      ]
    })
  }

  async handleQianxing(e) {
    const parsed = parseCommand(e.msg || "")
    if (!parsed) {
      return false
    }

    if (parsed.action === "列表") {
      await e.reply(formatBackendList())
      return true
    }

    if (parsed.backendKey) {
      const backend = findBackend(parsed.backendKey)
      if (!backend) {
        await e.reply(`未找到千星后端：${parsed.backendKey}`)
        return true
      }
      await this.runSingle(e, backend, parsed)
      return true
    }

    if (parsed.action === "启动" || parsed.action === "截图") {
      await this.startSelector(e, parsed.action)
      return true
    }

    await this.runBroadcast(e, parsed)
    return true
  }

  async handleSelection(e) {
    const key = selectionKey(e)
    const selection = pendingSelections.get(key)
    if (!selection || Date.now() > selection.expiresAt) {
      pendingSelections.delete(key)
      return false
    }

    const index = Number((e.msg || "").trim()) - 1
    const backend = normalizedBackends()[index]
    if (!backend) {
      await e.reply("选择无效，请重新发送 #千星启动 或 #千星截图")
      return true
    }

    pendingSelections.delete(key)
    await this.runSingle(e, backend, {
      action: selection.action,
      text: ""
    })
    return true
  }

  async startSelector(e, action) {
    const backends = normalizedBackends()
    if (backends.length === 0) {
      await e.reply("未配置千星后端")
      return
    }
    if (backends.length === 1) {
      await this.runSingle(e, backends[0], { action, text: "" })
      return
    }

    const summaries = await Promise.all(backends.map((backend) => statusLine(backend)))
    pendingSelections.set(selectionKey(e), {
      action,
      expiresAt: Date.now() + SELECTOR_TTL_MS
    })

    await e.reply([
      `请选择要${action}的千星后端：`,
      ...summaries.map((line, index) => `${index + 1}. ${line}`),
      "",
      `回复 1-${backends.length} ${action === "启动" ? "启动对应后端" : "获取对应截图"}`
    ].join("\n"))
  }

  async runBroadcast(e, parsed) {
    const backends = normalizedBackends()
    if (backends.length === 0) {
      await e.reply("未配置千星后端")
      return
    }

    if (parsed.action === "发送" && !parsed.text.trim()) {
      await e.reply("用法：#千星发送 <内容>")
      return
    }

    const results = await Promise.all(backends.map(async (backend) => {
      try {
        return await runAction(backend, parsed)
      } catch (error) {
        return formatUnavailable(backend)
      }
    }))
    await e.reply(results.join("\n\n"))
  }

  async runSingle(e, backend, parsed) {
    if (parsed.action === "发送" && !parsed.text.trim()) {
      await e.reply(`用法：#千星${backend.key}发送 <内容>`)
      return
    }

    try {
      if (parsed.action === "截图") {
        const image = await requestScreenshot(backend)
        await e.reply([`${backend.name} 截图：`, image])
        return
      }
      await e.reply(await runAction(backend, parsed))
    } catch (error) {
      await e.reply(formatUnavailable(backend))
    }
  }
}

export function parseCommand(message) {
  const text = String(message || "").trim()
  if (!text.startsWith("#千星")) {
    return null
  }

  const rest = text.slice("#千星".length).trim()
  if (!rest) {
    return { action: "状态", backendKey: "", text: "" }
  }

  for (const action of ACTIONS) {
    if (rest === action) {
      return { action, backendKey: "", text: "" }
    }
    if (rest.startsWith(action)) {
      const next = rest.slice(action.length)
      if (!next || /^\s/.test(next)) {
        return { action, backendKey: "", text: next.trimStart() }
      }
    }
  }

  for (const action of ACTIONS.filter((item) => item !== "列表")) {
    const index = rest.indexOf(action)
    if (index > 0) {
      const backendKey = rest.slice(0, index).trim()
      const text = rest.slice(index + action.length).trimStart()
      if (backendKey) {
        return { action, backendKey, text }
      }
    }
  }

  return null
}

function normalizedBackends() {
  return (config.backends || [])
    .map((backend, index) => ({
      key: String(backend.key || index + 1),
      name: backend.name || `${backend.key || index + 1}号千星`,
      baseUrl: String(backend.baseUrl || "").replace(/\/+$/, "")
    }))
    .filter((backend) => backend.baseUrl)
}

function findBackend(key) {
  const needle = String(key || "").toLowerCase()
  return normalizedBackends().find((backend) =>
    backend.key.toLowerCase() === needle || backend.name.toLowerCase() === needle
  )
}

function formatBackendList() {
  const backends = normalizedBackends()
  if (backends.length === 0) {
    return "未配置千星后端"
  }
  return [
    "千星后端：",
    ...backends.map((backend, index) =>
      `${index + 1}. ${backend.name}（${backend.key}）${backend.baseUrl}`
    )
  ].join("\n")
}

async function runAction(backend, parsed) {
  if (parsed.action === "状态") {
    return statusSummary(backend)
  }
  if (parsed.action === "发送") {
    await apiJson(backend, "/chat/send", {
      method: "POST",
      query: { text: parsed.text }
    })
    return `${backend.name}：已加入发送队列`
  }
  if (parsed.action === "启动") {
    await apiJson(backend, "/startup/wonderland", { method: "POST" })
    return "正在启动游戏并进入千星"
  }
  throw new Error(`Unsupported action: ${parsed.action}`)
}

async function statusSummary(backend) {
  const [status, queue] = await Promise.all([
    apiJson(backend, "/status"),
    apiJson(backend, "/queue")
  ])
  const queueItems = Array.isArray(queue) ? queue : []
  return [
    `${backend.name}：在线`,
    `播放：${formatPlayerStatus(status)}`,
    formatQueue(queueItems)
  ].join("\n")
}

async function statusLine(backend) {
  try {
    const status = await apiJson(backend, "/status")
    return `${backend.name}：在线，${formatPlayerStatus(status)}`
  } catch (error) {
    return `${backend.name}：离线或接口不可用`
  }
}

function formatPlayerStatus(status) {
  const state = status.status || "未知"
  const title = [status.name, status.singer].filter(Boolean).join(" - ") || "无当前歌曲"
  const progress = formatTime(status.progress)
  const duration = formatTime(status.duration)
  const volume = Number.isFinite(Number(status.volume)) ? `，音量 ${status.volume}` : ""
  if (duration) {
    return `${state}，${title}（${progress || "0:00"}/${duration}${volume}）`
  }
  return `${state}，${title}${volume}`
}

function formatQueue(queue) {
  const limit = Number(config.queuePreviewLimit || 5)
  if (queue.length === 0) {
    return "队列：空"
  }
  const preview = queue.slice(0, limit).map((item, index) => {
    const source = item.source ? ` [${item.source}]` : ""
    return `${index + 1}. ${item.keyword || item.uri || "未命名"}${source}`
  })
  return [`队列：${queue.length} 首`, ...preview].join("\n")
}

function formatTime(value) {
  const seconds = Math.floor(Number(value || 0))
  if (!Number.isFinite(seconds) || seconds <= 0) {
    return ""
  }
  const minutes = Math.floor(seconds / 60)
  const rest = String(seconds % 60).padStart(2, "0")
  return `${minutes}:${rest}`
}

function formatUnavailable(backend) {
  return `${backend.name}：千星机器人未在线或接口不可用`
}

async function requestScreenshot(backend) {
  const response = await apiFetch(backend, "/screenshot")
  const arrayBuffer = await response.arrayBuffer()
  const buffer = Buffer.from(arrayBuffer)
  return segment.image(buffer)
}

async function apiJson(backend, path, options = {}) {
  const response = await apiFetch(backend, path, options)
  const text = await response.text()
  if (!text) {
    return {}
  }
  return JSON.parse(text)
}

async function apiFetch(backend, path, options = {}) {
  const controller = new AbortController()
  const timeout = setTimeout(() => controller.abort(), Number(config.requestTimeoutMs || 5000))
  const url = new URL(`${backend.baseUrl}${path}`)
  for (const [key, value] of Object.entries(options.query || {})) {
    url.searchParams.set(key, value)
  }

  try {
    const response = await fetch(url, {
      method: options.method || "GET",
      signal: controller.signal
    })
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`)
    }
    return response
  } finally {
    clearTimeout(timeout)
  }
}

function selectionKey(e) {
  return `${e.group_id || "private"}:${e.user_id || "unknown"}`
}
