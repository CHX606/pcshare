import { useShallow } from 'zustand/react/shallow'
import type { DeviceId } from '../api/types'
import { ROOT } from '../lib/path'
import { statusText } from '../lib/status'
import { useDevice } from '../store/devices'
import { keyOf, useFiles, viewOf } from '../store/files'
import { useTasks } from '../store/tasks'
import { useUi } from '../store/ui'
import { StatusDot } from './DeviceStatus'
import { Icon } from './Icon'

/** 底部状态条：项数、选中数、连接与重连状态、传输入口。 */
export function StatusBar({ device }: { device: DeviceId }) {
  const dev = useDevice(device)
  const tab = useUi((s) => s.tab)
  const counts = useFiles((s) => {
    const v = viewOf(s, device)
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
  const active = useTasks(useShallow((s) => s.tasks.filter((t) => t.state === 'running' || t.state === 'paused')))
  const panelOpen = useTasks((s) => s.panelOpen)
  const setPanel = useTasks((s) => s.setPanel)
  const pct = active.length
    ? Math.round(
        (active.reduce((n, t) => n + t.bytesDone, 0) /
          Math.max(1, active.reduce((n, t) => n + t.bytesTotal, 0))) *
          100,
      )
    : null

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
      <button
        type="button"
        aria-expanded={panelOpen}
        onClick={() => setPanel(!panelOpen)}
        className={`flex h-6 items-center gap-1.5 rounded px-2 hover:bg-hover ${active.length ? 'text-accent' : ''}`}
      >
        <Icon name="transfer" size={14} />
        {active.length ? `传输队列 ${active.length} 个 · ${pct}%` : '传输'}
      </button>
    </footer>
  )
}
