import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// 端口对齐 Tauri 默认(1420),以后接壳不用改
export default defineConfig({
  plugins: [vue()],
  // 端口默认 1420(给 Tauri / 本地 dev);MCP 预览会用 PORT 环境变量走别的端口,互不抢
  server: { port: process.env.PORT ? Number(process.env.PORT) : 1420, strictPort: false },
})
