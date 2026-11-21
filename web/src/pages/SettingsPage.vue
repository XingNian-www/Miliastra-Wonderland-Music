<template>
  <h1 class="page-title">设置</h1>
  <p class="page-subtitle">访问令牌与页面偏好</p>
  <div class="page-grid">
    <div class="panel">
      <div class="panel-head"><h2>访问令牌</h2><small>面板所有请求都带 X-Miliastra-Token</small></div>
      <div class="panel-body">
        <label class="config-field-label">Token</label>
        <input v-model="tokenDraft" class="config-input" type="password" placeholder="填入面板访问令牌" />
        <p class="config-field-hint">保存在本机 cookie / localStorage；换浏览器需重新填。</p>
        <div style="display:flex;gap:8px;margin-top:8px">
          <button class="btn primary" @click="saveToken">保存并测试</button>
          <button class="btn ghost" @click="clear">清除</button>
          <span v-if="tokenMessage" class="config-field-hint">{{ tokenMessage }}</span>
        </div>
      </div>
    </div>
    <div class="panel">
      <div class="panel-head"><h2>页面偏好</h2></div>
      <div class="panel-body">
        <label style="display:flex;align-items:center;gap:8px">
          <input v-model="state.autoRefresh" type="checkbox" />
          <span>自动刷新状态（15 秒）</span>
        </label>
        <label style="display:flex;align-items:center;gap:8px;margin-top:8px">
          <input v-model="light" type="checkbox" @change="applyTheme" />
          <span>日间模式</span>
        </label>
      </div>
    </div>
  </div>
</template>

<script setup>
import { computed, onMounted, ref } from 'vue'
import { clearToken, readToken, request } from '../api.js'
import { loadStatus, setToken, setTheme, state, theme } from '../store.js'

const tokenDraft = ref('')
const tokenMessage = ref('')

// 主题只有一份状态（store.theme），这里只是它的复选视图。
const light = computed({
  get: () => theme.mode === 'light',
  set: (value) => setTheme(value ? 'light' : 'dark'),
})

function applyTheme() {
  // v-model 已经写入 store，这里只需要确认 DOM 属性与 store 一致。
  setTheme(light.value ? 'light' : 'dark')
}

onMounted(() => {
  tokenDraft.value = readToken()
})

async function saveToken() {
  setToken(tokenDraft.value)
  tokenMessage.value = '已保存，正在验证令牌…'
  try {
    // /health 不需要 token；必须用一个受保护接口验证令牌是否真的有效。
    await request('/config/schema')
    tokenMessage.value = '令牌有效，已恢复自动刷新'
    state.autoRefresh = true
    await loadStatus(true)
  } catch (error) {
    tokenMessage.value = `令牌验证失败：${error.message || error}`
  }
}

function clear() {
  if (!window.confirm('清除本机保存的访问令牌？清除后需要重新填写。')) return
  clearToken()
  state.token = ''
  tokenDraft.value = ''
  tokenMessage.value = '已清除，请到旧版设置页或重新填入'
}
</script>
