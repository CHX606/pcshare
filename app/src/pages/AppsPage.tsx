import { useVirtualizer } from '@tanstack/react-virtual'
import { useEffect, useId, useMemo, useRef, useState } from 'react'
import { backend } from '../api'
import type { AppInfo, DeviceId } from '../api/types'
import { Icon } from '../components/Icon'
import { Button } from '../components/Modal'
import { isImeComposing } from '../lib/keyboard'
import { fuzzyScore } from '../lib/sort'
import { isOnline, useDevice } from '../store/devices'
import { useRemote } from '../store/remote'

const EMPTY: string[] = []
const ROW_HEIGHT = 48
const MAX_ICON_CACHE = 512
const MAX_ICON_CONCURRENCY = 6

/** 只长期缓存有效图标 URL（限制上限防常驻内存膨胀）；瞬态失败不缓存以便后续可重试。 */
const iconUrlCache = new Map<string, string>()

interface IconTask {
  key: string
  device: DeviceId
  appId: string
  resolve: (url: string | null) => void
  cancelled: boolean
}

const iconQueue: IconTask[] = []
const iconInflight = new Map<string, Promise<string | null>>()
let activeIconLoads = 0

function rememberIcon(key: string, url: string) {
  if (iconUrlCache.size >= MAX_ICON_CACHE) {
    const oldest = iconUrlCache.keys().next().value
    if (oldest !== undefined) iconUrlCache.delete(oldest)
  }
  iconUrlCache.set(key, url)
}

function pumpIconQueue() {
  while (activeIconLoads < MAX_ICON_CONCURRENCY && iconQueue.length > 0) {
    const task = iconQueue.shift()!
    if (task.cancelled) continue
    const cached = iconUrlCache.get(task.key)
    if (cached) {
      task.resolve(cached)
      continue
    }
    const existing = iconInflight.get(task.key)
    if (existing) {
      void existing.then((u) => task.resolve(u))
      continue
    }
    activeIconLoads++
    const p = backend
      .appIcon(task.device, task.appId)
      .then((url) => {
        if (url) rememberIcon(task.key, url)
        return url
      })
      .catch(() => null)
      .finally(() => {
        iconInflight.delete(task.key)
        activeIconLoads--
        pumpIconQueue()
      })
    iconInflight.set(task.key, p)
    void p.then((u) => task.resolve(u))
  }
}

function enqueueIconLoad(
  device: DeviceId,
  appId: string,
  onReady: (url: string | null) => void,
): () => void {
  const key = `${device}|${appId}`
  const task: IconTask = { key, device, appId, resolve: onReady, cancelled: false }
  iconQueue.push(task)
  pumpIconQueue()
  return () => {
    task.cancelled = true
  }
}

function AppAvatar({ device, app }: { device: DeviceId; app: AppInfo }) {
  const cacheKey = `${device}|${app.id}`
  const [fetched, setFetched] = useState<{ key: string; url: string | null } | null>(null)
  const iconUrl = iconUrlCache.get(cacheKey) ?? (fetched?.key === cacheKey ? fetched.url : undefined)

  useEffect(() => {
    if (iconUrlCache.has(cacheKey)) return
    let alive = true
    const cancel = enqueueIconLoad(device, app.id, (url) => {
      if (alive) setFetched({ key: cacheKey, url })
    })
    return () => {
      alive = false
      cancel()
    }
  }, [cacheKey, device, app.id])

  if (iconUrl) {
    return <img src={iconUrl} alt="" width={32} height={32} className="size-8 shrink-0 rounded-lg object-contain" />
  }

  let h = 0
  for (const c of app.name) h = (h * 31 + c.charCodeAt(0)) % 360
  return (
    <span
      aria-hidden="true"
      className="grid size-8 shrink-0 place-items-center rounded-lg text-sm font-medium text-white"
      style={{ background: `oklch(0.62 0.13 ${h})` }}
    >
      {app.name.slice(0, 1).toUpperCase()}
    </span>
  )
}

