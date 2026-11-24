<template>
  <div class="form-mac-page form-mac-settings">
    <h1 class="page-title">设置</h1>
    <p class="page-subtitle">访问令牌与页面偏好</p>

    <div class="form-mac-group">
      <div class="form-mac-group-head">访问令牌</div>
      <div class="form-mac-row">
        <label class="form-mac-row-label" for="formMacToken">
          <b>Token</b>
          <span>面板所有请求都带 X-Miliastra-Token</span>
        </label>
        <div class="form-mac-row-control">
          <input id="formMacToken" v-model="tokenDraft" class="config-input" type="password" placeholder="填入面板访问令牌" />
        </div>
        <p class="form-mac-row-hint">保存在本机浏览器 cookie；换浏览器或清除 cookie 后需要重新填。</p>
      </div>
      <div class="form-mac-row">
        <div class="form-mac-row-label">
          <b>令牌操作</b>
          <span v-if="tokenMessage">{{ tokenMessage }}</span>
          <span v-else>保存后会立即用一个受保护接口验证令牌是否有效</span>
        </div>
        <div class="form-mac-row-control">
          <button class="btn primary" @click="saveToken">保存并测试</button>
          <button class="btn ghost" @click="clear">清除</button>
        </div>
      </div>
    </div>

    <div class="form-mac-group">
      <div class="form-mac-group-head">页面偏好</div>
      <!-- 开关行整行可点：用 <label> 包住「文字 + 开关」，与改造前的
           点击命中面积一致（只是外观从"行内复选框"换成 macOS 的开关行）。 -->
      <label class="form-mac-row">
        <span class="form-mac-row-label">
          <b>自动刷新状态</b>
          <span>每 15 秒拉取一次监控状态</span>
        </span>
        <span class="form-mac-row-control">
          <input v-model="state.autoRefresh" type="checkbox" />
        </span>
      </label>
      <label class="form-mac-row">
        <span class="form-mac-row-label">
          <b>日间模式</b>
          <span>在浅色与暗色主题之间切换</span>
        </span>
        <span class="form-mac-row-control">
          <input v-model="light" type="checkbox" @change="applyTheme" />
        </span>
      </label>
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
  tokenMessage.value = '已清除，请重新填入'
}
</script>
