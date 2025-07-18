import { sentryVitePlugin } from "@sentry/vite-plugin";
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from 'path'

export default defineConfig({
  plugins: [react(), sentryVitePlugin({
    org: "bloop-ai",
    project: "vibe-kanban"
  })],

  // 从环境变量读取 base 路径
  base: process.env.VITE_BASE_PATH || '/',

  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
      "shared": path.resolve(__dirname, "../shared"),
    },
  },

  server: {
    port: parseInt(process.env.FRONTEND_PORT || '3000'),
    proxy: {
      '/api': {
        target: `http://localhost:${process.env.BACKEND_PORT || '3002'}`,
        changeOrigin: true,
      },
    },
  },

  build: {
    sourcemap: true
  }
})