export function AppsPage({ device }: { device: DeviceId }) {
  const dev = useDevice(device)
  const online = isOnline(dev)
  const state = useRemote((s) => s.apps[device])
  const favorites = useRemote((s) => s.favorites[device] ?? EMPTY)
  const launching = useRemote((s) => s.launching)
  const { loadApps, launch, toggleFavorite } = useRemote.getState()
  const [query, setQuery] = useState('')
  const [active, setActive] = useState(0)
  const input = useRef<HTMLInputElement>(null)
  const listRef = useRef<HTMLUListElement>(null)
  const listId = useId()

  useEffect(() => {
    if (online) void loadApps(device)
  }, [device, online, loadApps])

  useEffect(() => {
    input.current?.focus()
  }, [device])

  const favSet = useMemo(() => new Set(favorites), [favorites])

  const results = useMemo(() => {
    return (state?.data ?? [])
      .map((a) => ({ a, score: fuzzyScore(a.name, query), fav: favSet.has(a.id) }))
      .filter((x) => x.score >= 0)
      .sort((x, y) => Number(y.fav) - Number(x.fav) || x.score - y.score || x.a.name.localeCompare(y.a.name, 'zh-CN'))
      .map((x) => x.a)
  }, [state?.data, query, favSet])

  const activeIdx = Math.min(active, Math.max(0, results.length - 1))
  const open = (a: AppInfo | undefined) => a && online && void launch(device, a)

  // oxlint-disable-next-line react/incompatible-library
  const virt = useVirtualizer({
    count: results.length,
    getScrollElement: () => listRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 8,
  })

  useEffect(() => {
    if (results.length > 0) virt.scrollToIndex(activeIdx, { align: 'auto' })
  }, [activeIdx, results.length, virt])

  return (
    <section aria-label="软件" className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center gap-2 border-b border-line px-4 py-3">
        <div className="relative flex-1">
          <Icon name="search" className="pointer-events-none absolute top-1/2 left-3 -translate-y-1/2 text-faint" />
          <input
            ref={input}
            type="search"
            role="combobox"
            aria-expanded="true"
            aria-controls={listId}
            aria-activedescendant={results.length ? `${listId}-${activeIdx}` : undefined}
            aria-label="搜索软件"
            placeholder={online ? `在“${dev?.name}”上找软件，回车打开第一个` : '设备离线，暂时不能打开软件'}
            value={query}
            onChange={(e) => {
              setQuery(e.target.value)
              setActive(0)
            }}
            onKeyDown={(e) => {
              if (isImeComposing(e)) return
              if (e.key === 'ArrowDown') {
                e.preventDefault()
                setActive(Math.min(results.length - 1, activeIdx + 1))
              } else if (e.key === 'ArrowUp') {
                e.preventDefault()
                setActive(Math.max(0, activeIdx - 1))
              } else if (e.key === 'Enter') {
                e.preventDefault()
                open(results[activeIdx])
              } else if (e.key === 'Escape' && query) {
                e.preventDefault()
                setQuery('')
              } else if (e.ctrlKey && e.key.toLowerCase() === 'd' && results[activeIdx]) {
                e.preventDefault()
                toggleFavorite(device, results[activeIdx].id)
              }
            }}
            className="h-10 w-full rounded-lg border border-line bg-panel pr-3 pl-9 outline-none focus:border-accent"
          />
        </div>
        <Button onClick={() => void loadApps(device, true)} disabled={!online || state?.loading} title="重新读取软件列表">
          <Icon name="refresh" className={state?.loading ? 'spin' : ''} /> 刷新
        </Button>
      </div>

      {state?.error && !state.data ? (
        <p className="p-8 text-center text-sm text-muted">{state.error}</p>
      ) : !state?.data ? (
        <p className="flex items-center justify-center gap-2 p-8 text-sm text-muted">
          {online ? (
            <>
              <Icon name="loader" className="spin text-accent" /> 正在读取已装软件…
            </>
          ) : (
            '设备离线，等它上线后再看'
          )}
        </p>
      ) : (
        <ul
          ref={listRef}
          id={listId}
          role="listbox"
          aria-label="软件列表"
          className={`scroll-thin flex-1 overflow-y-auto p-2 ${online ? '' : 'opacity-60'}`}
        >
          {results.length === 0 ? (
            <li className="p-8 text-center text-sm text-muted">没有找到“{query}”</li>
          ) : (
            <div style={{ height: virt.getTotalSize(), position: 'relative' }}>
              {virt.getVirtualItems().map((row) => {
                const a = results[row.index]
                const fav = favSet.has(a.id)
                const isActive = row.index === activeIdx
                return (
                  <li
                    key={a.id}
                    id={`${listId}-${row.index}`}
                    role="option"
                    aria-selected={isActive}
                    onMouseMove={() => !isActive && setActive(row.index)}
                    onDoubleClick={() => open(a)}
                    style={{ height: ROW_HEIGHT, transform: `translateY(${row.start}px)` }}
                    className={`group absolute inset-x-0 top-0 flex items-center gap-3 rounded-lg px-3 ${
                      isActive ? 'bg-accent-soft' : ''
                    }`}
                  >
                    <AppAvatar device={device} app={a} />
                    <span className="min-w-0 flex-1 truncate">{a.name}</span>
                    {row.index === 0 && query && (
                      <kbd className="rounded border border-line px-1.5 text-xs text-muted">Enter</kbd>
                    )}
                    <button
                      type="button"
                      tabIndex={-1}
                      aria-label={fav ? `取消收藏 ${a.name}` : `收藏 ${a.name}`}
                      title={fav ? '取消收藏（Ctrl+D）' : '收藏，排到最前（Ctrl+D）'}
                      aria-pressed={fav}
                      onClick={() => toggleFavorite(device, a.id)}
                      className={`rounded p-1 hover:bg-hover ${fav ? 'text-warn' : 'text-faint opacity-0 group-hover:opacity-100'}`}
                    >
                      <Icon name="star" fill={fav ? 'currentColor' : 'none'} />
                    </button>
                    <Button
                      tabIndex={-1}
                      variant={isActive ? 'primary' : 'default'}
                      disabled={!online || launching === a.id}
                      onClick={() => open(a)}
                    >
                      {launching === a.id ? <Icon name="loader" className="spin" /> : <Icon name="play" size={13} />}
                      打开
                    </Button>
                  </li>
                )
              })}
            </div>
          )}
        </ul>
      )}
    </section>
  )
}
