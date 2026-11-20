import type { CSSProperties } from 'react'
import type { TaskProgress } from '../api/types'
import { formatDuration, formatSize, formatSpeed } from '../lib/format'
import { useDeviceName } from '../store/devices'
import { useTasks } from '../store/tasks'
import { Icon, type IconName } from './Icon'

const KIND: Record<TaskProgress['kind'], { icon: IconName; label: string }> = {
  upload: { icon: 'upload', label: '上传' },
  download: { icon: 'download', label: '下载' },
  relay: { icon: 'transfer', label: '直传' },
  copy: { icon: 'copy', label: '复制' },
  move: { icon: 'cut', label: '移动' },
}

const STATE_TEXT = {
  paused: '已暂停（支持断点续传）',
  done: '已完成',
  cancelled: '已取消',
  failed: '失败',
} as const

const SPEED_OPTIONS: Array<{ label: string; bps: number | null }> = [
  { label: '不限速', bps: null },
  { label: '限速 5 MB/s', bps: 5 * 1024 * 1024 },
  { label: '限速 20 MB/s', bps: 20 * 1024 * 1024 },
  { label: '限速 50 MB/s', bps: 50 * 1024 * 1024 },
]

export function TransferPanel() {
  const tasks = useTasks((s) => s.tasks)
  const speedLimitBps = useTasks((s) => s.speedLimitBps)
  const { pause, resume, cancel, setSpeedLimit, clearFinished, setPanel } = useTasks.getState()
  const activeCount = tasks.filter((t) => t.state === 'running' || t.state === 'paused').length
  const finished = tasks.some((t) => t.state !== 'running' && t.state !== 'paused')

  return (
    <section aria-label="传输" className="flex h-60 shrink-0 flex-col border-t border-line bg-panel">
      <header className="flex items-center gap-2 border-b border-line/60 px-4 py-1.5 text-sm">
        <h2 className="font-medium">传输队列</h2>
        <span className="text-xs text-muted">{activeCount} 个进行中（4 路并发 · 断点续传）</span>
        <label className="ml-auto flex items-center gap-1.5 text-xs text-muted">
          限速
          <select
            aria-label="传输限速"
            value={speedLimitBps ?? ''}
            onChange={(e) => {
              const raw = e.target.value
              void setSpeedLimit(raw === '' ? null : Number(raw))
            }}
            className="h-6 rounded border border-line bg-panel px-1.5 text-xs text-fg outline-none focus:border-accent"
          >
            {SPEED_OPTIONS.map((opt) => (
              <option key={opt.label} value={opt.bps ?? ''}>
                {opt.label}
              </option>
            ))}
          </select>
        </label>
        <button
          type="button"
          disabled={!finished}
          onClick={clearFinished}
          className="rounded px-2 py-1 text-xs text-muted hover:bg-hover disabled:opacity-40"
        >
          清除已结束
        </button>
        <button
          type="button"
          aria-label="收起传输面板"
          onClick={() => setPanel(false)}
          className="rounded p-1 text-muted hover:bg-hover"
        >
          <Icon name="down" size={14} />
        </button>
      </header>
      <ul className="scroll-thin flex-1 overflow-y-auto px-2 py-1.5">
        {tasks.length === 0 && (
          <li className="p-6 text-center text-sm text-muted">
            没有传输任务。可从资源管理器直接拖入文件或文件夹，或在双栏间拖拽互传。
          </li>
        )}
        {tasks.map((t) => (
          <TaskRow
            key={t.id}
            t={t}
            onPause={() => void pause(t.id)}
            onResume={() => void resume(t.id)}
            onCancel={() => void cancel(t.id)}
          />
        ))}
      </ul>
    </section>
  )
}

function TaskRow({
  t,
  onPause,
  onResume,
  onCancel,
}: {
  t: TaskProgress
  onPause: () => void
  onResume: () => void
  onCancel: () => void
}) {
  const pct = Math.min(100, Math.max(0, (t.bytesDone / Math.max(1, t.bytesTotal)) * 100))
  const running = t.state === 'running'
  const paused = t.state === 'paused'
  const isTransfer = t.kind === 'upload' || t.kind === 'download' || t.kind === 'relay'
  const srcName = useDeviceName(t.srcDevice)
  const dstName = useDeviceName(t.dstDevice)
  const route =
    t.srcDevice === t.dstDevice ? `在“${srcName}”上` : `${srcName} → ${dstName}`
  const remain = running && t.speed > 0 ? formatDuration((t.bytesTotal - t.bytesDone) / t.speed) : ''

  const barColor =
    t.state === 'failed'
      ? 'var(--color-bad)'
      : t.state === 'cancelled'
        ? 'var(--color-faint)'
        : t.state === 'paused'
          ? 'var(--color-warn)'
          : t.state === 'done'
            ? 'var(--color-ok)'
            : 'var(--color-accent)'

  return (
    <li className="flex items-center gap-3 rounded-md px-2 py-2 hover:bg-hover">
      <Icon name={KIND[t.kind].icon} className="shrink-0 text-muted" />
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-2 text-sm">
          <span className="truncate">{t.title}</span>
          <span className="shrink-0 text-xs text-muted">
            {KIND[t.kind].label} · {route}
          </span>
        </div>
        <progress
          className="task-progress mt-1.5"
          aria-label={`${KIND[t.kind].label} ${t.title}`}
          max={100}
          value={Math.round(pct)}
          style={{ '--bar-color': barColor } as CSSProperties}
        />
        <div className="mt-1 text-xs text-muted tabular-nums">
          {t.state === 'running' ? (
            `${formatSize(t.bytesDone)} / ${formatSize(t.bytesTotal)} · ${formatSpeed(t.speed)}${remain ? ` · 还剩 ${remain}` : ''}`
          ) : t.state === 'paused' ? (
            <span className="text-warn">
              {STATE_TEXT.paused} · {formatSize(t.bytesDone)} / {formatSize(t.bytesTotal)}
            </span>
          ) : t.state === 'failed' ? (
            <span className="text-bad">
              {STATE_TEXT.failed}：{t.error?.msg}
            </span>
          ) : (
            `${STATE_TEXT[t.state]} · ${formatSize(t.state === 'done' ? t.bytesTotal : t.bytesDone)}`
          )}
        </div>
      </div>
      <div className="flex shrink-0 items-center gap-1">
        {isTransfer && running && (
          <button
            type="button"
            onClick={onPause}
            aria-label={`暂停 ${t.title}`}
            className="rounded px-2 py-1 text-xs text-muted hover:bg-panel hover:text-fg"
          >
            暂停
          </button>
        )}
        {isTransfer && paused && (
          <button
            type="button"
            onClick={onResume}
            aria-label={`继续 ${t.title}`}
            className="rounded px-2 py-1 text-xs text-accent hover:bg-panel"
          >
            继续
          </button>
        )}
        {(running || paused) && (
          <button
            type="button"
            onClick={onCancel}
            aria-label={`取消 ${t.title}`}
            className="rounded px-2 py-1 text-xs text-muted hover:bg-panel hover:text-bad"
          >
            取消
          </button>
        )}
      </div>
    </li>
  )
}
