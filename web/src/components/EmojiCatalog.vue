<template>
  <div class="emoji-catalog">
    <div class="emoji-catalog-head">
      <b>全量表情目录</b>
      <span class="emoji-catalog-meta">上次扫描：{{ scannedText }}</span>
    </div>
    <div class="emoji-catalog-meta">
      页签 1 = 收藏，2..6 = 角色。第 7 个页签会把收藏页向左挤走并隐藏，所以只扫描 1..6。
    </div>
    <div v-if="message.text" class="config-message" :class="message.kind" style="margin-top: 8px">
      {{ message.text }}
    </div>

    <!-- 当前 AI 可用表情（GET /emoji/catalog）：与扫描目录并列展示 -->
    <div
      style="margin-top: 10px; padding: 8px 10px; border: 1px solid var(--sep-2); border-radius: var(--radius-s); background: var(--bg-1)"
    >
      <div class="emoji-catalog-head">
        <b>AI 可用表情（GET /emoji/catalog）</b>
        <span class="emoji-catalog-meta">
          <template v-if="aiCatalog">
            服务端白名单 {{ serverAllowedCount }} 条 · 表情能力{{ emojiEnabled ? '已开启' : '已关闭' }} · AI
            表情{{ aiEnabled ? '已开启' : '已关闭' }}
          </template>
          <template v-else-if="aiCatalogLoading">读取中…</template>
          <template v-else>尚未读取</template>
        </span>
        <button
          class="btn small ghost"
          type="button"
          :disabled="disabled || aiCatalogLoading"
          @click="loadAiCatalog"
        >
          {{ aiCatalogLoading ? '刷新中…' : '刷新' }}
        </button>
      </div>
      <div class="emoji-catalog-meta" style="margin-top: 4px">
        扫描目录 {{ scannedTotal }} 条：已允许 <b>{{ scannedAllowedCount }}</b> 条、未允许
        <b>{{ scannedNotAllowedCount }}</b> 条；草稿白名单 {{ allowedList.length }} 条<template v-if="outsideCatalogCount">（扫描目录外 {{ outsideCatalogCount }} 条）</template>
        <span v-if="draftDiffersFromServer" class="config-dirty"> · 白名单有未保存修改</span>
      </div>
      <div v-if="aiCatalogError" class="config-field-hint" style="margin-top: 4px; color: var(--danger)">
        {{ aiCatalogError }}
      </div>
      <details v-if="catalogText" class="emoji-pack">
        <summary>AI 提示词中的表情目录文本</summary>
        <pre style="margin: 4px 0 0; white-space: pre-wrap; word-break: break-word; font-family: var(--font-mono); font-size: var(--fs-tiny); line-height: var(--lh-tiny); color: var(--text-2)">{{ catalogText }}</pre>
      </details>
      <details v-if="aiCatalog && aiCatalog.allowed.length" class="emoji-pack">
        <summary>服务端已允许的表情（{{ aiCatalog.allowed.length }}）</summary>
        <div class="emoji-allow-chips">
          <span
            v-for="item in aiCatalog.allowed"
            :key="`${item.pack}|${item.text}`"
            class="emoji-allow-chip"
          >
            <code>{{ item.pack }}|{{ item.text }}</code>
          </span>
        </div>
      </details>
    </div>
    <div class="emoji-catalog-actions">
      <button type="button" class="btn primary" :disabled="disabled || scanning" @click="scan">
        {{ scanning ? '扫描中…' : '扫描全部页签' }}
      </button>
      <button
        type="button"
        class="btn"
        :disabled="disabled || !canAdd || checkedCount === 0"
        @click="addSelected"
      >
        加入勾选到 AI 白名单
      </button>
      <span class="emoji-catalog-meta">
        已勾选 <b>{{ checkedCount }}</b> 条；加入后需点页面底部「保存」才会写入配置
      </span>
    </div>
    <div class="emoji-search">
      <input
        v-model="searchText"
        type="text"
        placeholder="搜索表情文字（忽略符号）"
        :disabled="disabled"
      />
    </div>
    <div v-if="loadError" class="config-message err" style="margin-top: 8px">{{ loadError }}</div>
    <div v-else-if="loading" class="emoji-catalog-empty">正在读取已保存的表情目录…</div>
    <div v-else-if="!packs.length" class="emoji-catalog-empty">
      还没有扫描结果。点「扫描全部页签」读取 1..6 页签的可见表情。
    </div>
    <details
      v-for="pack in packs"
      :key="pack.pack"
      class="emoji-pack"
      :open="isOpen(pack.pack)"
      @toggle="onPackToggle(pack.pack, $event)"
    >
      <summary>
        页签 {{ pack.pack }}
        <span class="emoji-catalog-meta">
          {{ pack.labels.length }} 条 · 已允许 {{ packAllowedCount(pack.pack) }} 条<template v-if="query">，匹配 {{ visibleLabels(pack).length }}</template>
        </span>
      </summary>
      <div class="emoji-pack-actions">
        <button type="button" class="btn small ghost" :disabled="disabled" @click="selectPack(pack.pack, true)">
          全选本页签
        </button>
        <button type="button" class="btn small ghost" :disabled="disabled" @click="selectPack(pack.pack, false)">
          清空本页签
        </button>
        <button
          type="button"
          class="btn small"
          :disabled="disabled || !canAdd || packMissingCount(pack.pack) === 0"
          :title="canAdd ? `把页签 ${pack.pack} 中尚未允许的表情直接加入草稿白名单` : '当前 schema 没有 emoji.ai_allowed 字段'"
          @click="addPackToAllowed(pack.pack)"
        >
          本页签全部加入白名单<template v-if="packMissingCount(pack.pack)">（{{ packMissingCount(pack.pack) }}）</template>
        </button>
      </div>
      <div class="emoji-labels">
        <label
          v-for="label in visibleLabels(pack)"
          :key="pack.pack + '|' + label"
          class="emoji-label"
          :class="{ on: checked.has(entryOf(pack.pack, label)) }"
        >
          <input
            type="checkbox"
            :checked="checked.has(entryOf(pack.pack, label))"
            :disabled="disabled"
            @change="toggleLabel(pack.pack, label, $event)"
          />
          <span>{{ label }}</span>
        </label>
        <span v-if="!visibleLabels(pack).length" class="emoji-catalog-meta">（无可显示的表情）</span>
      </div>
    </details>
  </div>
