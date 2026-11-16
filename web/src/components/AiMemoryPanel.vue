<template>
  <section class="panel ai-memory-panel">
    <div class="panel-head">
      <h2>AI 记忆</h2>
      <button class="btn small" type="button" :disabled="loading" @click="load">刷新</button>
    </div>
    <div class="panel-body">
      <div v-if="error" class="config-message err">{{ error }}</div>
      <div v-if="loading" class="loading-card">正在读取记忆…</div>

      <template v-else>
        <div class="ai-memory-section">
          <h3>大厅共享记忆</h3>
          <p class="config-field-hint">
            原文 {{ hall.count || 0 }} 条，约 {{ hall.tokens || 0 }} token
          </p>
          <div v-if="hall.summary" class="ai-memory-summary">
            <strong>摘要</strong>
            <pre>{{ hall.summary }}</pre>
          </div>
          <p v-if="hall.count && hall.count > (hall.lines || []).length" class="config-field-hint">
            接口返回最近 {{ (hall.lines || []).length }} 条原文，共 {{ hall.count }} 条；更早内容只保留在摘要里。
          </p>
          <div v-if="(hall.lines || []).length" class="ai-memory-lines">
            <div v-for="(line, index) in hall.lines" :key="index" class="ai-memory-line">
              <strong>{{ line.speaker }}</strong>：{{ line.text }}
            </div>
          </div>
          <p v-else class="config-field-hint">（暂无大厅记忆）</p>
          <button class="btn small danger" type="button" :disabled="acting" @click="clearHall">
            清空大厅记忆
          </button>
        </div>

        <div class="ai-memory-section">
          <h3>个人记忆</h3>
          <p v-if="!actors.length" class="config-field-hint">（还没有任何玩家记忆）</p>
          <div v-else class="ai-actor-list">
            <div
              v-for="actor in actors"
              :key="actor.key || actor.actor"
              class="ai-actor-row"
              :class="{ selected: selectedKey === (actor.key || actor.actor) }"
            >
              <div class="ai-actor-main">
                <strong>{{ actor.actor || '未命名玩家' }}</strong>
                <span class="config-field-hint">
                  事实 {{ actor.facts || 0 }} 条 / 对话 {{ actor.turns || 0 }} 轮 / 约 {{ actor.tokens || 0 }} token
                </span>
              </div>
              <div class="ai-actor-actions">
                <button class="btn small" type="button" @click="viewActor(actor)">查看</button>
                <button class="btn small danger" type="button" @click="deleteActor(actor)">删除</button>
              </div>
            </div>
          </div>
        </div>

        <div v-if="detail || detailLoading" class="ai-memory-section ai-memory-detail">
          <div class="ai-memory-detail-head">
            <h3>{{ detail ? (detail.actor || detail.key || '玩家记忆') : '读取中…' }}</h3>
            <div class="ai-memory-detail-actions">
              <button
                v-if="detail && detail.found !== false"
                class="btn small"
                type="button"
                :disabled="acting"
                @click="rebuildFacts"
              >
                重建事实
              </button>
              <button v-if="detail" class="btn small ghost" type="button" @click="detail = null">收起</button>
            </div>
          </div>
          <div v-if="detailLoading" class="loading-card">正在读取玩家记忆…</div>
          <template v-else-if="detail">
            <div v-if="detail.found === false" class="config-message err">
              没有这个玩家的记忆（可能已被删除）。
            </div>
            <template v-else>
              <div class="ai-memory-summary">
                <div class="ai-memory-summary-head">
                  <strong>中期摘要</strong>
                  <span class="config-field-hint">
                    {{ detail.summary ? `已生成 · 约 ${String(detail.summary).length} 字` : '未生成（原文未超预算）' }}
                  </span>
                </div>
                <pre v-if="detail.summary">{{ detail.summary }}</pre>
                <p v-else class="config-field-hint">这一层还没有摘要，事实与近期对话就是当前记忆。</p>
              </div>

              <div class="ai-memory-facts">
                <strong>事实</strong>
                <p v-if="!(detail.facts || []).length" class="config-field-hint">（暂无事实）</p>
                <div v-for="(fact, index) in detail.facts || []" :key="index" class="ai-fact-row">
                  <span>{{ fact }}</span>
                  <button class="btn small danger" type="button" @click="deleteFact(fact)">删除</button>
                </div>
              </div>

              <div v-if="(detail.turns || []).length" class="ai-memory-turns">
                <strong>近期对话</strong>
                <div class="ai-turn-list">
                  <div v-for="(turn, index) in detail.turns" :key="index" class="ai-turn-row">
                    <span class="config-field-hint">[{{ turn.asker }}]</span>
                    {{ turn.question }}
                    <div class="ai-turn-answer">↳ {{ turn.answer }}</div>
                  </div>
                </div>
              </div>
            </template>
          </template>
        </div>
      </template>
    </div>
  </section>
</template>

<script setup>
import { onMounted, ref } from 'vue'
import { getJson, requestJson } from '../api.js'
import { toast } from '../store.js'

const loading = ref(false)
const acting = ref(false)
const error = ref('')
const actors = ref([])
const hall = ref({})
const detail = ref(null)
const detailLoading = ref(false)
const selectedKey = ref('')

async function load() {
  loading.value = true
  error.value = ''
  try {
    const [actorsData, hallData] = await Promise.all([
      getJson('/ai/memory/actors'),
      getJson('/ai/memory/hall'),
    ])
    actors.value = Array.isArray(actorsData.actors) ? actorsData.actors : []
    hall.value = hallData || {}
    if (selectedKey.value && !actors.value.some((item) => (item.key || item.actor) === selectedKey.value)) {
      detail.value = null
      selectedKey.value = ''
    }
  } catch (e) {
    error.value = `读取记忆失败：${e.message || e}`
    toast(error.value, 'err')
  } finally {
    loading.value = false
  }
}

