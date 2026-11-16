<template>
  <h1 class="page-title">账号登录</h1>
  <p class="page-subtitle">音乐平台登录、账号凭据与酷狗 VIP 状态</p>

  <div class="page-grid">
    <section class="panel wide">
      <div class="panel-head">
        <h2>登录会话</h2>
        <small>{{ loginMeta }}</small>
      </div>
      <div class="panel-body">
        <div class="login-session">
          <div class="login-session-grid">
            <div class="login-fact"><span>当前平台</span><strong>{{ activeProviderName }}</strong></div>
            <div class="login-fact"><span>阶段</span><strong>{{ login.phase || login.stage || (login.active ? '等待扫码/确认' : '空闲') }}</strong></div>
            <div class="login-fact"><span>会话 ID</span><strong :title="login.sessionId || ''">{{ login.sessionId || '无' }}</strong></div>
            <div class="login-fact"><span>剩余时间</span><strong>{{ remainingText }}</strong></div>
            <div class="login-fact"><span>开始时间</span><strong>{{ formatDate(login.startedAtMs ?? login.startedAt) }}</strong></div>
            <div class="login-fact"><span>截止时间</span><strong>{{ formatDate(login.deadlineAtMs ?? login.deadlineAt) }}</strong></div>
            <div class="login-fact"><span>最近错误代码</span><strong>{{ login.lastError?.code || login.lastErrorCode || '无' }}</strong></div>
            <div class="login-fact"><span>最近错误信息</span><strong :title="login.lastError?.message || login.lastErrorMessage || ''">{{ login.lastError?.message || login.lastErrorMessage || '无' }}</strong></div>
          </div>

          <div v-if="qrImage && !qrBroken" class="login-qr">
            <img :src="qrImage" alt="平台登录二维码" @error="qrBroken = true" />
            <div>
              <strong>使用 {{ activeProviderName }} App 扫码登录</strong>
              <small>二维码过期或扫码失败后，请重新发起登录。</small>
            </div>
          </div>
          <div v-else-if="login.active" class="result">
            {{ qrHint }}
          </div>
        </div>

        <div class="controls">
          <button class="btn" :disabled="busy" @click="loadAll">刷新全部状态</button>
          <button class="btn warn" :disabled="busy || !login.active || !login.sessionId" @click="cancelLogin">取消当前登录</button>
        </div>
        <div v-if="actionText" class="result">{{ actionText }}</div>
        <div class="config-field-hint">登录二维码由后端登录会话返回；没有二维码时，请在独立安全窗口完成平台登录，本页仍会显示状态。</div>
      </div>
    </section>

    <section class="panel wide">
      <div class="panel-head">
        <h2>酷狗账号</h2>
        <small>/player/kugou/status</small>
      </div>
      <div class="panel-body">
        <div v-if="kugou" class="login-session">
          <div class="login-session-grid">
            <div class="login-fact"><span>登录</span><strong>{{ kugou.loggedIn ? '已登录' : '未登录' }}</strong></div>
            <div class="login-fact"><span>账号</span><strong>{{ kugou.nickname || kugou.userId || '未知' }}</strong></div>
            <div class="login-fact"><span>VIP</span><strong>{{ kugou.vip ? (kugou.vipType && String(kugou.vipType) !== '0' ? `VIP（${kugou.vipType}）` : 'VIP') : '非 VIP' }}</strong></div>
            <div class="login-fact"><span>VIP 到期</span><strong>{{ kugou.vipExpireAtMs ? formatDate(kugou.vipExpireAtMs) : '未提供' }}</strong></div>
          </div>
        </div>
        <div v-else class="result">暂无酷狗账号状态，或该平台未启用。</div>
        <div class="config-field-hint">此状态只读；领取/升级 VIP 使用下方平台卡片按钮。</div>
      </div>
    </section>

    <section class="panel wide">
      <div class="panel-head">
        <h2>平台账号</h2>
        <small>{{ providers.length }} 个平台</small>
      </div>
      <div class="panel-body">
        <div class="controls">
          <button class="btn ghost" :disabled="busy" @click="refreshAllAccounts">刷新全部账号</button>
          <button class="btn ghost" :disabled="busy" @click="loadProviders">刷新平台列表</button>
        </div>

        <div v-if="!providers.length" class="result">没有可用平台，请先在「配置中心 → 播放器」填写平台凭据。</div>

        <div v-for="item in providers" :key="item.provider" class="login-card">
          <div class="login-card-head">
            <div>
              <strong>{{ providerLabel(item.provider) }}</strong>
              <small>{{ item.configured ? '已配置' : '未配置' }} · 完整度 {{ completeness(item) }}%</small>
            </div>
            <div class="controls">
              <button class="btn small primary" :disabled="busy || login.active" @click="startLogin(item.provider)">登录</button>
              <button v-if="login.active && login.provider === item.provider" class="btn small warn" :disabled="busy" @click="cancelLogin">取消</button>
              <button class="btn small" :disabled="busy" @click="refreshAccount(item.provider)">刷新状态</button>
              <button v-if="item.manualRefreshSupported || item.refreshSupported" class="btn small" :disabled="busy" @click="refreshCredentials(item.provider)">刷新凭据</button>
              <button class="btn small ghost" :disabled="busy || login.active" @click="logout(item.provider)">退出</button>
            </div>
          </div>

          <div class="login-progress"><i :style="{ width: completeness(item) + '%' }"></i></div>

          <div v-if="account(item.provider)" class="login-account" :class="{ vip: account(item.provider).vip }">
            <span class="field-state" :class="{ ok: account(item.provider).loggedIn }">{{ account(item.provider).loggedIn ? '已登录' : '未登录' }}</span>
            <span>账号：{{ accountName(item.provider) }}{{ loginMethod(item.provider) }}</span>
            <span v-if="vipLabel(item.provider)" class="vip-badge">{{ vipLabel(item.provider) }}</span>
            <span v-if="account(item.provider).stale" class="field-state">状态可能过期</span>
          </div>

          <div class="login-fields">
            <span
              v-for="[name, value] in Object.entries(item.fields || {})"
              :key="name"
              class="field-state"
              :class="{ ok: value }"
            >{{ name }}：{{ value ? '就绪' : '缺失' }}</span>
            <span v-if="!Object.keys(item.fields || {}).length" class="field-state">后端暂未提供字段状态</span>
          </div>

          <div class="login-refresh-info">
            {{ refreshInfo(item) }}
          </div>

          <div v-if="item.provider === 'kugou'" class="controls" style="margin-top:10px">
            <button class="btn small good" :disabled="busy" @click="claimVip">领取酷狗 VIP</button>
            <button class="btn small good" :disabled="busy" @click="upgradeVip">升级酷狗 VIP</button>
          </div>
        </div>
      </div>
    </section>
  </div>
