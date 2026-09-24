import { fileURLToPath, URL } from 'node:url'
import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// 开发期：/api 与 /ws 代理到本机 im-server（8080），
// 浏览器视角同源——生产由反向代理做同样的收口。
export default defineConfig({
  plugins: [vue()],
  resolve: {
    // `@` 别名与 tsconfig paths 保持一致（tsconfig 只影响类型检查，
    // 打包解析在这里配）
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  server: {
    port: 5173,
    proxy: {
      '/api': { target: 'http://127.0.0.1:8080', changeOrigin: true },
      '/ws': { target: 'ws://127.0.0.1:8080', ws: true },
    },
  },
})
