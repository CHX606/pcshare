import type { DeviceId } from '../api/types'
import { ROOT } from '../lib/path'
import { statusText } from '../lib/status'
import { useDevice } from '../store/devices'
import { keyOf, useFiles, viewOf } from '../store/files'
import { useTasks } from '../store/tasks'
import { useUi } from '../store/ui'
import { StatusDot } from './DeviceStatus'
import { Icon } from './Icon'

/** 独立订阅传输队列摘要，仅在活跃任务数或整数百分比变化时重渲，避免高频进度推送拖累整条状态栏。 */
function TransferSummaryButton() {
  const summary = useTasks((s) => {
    let activeCount = 0
    let done = 0
    let total = 0
    for (const t of s.tasks) {
      if (t.state === 'running' || t.state === 'paused') {
        activeCount++
        done += t.bytesDone
        total += t.bytesTotal
      }
    }
    if (activeCount === 0) return ''
    const pct = Math.round((done / Math.max(1, total)) * 100)
    return `传输队列 ${activeCount} 个 · ${pct}%`
  })
  const panelOpen = useTasks((s) => s.panelOpen)
  const setPanel = useTasks((s) => s.setPanel)

  return (
    <button
      type="button"
      aria-expanded={panelOpen}
      onClick={() => setPanel(!panelOpen)}
      className={`flex h-6 items-center gap-1.5 rounded px-2 hover:bg-hover ${summary ? 'text-accent' : ''}`}
    >
      <Icon name="transfer" size={14} />
      {summary || '传输'}
    </button>
  )
}

/** 底部状态条：项数、选中数、连接与重连状态、传输入口。 */
export function StatusBar({ device }: { device: DeviceId }) {
  const dev = useDevice(device)
  const tab = useUi((s) => s.tab)
  const counts = useFiles((s) => {
    const v = viewOf(s, device, 'left')
    if (v.path === ROOT) return null
    const k = keyOf(device, v.path)
    const stream = s.streaming[k]
    const n = s.listings[k]?.entries.length
    if (n == null) return null
    const base = stream
      ? `已显示 ${stream.loaded}${stream.totalHint ? ` / ${stream.totalHint}` : ''} 项（加载中…）`
      : `${n} 项`
    return `${base}${v.selected.size ? `，已选 ${v.selected.size} 项` : ''}`
  })

  return (
    <footer className="flex h-8 shrink-0 items-center gap-4 border-t border-line bg-sidebar px-4 text-xs text-muted">
      <span className="tabular-nums">{tab === 'files' ? counts : ''}</span>
      <span className="ml-auto flex items-center gap-1.5">
        {dev && (
          <>
            <StatusDot device={dev} />
            {dev.name} · {statusText(dev)}
          </>
        )}
      </span>
      <TransferSummaryButton />
    </footer>
  )
}