</template>

<script setup>
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { getJson, requestJson } from '../api.js'
import { handleAuth, toast } from '../store.js'

const providers = ref([])
const login = ref({ active: false })
const accounts = ref({})
const kugou = ref(null)
const busy = ref(false)
const actionText = ref('')
const qrBroken = ref(false)
const now = ref(Date.now())
let timer = null
let statusTimer = null
let loading = false

const sourceNames = { qqmusic: 'QQ音乐', netease: '网易云音乐', bilibili: '哔哩哔哩', kugou: '酷狗音乐' }
const activeProviderName = computed(() => login.value?.active ? providerLabel(login.value.provider) : '无')
const loginMeta = computed(() => login.value?.active ? `正在登录 ${providerLabel(login.value.provider)}` : '无进行中的登录')
const remainingText = computed(() => {
  if (!login.value?.active) return '—'
  const seconds = login.value.remainingSeconds
    ?? (login.value.deadlineAtMs ? Math.max(0, Math.ceil((Number(login.value.deadlineAtMs) - now.value) / 1000)) : null)
  return seconds == null ? '暂未提供' : `${Math.max(0, Number(seconds))} 秒`
})
const qrImage = computed(() => {
  const raw = login.value?.qrCode?.imageDataUrl || login.value?.qrImage || ''
  return typeof raw === 'string' && /^data:image\/(?:png|jpeg);base64,[A-Za-z0-9+/]+={0,2}$/.test(raw) ? raw : ''
})
watch(qrImage, () => { qrBroken.value = false })
const qrHint = computed(() => {
  if (!login.value?.active) return ''
  if (login.value.provider === 'kugou') return '正在获取酷狗登录二维码…'
  return '二维码正在准备；若未显示，请在独立安全窗口完成登录。'
})

