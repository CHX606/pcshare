import { useEffect, useMemo, useState } from 'react'
import type { DeviceId, ProcInfo } from '../api/types'
import { formatSize, formatTime } from '../lib/format'
import { isOnline, useDevice } from '../store/devices'
import { useRemote } from '../store/remote'
import { Icon } from '../components/Icon'
import { Button } from '../components/Modal'

type Key = 'name' | 'title' | 'memory' | 'cpu'
const REFRESH_MS = 3000

export function ProcsPage({ device }: { device: DeviceId }) {
  const dev = useDevice(device)
  const online = isOnline(dev)
  const state = useRemote((s) => s.procs[device])
  const { loadProcs, kill } = useRemote.getState()
  const [sort, setSort] = useState<{ key: Key; asc: boolean }>({ key: 'memory', asc: false })
  const [filter, setFilter] = useState('')

  // 页面可见且设备在线时，每 3 秒刷新一次；若上一次请求仍在途中则跳过避免堆积
  useEffect(() => {
    if (!online) return
    void loadProcs(device)
    const t = window.setInterval(() => {
      if (document.visibilityState !== 'visible') return
      if (useRemote.getState().procs[device]?.loading) return
      void loadProcs(device)
    }, REFRESH_MS)
    return () => window.clearInterval(t)
  }, [device, online, loadProcs])

  const rows = useMemo(() => {
    const q = filter.trim().toLowerCase()
    const list = (state?.data ?? []).filter((p) => !q || p.name.toLowerCase().includes(q) || p.title.toLowerCase().includes(q))
    const dir = sort.asc ? 1 : -1
    return list.sort((a, b) => {
      const r = sort.key === 'name' || sort.key === 'title' ? a[sort.key].localeCompare(b[sort.key], 'zh-CN') : a[sort.key] - b[sort.key]
      return r * dir || a.pid - b.pid
    })
  }, [state?.data, sort, filter])

  const th = (key: Key, label: string, cls = '') => (
    <th scope="col" aria-sort={sort.key === key ? (sort.asc ? 'ascending' : 'descending') : 'none'} className={`py-1 font-normal ${cls}`}>
      <button
        type="button"
        onClick={() => setSort((s) => ({ key, asc: s.key === key ? !s.asc : key === 'name' || key === 'title' }))}
        className={`inline-flex items-center gap-1 rounded px-1 py-1 hover:bg-hover ${cls.includes('text-right') ? 'flex-row-reverse' : ''}`}
      >
        {label}
        {sort.key === key && <Icon name={sort.asc ? 'up' : 'down'} size={12} />}
      </button>
    </th>
  )

  return (
    <section aria-label="进程" className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center gap-2 border-b border-line px-4 py-3">
        <div className="relative w-72">
          <Icon name="search" className="pointer-events-none absolute top-1/2 left-3 -translate-y-1/2 text-faint" />
          <input
            type="search"
            aria-label="筛选进程"
            placeholder="按名称或窗口标题筛选"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            className="h-8 w-full rounded-md border border-line bg-panel pr-3 pl-9 outline-none focus:border-accent"
          />
        </div>
        <span className="ml-auto text-xs text-muted">
          {state?.at ? `每 3 秒自动刷新 · 上次 ${formatTime(state.at).slice(-5)}` : ''}
        </span>
        <Button onClick={() => void loadProcs(device)} disabled={!online}>
          <Icon name="refresh" className={state?.loading ? 'spin' : ''} /> 刷新
        </Button>
      </div>

      {!state?.data ? (
        <p className="flex items-center justify-center gap-2 p-8 text-sm text-muted">
          {state?.error ?? (online ? (
            <>
              <Icon name="loader" className="spin text-accent" /> 正在读取进程…
            </>
          ) : (
            '设备离线，等它上线后再看'
          ))}
        </p>
      ) : (
        <div className={`scroll-thin flex-1 overflow-y-auto ${online ? '' : 'opacity-60'}`}>
          <table className="w-full table-fixed border-collapse text-sm">
            <thead className="sticky top-0 bg-bg text-left text-xs text-muted">
              <tr className="border-b border-line">
                {th('name', '名称', 'w-[22%] pl-4')}
                {th('title', '窗口标题')}
                {th('memory', '内存', 'w-28 text-right')}
                {th('cpu', '处理器', 'w-24 text-right')}
                <th scope="col" className="w-28 pr-4">
                  <span className="sr-only">操作</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {rows.map((p) => (
                <Row key={p.pid} p={p} disabled={!online} onKill={() => void kill(device, p)} />
              ))}
            </tbody>
          </table>
          {rows.length === 0 && <p className="p-8 text-center text-sm text-muted">没有匹配的进程</p>}
        </div>
      )}
    </section>
  )
}

function Row({ p, disabled, onKill }: { p: ProcInfo; disabled: boolean; onKill: () => void }) {
  return (
    <tr className="group border-b border-line/60 hover:bg-hover">
      <td className="truncate py-1.5 pl-5" title={`进程号 ${p.pid}`}>
        {p.name}
        {p.critical && <Icon name="shield" size={13} className="ml-1.5 inline text-faint" aria-label="系统关键进程" />}
      </td>
      <td className="truncate text-muted">{p.title}</td>
      <td className="pr-1 text-right tabular-nums">{formatSize(p.memory)}</td>
      <td className={`pr-1 text-right tabular-nums ${p.cpu > 50 ? 'text-bad' : ''}`}>{p.cpu.toFixed(1)}%</td>
      <td className="pr-4 text-right">
        <button
          type="button"
          disabled={disabled || p.critical}
          title={p.critical ? '系统关键进程，不能结束' : `结束 ${p.name}`}
          onClick={onKill}
          className="rounded px-2 py-0.5 text-xs text-bad opacity-0 group-focus-within:opacity-100 group-hover:opacity-100 hover:bg-bad/10 focus-visible:opacity-100 disabled:text-faint"
        >
          结束进程
        </button>
      </td>
    </tr>
  )
}