</template>

<script setup>
import { computed, onMounted, ref, watch } from 'vue'
import { getJson, requestJson } from '../api.js'
import { handleAuth } from '../store.js'

const props = defineProps({
  allowed: { type: Array, default: () => [] },
  disabled: { type: Boolean, default: false },
  canAdd: { type: Boolean, default: true },
})

const emit = defineEmits(['update:allowed'])

const packs = ref([])
const scannedAtMs = ref(0)
const scanning = ref(false)
const loading = ref(false)
const loadError = ref('')
const message = ref({ text: '', kind: '' })
const searchText = ref('')
const openPacks = ref({})
const checked = ref(new Set())
const aiCatalog = ref(null)
const aiCatalogLoading = ref(false)
const aiCatalogError = ref('')

const query = computed(() => searchText.value.trim().toLowerCase())
const queryStripped = computed(() => stripSymbols(searchText.value))
const checkedCount = computed(() => checked.value.size)
const scannedText = computed(() =>
  scannedAtMs.value ? new Date(Number(scannedAtMs.value)).toLocaleString() : '—',
)
const catalogEntries = computed(() => {
  const entries = new Set()
  for (const pack of packs.value) {
    for (const label of pack.labels) entries.add(entryOf(pack.pack, label))
  }
  return entries
})

