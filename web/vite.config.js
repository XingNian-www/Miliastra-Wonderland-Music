import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// 产物由 Rust build.rs 嵌入 exe；base 必须是相对路径。
export default defineConfig({
  plugins: [vue()],
  base: './',
  build: {
    outDir: 'dist',
    assetsDir: 'assets',
    emptyOutDir: true,
    target: 'es2019',
    cssCodeSplit: false,
    rollupOptions: {
      output: {
        entryFileNames: 'assets/app-[hash].js',
        chunkFileNames: 'assets/chunk-[hash].js',
        assetFileNames: 'assets/[name]-[hash][extname]',
      },
    },
  },
  server: {
    port: 5173,
    // 生产环境同源；开发时把面板会调用到的接口前缀全部代理到本机面板端口。
    proxy: {
      '^/(health|status|monitor|history|clear-history|state|queue|search|searchPlay|searchSource|play|pause|volume|skip-next|skip-prev|player|playback|operator|commands|config|debug|decisions|identity|tasks|emoji|ai|tts|tools|events)(/|\\?|$)': {
        target: 'http://127.0.0.1:18888',
        changeOrigin: true,
      },
    },
  },
})