function providerLabel(provider) {
  return sourceNames[provider] || provider || '未知平台'
}
function formatDate(value) {
  if (value == null || value === '') return '暂未提供'
  const date = new Date(typeof value === 'number' && value < 1e12 ? value * 1000 : value)
  return Number.isNaN(date.getTime()) ? '暂未提供' : date.toLocaleString('zh-CN', { hour12: false })
}
function completeness(item) {
  if (item.completeness != null) return Math.max(0, Math.min(100, Number(item.completeness) || 0))
  if (item.completion != null) return Math.max(0, Math.min(100, Number(item.completion) || 0))
  const fields = Object.values(item.fields || {})
  if (fields.length) return Math.round((fields.filter(Boolean).length / fields.length) * 100)
  return item.configured ? 100 : 0
}
function account(provider) {
  return accounts.value[provider] || null
}
function maskUserId(value) {
  const text = String(value == null ? '' : value)
  if (text.length <= 4) return '****'
  return `${text.slice(0, 2)}****${text.slice(-2)}`
}
function accountName(provider) {
  const info = account(provider)
  if (info?.nickname) return info.nickname
  if (info?.userId != null) return `ID ${maskUserId(info.userId)}`
  return '未知'
}
function loginMethod(provider) {
  const method = account(provider)?.loginMethod
  if (method === 'qq') return ' · QQ登录'
  if (method === 'wechat') return ' · 微信登录'
  return ''
}
function vipLabel(provider) {
  const info = account(provider)
  if (!info || !info.loggedIn) return ''
  if (provider === 'bilibili') return '播放无需 VIP'
  if (!info.vipKnown) return 'VIP 状态未知'
  if (!info.vip) return '非 VIP'
  const type = info.vipType && String(info.vipType) !== '0' ? `（${info.vipType}）` : ''
  const expire = info.vipExpireAtMs ? ` · 到期 ${formatDate(info.vipExpireAtMs)}` : ''
  return `VIP${type}${expire}`
}
function refreshInfo(item) {
  const parts = []
  if (item.refreshSupported || item.manualRefreshSupported) {
    parts.push(`刷新：${item.refreshState || 'idle'}`)
    parts.push(`上次：${formatDate(item.lastRefreshAtMs)}`)
    if (item.nextRefreshCheckAtMs) parts.push(`下次：${formatDate(item.nextRefreshCheckAtMs)}`)
  } else {
    parts.push('不支持自动刷新凭据')
  }
  if (item.lastRefreshError) parts.push(item.lastRefreshError)
  return parts.join(' · ')
}
function report(error) {
  if (handleAuth(error)) return false
  actionText.value = `错误：${error?.message || error}`
  toast(actionText.value, 'err')
  return false
}
async function loadProviders() {
  try {
    const value = await getJson('/player/providers')
    providers.value = Array.isArray(value) ? value : (Array.isArray(value?.providers) ? value.providers : [])
  } catch (error) {
    report(error)
  }
}
async function loadKugouStatus() {
  try {
    kugou.value = await getJson('/player/kugou/status')
  } catch {
    kugou.value = null
  }
}
async function loadLoginStatus() {
  try {
    login.value = (await getJson('/player/login/status')) || { active: false }
  } catch (error) {
    report(error)
  }
}
async function loadAccounts() {
  const next = {}
  await Promise.all(['qqmusic', 'netease', 'kugou', 'bilibili'].map(async (provider) => {
    try {
      next[provider] = await getJson(`/player/account/status?provider=${encodeURIComponent(provider)}`)
    } catch {
      next[provider] = null
    }
  }))
  accounts.value = next
}
async function loadAll() {
  if (loading) return
  loading = true
  try {
    await Promise.allSettled([loadProviders(), loadLoginStatus(), loadAccounts(), loadKugouStatus()])
  } finally {
    loading = false
  }
}
async function startLogin(provider) {
  if (busy.value) return
  busy.value = true
  actionText.value = `正在发起 ${providerLabel(provider)} 登录…`
  try {
    await requestJson('/player/login/start', { provider })
    actionText.value = '登录请求已提交'
    await loadAll()
  } catch (error) {
    report(error)
  } finally {
    busy.value = false
  }
}
async function cancelLogin() {
  if (busy.value || !login.value?.sessionId) return
  busy.value = true
  try {
    await requestJson('/player/login/cancel', { sessionId: login.value.sessionId })
    actionText.value = '已取消登录'
    await loadAll()
  } catch (error) {
    report(error)
  } finally {
    busy.value = false
  }
}
async function logout(provider) {
  if (busy.value || !window.confirm(`确定退出 ${providerLabel(provider)}？`)) return
  busy.value = true
  try {
    await requestJson('/player/logout', { provider })
    actionText.value = `已退出 ${providerLabel(provider)}`
    await loadAll()
  } catch (error) {
    report(error)
  } finally {
    busy.value = false
  }
}
async function refreshAccount(provider) {
  if (busy.value) return
  busy.value = true
  try {
    const info = await requestJson(`/player/account/refresh?provider=${encodeURIComponent(provider)}`, {})
    accounts.value = { ...accounts.value, [provider]: info || null }
    actionText.value = `已刷新 ${providerLabel(provider)} 账号状态`
  } catch (error) {
    report(error)
  } finally {
    busy.value = false
  }
}
async function refreshAllAccounts() {
  if (busy.value) return
  busy.value = true
  try {
    await loadAccounts()
    actionText.value = '已刷新全部账号状态'
  } finally {
    busy.value = false
  }
}
async function refreshCredentials(provider) {
  if (busy.value) return
  busy.value = true
  try {
    await requestJson(`/player/login/refresh?provider=${encodeURIComponent(provider)}`, {})
    actionText.value = `已刷新 ${providerLabel(provider)} 凭据`
    await loadAll()
  } catch (error) {
    report(error)
  } finally {
    busy.value = false
  }
}
async function claimVip() {
  if (busy.value) return
  busy.value = true
  try {
    const value = await requestJson('/player/kugou/claim-vip', {})
    actionText.value = value?.accepted
      ? `领取成功：VIP+${value.vipDays ?? 0} 天${value.message ? `（${value.message}）` : ''}`
      : `领取失败：${value?.message || '未知原因'}`
    await loadAll()
  } catch (error) {
    report(error)
  } finally {
    busy.value = false
  }
}
async function upgradeVip() {
  if (busy.value) return
  busy.value = true
  try {
    const value = await requestJson('/player/kugou/upgrade-vip', {})
    actionText.value = value?.accepted
      ? `升级成功：VIP+${value.vipDays ?? 0} 天${value.message ? `（${value.message}）` : ''}`
      : `升级失败：${value?.message || '未知原因'}`
    await loadAll()
  } catch (error) {
    report(error)
  } finally {
    busy.value = false
  }
}

onMounted(() => {
  loadAll()
  timer = setInterval(() => {
    now.value = Date.now()
  }, 1000)
  statusTimer = setInterval(loadAll, 5000)
})
onBeforeUnmount(() => {
  clearInterval(timer)
  clearInterval(statusTimer)
})
</script>