/* ---------- AI 可用表情（GET /emoji/catalog）与统计 ---------- */
const allowedList = computed(() => (Array.isArray(props.allowed) ? props.allowed.map(String) : []))
const allowedEntries = computed(() => new Set(allowedList.value))
const scannedTotal = computed(() => packs.value.reduce((sum, pack) => sum + pack.labels.length, 0))
const scannedAllowedCount = computed(() => {
  let count = 0
  for (const pack of packs.value) {
    for (const label of pack.labels) {
      if (allowedEntries.value.has(entryOf(pack.pack, label))) count += 1
    }
  }
  return count
})
const scannedNotAllowedCount = computed(() => Math.max(scannedTotal.value - scannedAllowedCount.value, 0))
const outsideCatalogCount = computed(
  () => allowedList.value.filter((entry) => !catalogEntries.value.has(entry)).length,
)
const serverAllowedCount = computed(() =>
  aiCatalog.value && Array.isArray(aiCatalog.value.allowed) ? aiCatalog.value.allowed.length : 0,
)
const aiEnabled = computed(() => Boolean(aiCatalog.value && aiCatalog.value.aiEnabled === true))
const emojiEnabled = computed(() => Boolean(aiCatalog.value && aiCatalog.value.enabled === true))
const catalogText = computed(() =>
  aiCatalog.value && typeof aiCatalog.value.catalog === 'string' ? aiCatalog.value.catalog : '',
)
const draftDiffersFromServer = computed(() => {
  if (!aiCatalog.value) return false
  const server = new Set(
    (Array.isArray(aiCatalog.value.allowed) ? aiCatalog.value.allowed : []).map(
      (item) => `${item.pack}|${item.text}`,
    ),
  )
  if (server.size !== allowedEntries.value.size) return true
  for (const entry of allowedEntries.value) {
    if (!server.has(entry)) return true
  }
  return false
})

// 与后端 normalize_emoji_label 对齐：只保留字母/数字/汉字，便于搜索时忽略符号。
function stripSymbols(text) {
  return String(text == null ? '' : text)
    .toLowerCase()
    .replace(/[\s\p{P}\p{S}]+/gu, '')
}

function entryOf(pack, label) {
  return `${pack}|${label}`
}

function matches(label) {
  if (!query.value) return true
  const text = String(label)
  if (text.toLowerCase().includes(query.value)) return true
  return queryStripped.value !== '' && stripSymbols(text).includes(queryStripped.value)
}

function visibleLabels(pack) {
  return pack.labels.filter(matches)
}

function isOpen(pack) {
  if (query.value) return true
  return !!openPacks.value[pack]
}

function onPackToggle(pack, event) {
  if (query.value) return
  const target = event && event.target ? event.target.open : false
  openPacks.value = { ...openPacks.value, [pack]: !!target }
}

function setMessage(text, kind = '') {
  message.value = { text: text || '', kind }
}

function syncChecked() {
  const entries = catalogEntries.value
  const list = Array.isArray(props.allowed) ? props.allowed.map(String) : []
  checked.value = new Set(list.filter((entry) => entries.has(entry)))
}

watch(() => props.allowed, syncChecked)

function applyPayload(payload) {
  const tabs = payload && Array.isArray(payload.tabs) ? payload.tabs : []
  packs.value = tabs.map((tab) => ({
    pack: tab.pack,
    labels: Array.isArray(tab.labels) ? tab.labels.map(String) : [],
  }))
  scannedAtMs.value = Number(payload && payload.scannedAtMs) || 0
  if (!Object.keys(openPacks.value).length && packs.value.length) {
    openPacks.value = { [packs.value[0].pack]: true }
  }
}

async function loadScanned() {
  loading.value = true
  loadError.value = ''
  try {
    const data = await getJson('/emoji/scanned')
    const payload =
      data && Array.isArray(data.tabs)
        ? data
        : data && data.catalog && typeof data.catalog === 'object'
          ? data.catalog
          : data && typeof data === 'object'
            ? data
            : {}
    applyPayload(payload)
    syncChecked()
  } catch (error) {
    if (!handleAuth(error)) loadError.value = `读取表情目录失败：${error.message || error}`
  } finally {
    loading.value = false
  }
}

async function scan() {
  if (scanning.value || props.disabled) return
  scanning.value = true
  setMessage('扫描中：会逐个点击 1..6 页签并 OCR，约 10 秒…', 'ok')
  try {
    const data = await requestJson('/emoji/scan', {})
    const payload = data && typeof data === 'object' ? data : {}
    applyPayload(payload)
    syncChecked()
    if (payload.ok === false) setMessage(`扫描返回异常：${payload.message || '未知错误'}`, 'err')
    else setMessage('扫描完成，已保存到 deps/data/emoji-catalog.json', 'ok')
  } catch (error) {
    if (handleAuth(error)) setMessage('访问令牌无效或已过期，请到「设置」页填写令牌', 'err')
    else setMessage(`扫描失败：${error.message || error}`, 'err')
  } finally {
    scanning.value = false
  }
}

