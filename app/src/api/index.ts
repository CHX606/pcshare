import { isTauri } from '@tauri-apps/api/core'
import type { Backend } from './backend'
import { createMockBackend } from './mock'
import { createTauriBackend } from './tauri'

function pick(): Backend {
  if (isTauri() || import.meta.env.PROD) return createTauriBackend()
  const mock = createMockBackend()
  ;(window as unknown as { __pcshareMock?: unknown }).__pcshareMock = mock.debug
  return mock
}

/** 全局唯一的后端。在 Tauri 窗口里走真实命令，在普通浏览器里走模拟数据。 */
export const backend: Backend = pick()
