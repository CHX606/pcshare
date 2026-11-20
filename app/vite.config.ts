import { fileURLToPath } from 'node:url'
import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig, type Plugin } from 'vite'

const mockStubPath = fileURLToPath(new URL('./src/api/mock/prod-stub.ts', import.meta.url))
const mockEntryPath = fileURLToPath(new URL('./src/api/mock/index.ts', import.meta.url))

/** 生产构建剥离 mock 后端与模拟数据，同时保留 npm run dev 与 vitest 下的模拟能力。 */
function stripMockInBuild(): Plugin {
  const norm = (p: string) => p.replace(/\\/g, '/').toLowerCase()
  const targetEntry = norm(mockEntryPath)
  return {
    name: 'strip-mock-in-build',
    apply: 'build',
    enforce: 'pre',
    async resolveId(source, importer, options) {
      if (!importer || !source.includes('mock')) return null
      const resolved = await this.resolve(source, importer, { ...options, skipSelf: true })
      if (resolved && norm(resolved.id) === targetEntry) {
        return mockStubPath
      }
      return null
    },
  }
}

// Tauri 开发时固定端口；构建目标对齐 Windows 上的 WebView2（Chromium 内核）
export default defineConfig({
  plugins: [stripMockInBuild(), react(), tailwindcss()],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: { target: 'chrome120' },
})