function toggleLabel(pack, label, event) {
  const entry = entryOf(pack, label)
  const next = new Set(checked.value)
  if (event && event.target && event.target.checked) next.add(entry)
  else next.delete(entry)
  checked.value = next
}

function selectPack(pack, on) {
  const target = packs.value.find((item) => String(item.pack) === String(pack))
  if (!target) return
  const next = new Set(checked.value)
  for (const label of target.labels) {
    const entry = entryOf(pack, label)
    if (on) next.add(entry)
    else next.delete(entry)
  }
  checked.value = next
}

/* ---------- 按页签统计 / 直接把整页签加入白名单 ---------- */
function packByNumber(pack) {
  return packs.value.find((item) => String(item.pack) === String(pack)) || null
}

function packAllowedCount(pack) {
  const target = packByNumber(pack)
  if (!target) return 0
  return target.labels.filter((label) => allowedEntries.value.has(entryOf(target.pack, label))).length
}

function packMissingCount(pack) {
  const target = packByNumber(pack)
  if (!target) return 0
  return target.labels.filter((label) => !allowedEntries.value.has(entryOf(target.pack, label))).length
}

function addPackToAllowed(pack) {
  if (!props.canAdd) {
    setMessage('当前后端 schema 没有 emoji.ai_allowed 字段，无法写入白名单', 'err')
    return
  }
  const target = packByNumber(pack)
  if (!target) return
  const current = Array.isArray(props.allowed) ? props.allowed.map(String) : []
  const next = current.slice()
  let added = 0
  for (const label of target.labels) {
    const entry = entryOf(target.pack, label)
    if (next.includes(entry)) continue
    next.push(entry)
    added += 1
  }
  if (!added) {
    setMessage(`页签 ${pack} 的表情已全部在白名单中`)
    return
  }
  // 只改 draft：父组件 setFieldValue 写入配置草稿并 markDirty('emoji')，仍需点保存。
  emit('update:allowed', next)
  setMessage(
    `已把页签 ${pack} 的 ${added} 条表情加入草稿白名单（尚未保存，请点页面底部「保存」）`,
    'ok',
  )
}

async function loadAiCatalog() {
  if (aiCatalogLoading.value) return
  aiCatalogLoading.value = true
  aiCatalogError.value = ''
  try {
    const data = await getJson('/emoji/catalog')
    if (!data || typeof data !== 'object' || Array.isArray(data)) {
      throw new Error('响应格式不正确')
    }
    if (data.ok === false) throw new Error(data.message || '读取失败')
    aiCatalog.value = {
      enabled: data.enabled === true,
      aiEnabled: data.aiEnabled === true,
      allowed: Array.isArray(data.allowed) ? data.allowed : [],
      catalog: typeof data.catalog === 'string' ? data.catalog : '',
    }
  } catch (error) {
    if (!handleAuth(error)) aiCatalogError.value = `读取 AI 可用表情失败：${error.message || error}`
  } finally {
    aiCatalogLoading.value = false
  }
}

function addSelected() {
  if (!props.canAdd) {
    setMessage('当前后端 schema 没有 emoji.ai_allowed 字段，无法写入白名单', 'err')
    return
  }
  const entries = Array.from(checked.value)
  if (!entries.length) {
    setMessage('先勾选要加入 AI 白名单的表情', 'err')
    return
  }
  const current = Array.isArray(props.allowed) ? props.allowed.map(String) : []
  const merged = current.slice()
  let added = 0
  for (const entry of entries) {
    if (!merged.includes(entry)) {
      merged.push(entry)
      added += 1
    }
  }
  emit('update:allowed', merged)
  setMessage(
    added
      ? `已加入 ${added} 条到 AI 可用表情（尚未保存，请点页面底部「保存」）`
      : '勾选的表情都已在 AI 白名单中',
    added ? 'ok' : '',
  )
}

onMounted(() => {
  loadScanned()
  loadAiCatalog()
})
</script>
