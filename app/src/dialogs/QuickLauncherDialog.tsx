import { useEffect, useState } from 'react'
import { backend } from '../api'
import { humanError } from '../api/errors'
import type { QuickHit } from '../api/types'
import { Icon } from '../components/Icon'
import { Modal } from '../components/Modal'
import { parseQuickInputHint } from '../lib/stage3'
import { isOnline, useDevices } from '../store/devices'
import { useFiles } from '../store/files'
import { toast } from '../store/toasts'
import { useUi } from '../store/ui'

/** 任务 3.1：全局快捷搜索框（默认 Alt+Space 呼出，输入“设备名 关键字”，回车立即远端启动并收起）。 */
export function QuickLauncherDialog() {
  const open = useUi((s) => s.launcherOpen)
  const openLauncher = useUi((s) => s.openLauncher)
  const selectDevice = useDevices((s) => s.select)
  const devices = useDevices((s) => s.devices)
  const setTab = useUi((s) => s.setTab)

  const [query, setQuery] = useState('')
  const [hits, setHits] = useState<QuickHit[]>([])
  const [activeIndex, setActiveIndex] = useState(0)

  // 订阅后端全局热键事件（launcher://toggle）并在启动时消费托盘唤醒挂起标记
  useEffect(() => {
    void backend
      .launcherConsumePending()
      .then((pending) => {
        if (pending) useUi.getState().openLauncher(true)
      })
      .catch(() => {})

    const off = backend.on('launcher-toggle', () => {
      useUi.getState().toggleLauncher()
    })

    const onWindowKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault()
        useUi.getState().toggleLauncher()
      }
    }
    window.addEventListener('keydown', onWindowKey)
    return () => {
      off()
      window.removeEventListener('keydown', onWindowKey)
    }
  }, [])

  useEffect(() => {
    if (!open) return
    let cancelled = false
    void backend
      .quickSearch(query, 12)
      .then((list) => {
        if (!cancelled) {
          setHits(list)
          setActiveIndex(0)
        }
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [open, query])

  const hint = parseQuickInputHint(
    query,
    devices.map((d) => ({ id: d.id, name: d.name, online: isOnline(d) })),
  )

  const executeHit = (hit: QuickHit) => {
    // 立即收起启动框，保证零等待手感（任务 3.1：回车执行并收起）
    openLauncher(false)
    setQuery('')
    if (hit.kind === 'folder') {
      selectDevice(hit.peerId)
      setTab('files')
      useFiles.getState().open(hit.peerId, hit.target)
      return
    }
    void backend
      .quickLaunch(hit.peerId, hit.target, hit.launchKind)
      .then(() => {
        toast.success(`已在「${hit.peerName}」启动 ${hit.title}`)
      })
      .catch((err) => {
        toast.error(humanError(err))
      })
  }

  const onInputKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      if (hits.length) setActiveIndex((i) => (i + 1) % hits.length)
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      if (hits.length) setActiveIndex((i) => (i - 1 + hits.length) % hits.length)
    } else if (e.key === 'Enter') {
      e.preventDefault()
      const chosen = hits[activeIndex]
      if (chosen) executeHit(chosen)
    }
  }

  return (
    <Modal
      open={open}
      onClose={() => openLauncher(false)}
      title="快捷启动（Alt+Space）"
      closedBy="any"
      width={540}
    >
      <div className="flex flex-col gap-3">
        <search>
          <div className="flex items-center gap-2 rounded-lg border border-line bg-bg px-3 py-2 focus-within:border-accent">
            <Icon name="search" size={16} className="shrink-0 text-muted" />
            <input
              data-autofocus
              type="search"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={onInputKeyDown}
              placeholder="输入“设备名 关键字”（支持拼音首字母，如 sf wx；单台在线可省设备名）"
              aria-label="快捷搜索各设备软件与最近文件夹"
              className="w-full bg-transparent text-sm outline-none"
            />
          </div>
        </search>

        <div className="flex items-center justify-between text-xs text-muted">
          <span>
            {hint.scopedDeviceName
              ? `已锁定目标设备：${hint.scopedDeviceName}`
              : hint.defaultSingleOnlineName
                ? `当前仅「${hint.defaultSingleOnlineName}」在线，将优先在它上面启动`
                : '跨全部设备检索已装软件与最近打开过的文件夹'}
          </span>
          <span>↑↓ 选择 · Enter 启动</span>
        </div>

        <ul
          role="listbox"
          aria-label="快捷搜索结果"
          className="scroll-thin max-h-72 overflow-y-auto rounded-lg border border-line"
        >
          {hits.length === 0 ? (
            <li className="px-4 py-8 text-center text-sm text-muted">
              没有找到匹配的软件或最近文件夹
            </li>
          ) : (
            hits.map((h, i) => {
              const selected = i === activeIndex
              return (
                <li key={`${h.peerId}|${h.kind}|${h.target}`} role="option" aria-selected={selected}>
                  <button
                    type="button"
                    onMouseEnter={() => setActiveIndex(i)}
                    onClick={() => executeHit(h)}
                    className={`flex w-full items-center gap-3 px-3.5 py-2.5 text-left text-sm ${
                      selected ? 'bg-accent-soft text-fg' : 'hover:bg-hover'
                    }`}
                  >
                    <Icon
                      name={h.kind === 'folder' ? 'folder' : 'grid'}
                      size={18}
                      className={h.online ? 'text-accent' : 'text-faint'}
                    />
                    <span className="min-w-0 flex-1">
                      <span className="block truncate font-medium">{h.title}</span>
                      <span className="block truncate text-xs text-muted">{h.subtitle}</span>
                    </span>
                    <span
                      className={`shrink-0 rounded px-1.5 py-0.5 text-[11px] ${
                        h.online ? 'bg-ok/15 text-ok' : 'bg-hover text-faint'
                      }`}
                    >
                      {h.online ? '在线' : '离线'}
                    </span>
                  </button>
                </li>
              )
            })
          )}
        </ul>
      </div>
    </Modal>
  )
}
