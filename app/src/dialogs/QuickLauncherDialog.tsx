import { useEffect, useId, useState } from 'react'
import { backend } from '../api'
import { humanError } from '../api/errors'
import { SELF_ID, type QuickHit } from '../api/types'
import { Icon } from '../components/Icon'
import { Modal } from '../components/Modal'
import { isImeComposing, shouldSkipGlobalShortcut } from '../lib/keyboard'
import { parseQuickInputHint } from '../lib/stage3'
import { isOnline, useDevices } from '../store/devices'
import { useFiles } from '../store/files'
import { toast } from '../store/toasts'
import { useUi } from '../store/ui'

/** 任务 3.1：全局快捷搜索框（默认热键呼出，输入“设备名 关键字”，回车立即远端启动并收起）。 */
export function QuickLauncherDialog() {
  const open = useUi((s) => s.launcherOpen)
  const quickHotkey = useUi((s) => s.quickHotkey)
  const openLauncher = useUi((s) => s.openLauncher)
  const selectDevice = useDevices((s) => s.select)
  const devices = useDevices((s) => s.devices)
  const setTab = useUi((s) => s.setTab)

  const [query, setQuery] = useState('')
  const [hits, setHits] = useState<QuickHit[]>([])
  const [activeIndex, setActiveIndex] = useState(0)
  const listId = useId()

  // 订阅后端全局热键事件（launcher://toggle）并在启动时消费托盘唤醒挂起标记
  useEffect(() => {
    let active = true
    let toggledByEvent = false

    const off = backend.on('launcher-toggle', () => {
      toggledByEvent = true
      useUi.getState().toggleLauncher()
    })

    void backend
      .launcherConsumePending()
      .then((pending) => {
        if (active && pending && !toggledByEvent) {
          useUi.getState().openLauncher(true)
        }
      })
      .catch(() => {})

    const onWindowKey = (e: KeyboardEvent) => {
      if (shouldSkipGlobalShortcut(e)) return
      if ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === 'k') {
        e.preventDefault()
        useUi.getState().toggleLauncher()
      }
    }
    window.addEventListener('keydown', onWindowKey)
    return () => {
      active = false
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

  useEffect(() => {
    if (!open || hits.length === 0) return
    document.getElementById(`${listId}-${activeIndex}`)?.scrollIntoView({ block: 'nearest' })
  }, [open, activeIndex, hits.length, listId])

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
      useFiles.getState().open(hit.peerId, hit.target, 'push', 'left')
      return
    }
    const where = hit.peerId === SELF_ID ? '本机' : `「${hit.peerName}」`
    void backend
      .quickLaunch(hit.peerId, hit.target, hit.launchKind)
      .then((r) => {
        if (r.elevationPrompt) {
          toast.warn(
            `「${hit.title}」需要管理员权限，${where}的屏幕上弹出了确认框，需要有人在那台电脑前点“是”。也可以在对方的设置里打开“以管理员权限随登录启动”`,
          )
        } else if (r.desktop.place === 'locked') {
          toast.warn(`已在${where}启动「${hit.title}」，但对方现在是锁屏状态，屏幕上看不到`)
        } else if (r.desktop.place !== 'normal') {
          toast.warn(`已在${where}启动「${hit.title}」，但对方现在没有可见的桌面`)
        } else if (r.newWindows.length === 0) {
          toast.info(`已在${where}打开「${hit.title}」，没检测到新窗口，可能本来就开着`)
        } else {
          toast.success(`已在${where}启动「${hit.title}」`)
        }
      })
      .catch((err) => {
        toast.error(humanError(err))
      })
  }

  const onInputKeyDown = (e: React.KeyboardEvent) => {
    if (isImeComposing(e)) return
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

  const hotkeyLabel = quickHotkey || 'Ctrl+K'

  return (
    <Modal
      open={open}
      onClose={() => openLauncher(false)}
      title={`快捷启动（${hotkeyLabel}）`}
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
          id={listId}
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
                <li
                  key={`${h.peerId}|${h.kind}|${h.target}`}
                  id={`${listId}-${i}`}
                  role="option"
                  aria-selected={selected}
                >
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
