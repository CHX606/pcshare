import { useVirtualizer } from '@tanstack/react-virtual'
import { useCallback, useEffect, useId, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { backend } from '../../api'
import { humanError } from '../../api/errors'
import { SELF_ID, type DeviceId, type FsEntry } from '../../api/types'
import { ContextMenu, type MenuItem, type MenuState } from '../../components/ContextMenu'
import { Icon } from '../../components/Icon'
import { formatSize, formatTime, typeName } from '../../lib/format'
import { joinPath } from '../../lib/path'
import { typeaheadIndex, type SortKey } from '../../lib/sort'
import { deviceName } from '../../store/devices'
import { keyOf, useFiles, viewOf, type PaneSide } from '../../store/files'
import { toast } from '../../store/toasts'
import { EntryIcon, RenameInput } from './RenameInput'

const ROW = 30
const COLS = 'grid grid-cols-[minmax(0,1fr)_140px_104px_88px] gap-2.5'

interface Props {
  device: DeviceId
  side: PaneSide
  entries: FsEntry[]
  /** 设备离线时只读显示 */
  readOnly: boolean
}

export function FileList({ device, side, entries, readOnly }: Props) {
  const scrollRef = useRef<HTMLDivElement>(null)
  const dragCleanupRef = useRef<(() => void) | null>(null)
  const listId = useId()
  const view = useFiles((s) => viewOf(s, device, side))
  const sort = useFiles((s) => s.sort)
  const clipboard = useFiles((s) => s.clipboard)
  const renaming = useFiles((s) =>
    s.renaming?.side === side && s.renaming.device === device ? s.renaming.name : null,
  )
  const act = useFiles.getState
  const [menu, setMenu] = useState<MenuState | null>(null)
  const [pointerDrag, setPointerDrag] = useState<{
    names: string[]
    x: number
    y: number
    targetDevice: DeviceId | null
    targetSide: PaneSide | null
  } | null>(null)
  const closeMenu = useCallback(() => setMenu(null), [])
  const typeBuf = useRef({ text: '', at: 0 })

  const names = useMemo(() => entries.map((e) => e.name), [entries])
  const indexOf = useMemo(() => new Map(names.map((n, i) => [n, i])), [names])
  const focusIdx = view.focus != null ? (indexOf.get(view.focus) ?? -1) : -1
  const selectedNames = () => names.filter((n) => view.selected.has(n))

  // oxlint-disable-next-line react/incompatible-library
  const virt = useVirtualizer({
    count: entries.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW,
    overscan: 12,
  })

  useEffect(() => {
    if (focusIdx >= 0) virt.scrollToIndex(focusIdx, { align: 'auto' })
  }, [focusIdx, virt])

  useEffect(() => {
    const a = document.activeElement
    if (!a || a === document.body) scrollRef.current?.focus({ preventScroll: true })
  }, [view.path])

  const wasRenaming = useRef(false)
  useEffect(() => {
    if (wasRenaming.current && !renaming) scrollRef.current?.focus({ preventScroll: true })
    wasRenaming.current = !!renaming
  }, [renaming])

  // 组件卸载时确保移除残留的 window 指针监听
  useEffect(() => {
    return () => {
      dragCleanupRef.current?.()
      dragCleanupRef.current = null
    }
  }, [])

  const selectOnly = (i: number) => {
    const n = names[i]
    act().select(device, new Set([n]), n, n, side)
  }

  const moveTo = (i: number, e: { shiftKey: boolean; ctrlKey: boolean }) => {
    if (!names.length) return
    const idx = Math.max(0, Math.min(names.length - 1, i))
    const n = names[idx]
    if (e.shiftKey) {
      const a = view.anchor != null ? (indexOf.get(view.anchor) ?? idx) : idx
      const [lo, hi] = a < idx ? [a, idx] : [idx, a]
      act().select(device, new Set(names.slice(lo, hi + 1)), n, undefined, side)
    } else if (e.ctrlKey) {
      act().select(device, view.selected, n, undefined, side)
    } else selectOnly(idx)
  }

  // 任务 2.5：双栏间自定义指针拖拽（移除原生 draggable，兼容同设备双栏与 WebView2）
  const startRowPointerTrack = (rowIndex: number, ev: React.PointerEvent) => {
    if (ev.button !== 0 || readOnly || renaming) return
    dragCleanupRef.current?.()
    const rowName = names[rowIndex]
    const startX = ev.clientX
    const startY = ev.clientY
    const srcDir = view.path
    let dragging = false
    let dragTargets: string[] = []

    const findPaneTargetAt = (x: number, y: number): { device: DeviceId; side: PaneSide } | null => {
      const el = document.elementFromPoint(x, y)?.closest<HTMLElement>('[data-pane-side][data-pane-device]')
      const dstDevice = el?.dataset.paneDevice
      const dstSide = el?.dataset.paneSide as PaneSide | undefined
      if (!dstDevice || !dstSide || dstSide === side) return null
      return { device: dstDevice, side: dstSide }
    }

    const cleanup = () => {
      window.removeEventListener('pointermove', onMove)
      window.removeEventListener('pointerup', onUp)
      window.removeEventListener('pointercancel', onCancel)
      if (dragCleanupRef.current === cleanup) dragCleanupRef.current = null
    }

    const onMove = (me: PointerEvent) => {
      const dx = me.clientX - startX
      const dy = me.clientY - startY
      if (!dragging && Math.hypot(dx, dy) >= 7) {
        dragging = true
        const curSel = viewOf(act(), device, side).selected
        dragTargets = curSel.has(rowName) ? names.filter((n) => curSel.has(n)) : [rowName]
      }
      if (dragging) {
        const hit = findPaneTargetAt(me.clientX, me.clientY)
        setPointerDrag({
          names: dragTargets,
          x: me.clientX,
          y: me.clientY,
          targetDevice: hit?.device ?? null,
          targetSide: hit?.side ?? null,
        })
      }
    }

    const onUp = (ue: PointerEvent) => {
      cleanup()
      if (dragging) {
        const hit = findPaneTargetAt(ue.clientX, ue.clientY)
        setPointerDrag(null)
        if (hit && dragTargets.length) {
          const dstPath = viewOf(act(), hit.device, hit.side).path
          void act().dropBetweenPanes(device, srcDir, dragTargets, hit.device, dstPath)
        }
      }
    }

    const onCancel = () => {
      cleanup()
      if (dragging) setPointerDrag(null)
    }

    dragCleanupRef.current = cleanup
    window.addEventListener('pointermove', onMove)
    window.addEventListener('pointerup', onUp)
    window.addEventListener('pointercancel', onCancel)
  }

  const onRowClick = (i: number, e: React.MouseEvent) => {
    const n = names[i]
    if (e.shiftKey) return moveTo(i, { shiftKey: true, ctrlKey: false })
    if (e.ctrlKey || e.metaKey) {
      const next = new Set(view.selected)
      if (next.has(n)) next.delete(n)
      else next.add(n)
      return act().select(device, next, n, n, side)
    }
    selectOnly(i)
  }

  const itemMenu = (targets: string[]): MenuItem[] => {
    const one = targets.length === 1 ? entries[indexOf.get(targets[0]) ?? -1] : undefined
    const fullTargets = targets.map((n) => joinPath(view.path, n))
    const openLocalEdit = async () => {
      if (!one || one.dir) return
      try {
        const sess = await backend.fsOpenLocalEdit(device, joinPath(view.path, one.name), true)
        toast.info(`已取回本机临时打开「${sess.fileName}」，保存修改后会提示是否存回对方电脑`)
      } catch (err) {
        toast.error(humanError(err))
      }
    }
    const dragOutToShell = async () => {
      if (device !== SELF_ID) {
        toast.info(
          targets.length === 1
            ? `正在拉取「${targets[0]}」准备拖出，请稍候…`
            : `正在拉取 ${targets.length} 项文件准备拖出，请稍候…`,
        )
      }
      try {
        const ok = await backend.fsDragOut(device, fullTargets)
        if (ok) toast.success('已将文件拖出到目标位置')
        else toast.info('已取消拖出')
      } catch (err) {
        toast.error(humanError(err))
      }
    }
    const copyFullPath = async () => {
      if (!one) return
      try {
        await navigator.clipboard.writeText(joinPath(view.path, one.name))
        toast.success('已复制路径')
      } catch {
        toast.error('复制路径失败')
      }
    }
    const openLabel = one?.dir ? '打开' : device === SELF_ID ? '打开' : '在对方电脑上打开'
    return [
      {
        label: openLabel,
        icon: 'play',
        hint: 'Enter',
        disabled: !one || readOnly,
        onSelect: () => one && void act().openEntry(device, one, side),
      },
      {
        label: '取回本机打开（保存可回传）',
        icon: 'pencil',
        disabled: !one || !!one.dir || readOnly || device === SELF_ID,
        onSelect: () => void openLocalEdit(),
      },
      {
        label: one?.dir ? '在新标签页打开' : '在新标签页浏览此目录',
        icon: 'plus',
        disabled: !one?.dir || readOnly,
        onSelect: () => one?.dir && act().newTab(device, joinPath(view.path, one.name), side),
      },
      {
        label: '下载到本机…',
        icon: 'download',
        disabled: readOnly || device === SELF_ID,
        onSelect: () => void act().download(device, targets, side),
      },
      {
        label: '拖出到系统桌面 / 外部…',
        icon: 'transfer',
        disabled: readOnly || !targets.length,
        onSelect: () => void dragOutToShell(),
      },
      'sep',
      { label: '复制', icon: 'copy', hint: 'Ctrl+C', disabled: readOnly, onSelect: () => act().copy(device, targets, false, side) },
      { label: '剪切', icon: 'cut', hint: 'Ctrl+X', disabled: readOnly, onSelect: () => act().copy(device, targets, true, side) },
      { label: '粘贴', icon: 'paste', hint: 'Ctrl+V', disabled: readOnly || !clipboard, onSelect: () => void act().paste(device, side) },
      'sep',
      { label: '重命名', icon: 'pencil', hint: 'F2', disabled: !one || readOnly, onSelect: () => one && act().startRename(device, one.name, side) },
      { label: '复制路径', icon: 'clipboard', disabled: !one, onSelect: () => void copyFullPath() },
      'sep',
      { label: '删除', icon: 'trash', hint: 'Delete', danger: true, disabled: readOnly, onSelect: () => void act().remove(device, targets, false, side) },
    ]
  }

  const blankMenu = (): MenuItem[] => [
    { label: '新建文件夹', icon: 'folderPlus', hint: 'Ctrl+Shift+N', disabled: readOnly, onSelect: () => void act().newFolder(device, side) },
    { label: '粘贴', icon: 'paste', hint: 'Ctrl+V', disabled: readOnly || !clipboard, onSelect: () => void act().paste(device, side) },
    { label: '从本机上传文件…', icon: 'upload', disabled: readOnly || device === SELF_ID, onSelect: () => void act().upload(device, side) },
    { label: '从本机上传文件夹…', icon: 'folder', disabled: readOnly || device === SELF_ID, onSelect: () => void act().uploadFolder(device, side) },
    'sep',
    { label: '刷新', icon: 'refresh', hint: 'F5', onSelect: () => act().refresh(device, side) },
  ]

  const onContextMenu = (e: React.MouseEvent, i: number | null) => {
    e.preventDefault()
    e.stopPropagation()
    if (i == null) return setMenu({ x: e.clientX, y: e.clientY, items: blankMenu() })
    const n = names[i]
    const targets = view.selected.has(n) ? selectedNames() : [n]
    if (!view.selected.has(n)) selectOnly(i)
    setMenu({ x: e.clientX, y: e.clientY, items: itemMenu(targets) })
  }

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (renaming) return
    const ctrl = e.ctrlKey || e.metaKey
    const page = Math.max(1, Math.floor((scrollRef.current?.clientHeight ?? 300) / ROW) - 1)
    const cur = focusIdx
    const handled = () => e.preventDefault()
    const k = e.key

    if (k === 'ArrowDown') return handled(), moveTo(cur + 1, e)
    if (k === 'ArrowUp' && !e.altKey) return handled(), moveTo(cur < 0 ? 0 : cur - 1, e)
    if (k === 'Home') return handled(), moveTo(0, e)
    if (k === 'End') return handled(), moveTo(names.length - 1, e)
    if (k === 'PageDown') return handled(), moveTo(cur + page, e)
    if (k === 'PageUp') return handled(), moveTo(cur - page, e)
    if (k === ' ' && ctrl && cur >= 0) {
      handled()
      const next = new Set(view.selected)
      if (next.has(names[cur])) next.delete(names[cur])
      else next.add(names[cur])
      return act().select(device, next, names[cur], names[cur], side)
    }
    if (k === 'Enter' && cur >= 0 && !readOnly) return handled(), void act().openEntry(device, entries[cur], side)
    if (k === 'Escape' && view.selected.size) return handled(), act().select(device, new Set(), view.focus, undefined, side)
    if (k === 'F2' && cur >= 0 && !readOnly) return handled(), act().startRename(device, names[cur], side)
    if (k === 'Delete' && view.selected.size && !readOnly) return handled(), void act().remove(device, selectedNames(), e.shiftKey, side)
    if (ctrl && k.toLowerCase() === 'a') return handled(), act().select(device, new Set(names), view.focus, undefined, side)
    if (ctrl && k.toLowerCase() === 'c' && !readOnly) return handled(), act().copy(device, selectedNames(), false, side)
    if (ctrl && k.toLowerCase() === 'x' && !readOnly) return handled(), act().copy(device, selectedNames(), true, side)
    if (ctrl && k.toLowerCase() === 'v' && !readOnly) return handled(), void act().paste(device, side)
    if (ctrl && e.shiftKey && k.toLowerCase() === 'n' && !readOnly) return handled(), void act().newFolder(device, side)
    if (k === 'ContextMenu' || (e.shiftKey && k === 'F10')) {
      handled()
      const r = scrollRef.current!.getBoundingClientRect()
      const rowTop = cur >= 0 ? r.top + cur * ROW - (scrollRef.current?.scrollTop ?? 0) + ROW : r.top + 8
      const targets = view.selected.size ? selectedNames() : []
      return setMenu({ x: r.left + 40, y: Math.min(rowTop, r.bottom - 8), fromKeyboard: true, items: targets.length ? itemMenu(targets) : blankMenu() })
    }
    if (k.length === 1 && !ctrl && !e.altKey && k !== ' ') {
      const now = performance.now()
      const buf = typeBuf.current
      buf.text = now - buf.at > 800 ? k : buf.text + k
      buf.at = now
      const i = typeaheadIndex(names, buf.text, cur)
      if (i >= 0) selectOnly(i)
      return handled()
    }
  }

  const header = (key: SortKey, label: string, align = '') => (
    <button
      type="button"
      onClick={() => act().setSort(key)}
      aria-sort={sort.key === key ? (sort.asc ? 'ascending' : 'descending') : undefined}
      className={`flex items-center gap-1 truncate rounded px-1 py-1 hover:bg-hover ${align}`}
    >
      {label}
      {sort.key === key && <Icon name={sort.asc ? 'up' : 'down'} size={12} className="text-muted" />}
    </button>
  )

  const activeId = focusIdx >= 0 ? `${listId}-${focusIdx}` : undefined

  return (
    <div className={`flex min-h-0 flex-1 flex-col ${readOnly ? 'opacity-60' : ''}`}>
      <div className={`${COLS} border-b border-line px-4 py-0.5 text-xs text-muted`}>
        {header('name', '名称')}
        {header('mtime', '修改时间')}
        {header('type', '类型')}
        {header('size', '大小', 'justify-end')}
      </div>
      <div
        ref={scrollRef}
        tabIndex={0}
        role="listbox"
        aria-multiselectable="true"
        aria-label="文件和文件夹"
        aria-activedescendant={activeId}
        data-file-list=""
        onKeyDown={onKeyDown}
        onContextMenu={(e) => onContextMenu(e, null)}
        onMouseDown={(e) => {
          if (e.target === e.currentTarget) act().select(device, new Set(), null, undefined, side)
        }}
        className="scroll-thin relative min-h-0 flex-1 overflow-y-auto outline-none"
      >
        {entries.length === 0 && <p className="p-8 text-center text-sm text-muted">这个文件夹是空的（可从电脑或另一栏拖文件进来）</p>}
        <div style={{ height: virt.getTotalSize(), position: 'relative' }}>
          {virt.getVirtualItems().map((row) => {
            const e = entries[row.index]
            const selected = view.selected.has(e.name)
            const focused = row.index === focusIdx
            return (
              <div
                key={e.name}
                id={`${listId}-${row.index}`}
                role="option"
                aria-selected={selected}
                aria-posinset={row.index + 1}
                aria-setsize={entries.length}
                onPointerDown={(ev) => startRowPointerTrack(row.index, ev)}
                onMouseDown={(ev) => ev.button === 0 && onRowClick(row.index, ev)}
                onDoubleClick={() => !readOnly && void act().openEntry(device, e, side)}
                onContextMenu={(ev) => onContextMenu(ev, row.index)}
                style={{ height: ROW, transform: `translateY(${row.start}px)` }}
                className={`${COLS} absolute inset-x-0 top-0 mx-1 items-center rounded px-3 text-sm ${
                  selected ? 'bg-accent-soft' : 'hover:bg-hover'
                } ${focused ? 'ring-1 ring-accent/60 ring-inset' : ''} ${clipboard?.cut && keyOf(clipboard.device, clipboard.dir) === keyOf(device, view.path) && clipboard.names.includes(e.name) ? 'opacity-50' : ''}`}
              >
                <span className="flex min-w-0 items-center gap-2">
                  <EntryIcon entry={e} />
                  {renaming === e.name ? (
                    <RenameInput
                      entry={e}
                      onDone={(name) =>
                        name == null ? act().cancelRename() : act().commitRename(device, e.name, name, side)
                      }
                    />
                  ) : (
                    <span className="truncate">{e.name}</span>
                  )}
                </span>
                <span className="truncate text-muted tabular-nums">{formatTime(e.mtime)}</span>
                <span className="truncate text-muted">{typeName(e)}</span>
                <span className="text-right text-muted tabular-nums">{e.dir ? '' : formatSize(e.size)}</span>
              </div>
            )
          })}
        </div>
      </div>

      {pointerDrag &&
        createPortal(
          <div
            style={{ left: pointerDrag.x + 12, top: pointerDrag.y + 12 }}
            className="pointer-events-none fixed z-50 flex items-center gap-2 rounded-lg border border-accent bg-panel px-3 py-1.5 text-xs shadow-lg"
          >
            <Icon name="transfer" size={14} className="text-accent" />
            <span>
              {pointerDrag.names.length === 1 ? pointerDrag.names[0] : `${pointerDrag.names.length} 项`}
              {pointerDrag.targetDevice
                ? ` → 传到“${deviceName(pointerDrag.targetDevice)}”${pointerDrag.targetSide === 'right' ? '（右栏）' : '（左栏）'}`
                : '（拖到另一栏松手即传）'}
            </span>
          </div>,
          document.body,
        )}

      <ContextMenu menu={menu} onClose={closeMenu} label="文件操作" />
    </div>
  )
}
