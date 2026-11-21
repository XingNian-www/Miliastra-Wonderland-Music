import { createApp } from 'vue'
import App from './App.vue'
// 样式固定三层：令牌 → 骨架 → 组件。顺序即级联契约，不要调换。
import './styles/tokens.css'
import './styles/base.css'
import './styles/components.css'

// 首帧前确定主题，避免先渲染暗色再闪一下切浅色。
if ((() => {
  try {
    return localStorage.getItem('miliastraTheme') === 'light'
  } catch {
    return false
  }
})()) {
  document.documentElement.dataset.theme = 'light'
} else {
  document.documentElement.dataset.theme = 'dark'
}

createApp(App).mount('#app')
