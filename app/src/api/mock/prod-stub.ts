import type { Backend } from '../backend'
import { createTauriBackend } from '../tauri'

/** 生产构建占位：确保 mock 数据与模拟文件系统不会被打入正式发行包。 */
export function createMockBackend(): Backend & { debug?: unknown } {
  return createTauriBackend()
}