async function viewActor(actor) {
  const key = actor.key || actor.actor
  selectedKey.value = key
  detailLoading.value = true
  detail.value = null
  try {
    detail.value = await getJson(`/ai/memory/actor?name=${encodeURIComponent(key)}`)
  } catch (e) {
    toast(`读取玩家记忆失败：${e.message || e}`, 'err')
    selectedKey.value = ''
  } finally {
    detailLoading.value = false
  }
}

async function deleteActor(actor) {
  const key = actor.key || actor.actor
  const label = actor.actor || key
  if (!window.confirm(`删除「${label}」的全部记忆？此操作不可撤销。`)) return
  acting.value = true
  try {
    await requestJson('/ai/memory/forget-actor', { name: key })
    if (selectedKey.value === key) {
      detail.value = null
      selectedKey.value = ''
    }
    await load()
    toast('已删除玩家记忆', 'ok')
  } catch (e) {
    toast(`删除失败：${e.message || e}`, 'err')
  } finally {
    acting.value = false
  }
}

async function deleteFact(fact) {
  if (!detail.value) return
  const key = detail.value.key || selectedKey.value
  if (!key) return
  if (!window.confirm(`删除这条事实？\n\n${fact}`)) return
  acting.value = true
  try {
    await requestJson('/ai/memory/delete-fact', { name: key, fact })
    if (selectedKey.value) {
      detail.value = await getJson(`/ai/memory/actor?name=${encodeURIComponent(selectedKey.value)}`)
    }
    toast('已删除事实', 'ok')
  } catch (e) {
    toast(`删除失败：${e.message || e}`, 'err')
  } finally {
    acting.value = false
  }
}

async function rebuildFacts() {
  if (!detail.value) return
  const key = detail.value.key || selectedKey.value
  if (!key) return
  if (!window.confirm('重建事实会重放该玩家的历史对话并重新抽取事实，可能调用多次模型且耗时较长。继续？')) return
  acting.value = true
  try {
    const response = await requestJson('/ai/memory/rebuild', { name: key })
    toast((response && response.note) || '重建已在后台受理，结果见日志与记忆面板', 'ok')
    window.setTimeout(() => {
      if (selectedKey.value) viewActor({ key: selectedKey.value, actor: detail.value ? detail.value.actor : '' })
    }, 1500)
  } catch (e) {
    toast(`重建失败：${e.message || e}`, 'err')
  } finally {
    acting.value = false
  }
}

async function clearHall() {
  if (!window.confirm('清空大厅共享记忆？所有玩家共享的上下文都会被删除。')) return
  acting.value = true
  try {
    await requestJson('/ai/memory/forget-hall', {})
    await load()
    toast('已清空大厅记忆', 'ok')
  } catch (e) {
    toast(`清空失败：${e.message || e}`, 'err')
  } finally {
    acting.value = false
  }
}

onMounted(load)
</script>

<style scoped>
.ai-memory-panel { margin-top: var(--sp-3); }
.ai-memory-section { margin-top: var(--sp-3); }
.ai-memory-section h3 { margin: 0 0 var(--sp-2); font-size: var(--fs-title); color: var(--text-1); }
.ai-memory-summary { background: var(--bg-raised); border-radius: var(--radius-m); padding: var(--sp-2) var(--sp-3); margin: var(--sp-2) 0; }
.ai-memory-summary pre, .ai-memory-lines {
  max-height: 320px;
  overflow-y: auto; white-space: pre-wrap; word-break: break-word; margin: var(--sp-1) 0 0; font-family: var(--font-mono); font-size: var(--fs-tiny); line-height: var(--lh-tiny); max-height: 220px; overflow: auto; }
.ai-memory-summary-head { display: flex; gap: var(--sp-2); justify-content: space-between; align-items: baseline; flex-wrap: wrap; }
.ai-memory-lines { background: var(--bg-raised); border-radius: var(--radius-m); padding: var(--sp-2) var(--sp-3); }
.ai-memory-line { padding: 3px 0; border-bottom: 1px solid var(--sep-2); }
.ai-actor-list { display: flex; flex-direction: column; gap: var(--sp-1); }
.ai-actor-row { display: flex; gap: var(--sp-2); align-items: center; justify-content: space-between; padding: var(--sp-2); border: 1px solid var(--sep-2); border-radius: var(--radius-m); background: var(--bg-card); }
.ai-actor-row.selected { border-color: var(--accent); }
.ai-actor-main { display: flex; gap: var(--sp-2); align-items: baseline; flex-wrap: wrap; min-width: 0; }
.ai-actor-actions { display: flex; gap: var(--sp-1); flex: none; }
.ai-memory-detail-head { display: flex; align-items: baseline; justify-content: space-between; gap: var(--sp-2); }
.ai-memory-detail-actions { display: flex; gap: var(--sp-1); align-items: center; flex: none; }
.ai-fact-row { display: flex; gap: var(--sp-2); align-items: center; padding: 3px 0; }
.ai-fact-row span { flex: 1; min-width: 0; word-break: break-word; }
.ai-turn-list { max-height: 240px; overflow: auto; }
.ai-turn-row { padding: 3px 0; border-bottom: 1px solid var(--sep-2); }
.ai-turn-answer { opacity: .8; padding-left: var(--sp-2); }
</style>
