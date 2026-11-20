import { getCurrentWebview } from '@tauri-apps/api/webview'
import { useEffect, useMemo, useRef, useState } from 'react'
import { backend } from '../../api'
import { SELF_ID, type DeviceId } from '../../api/types'
import { Icon, type IconName } from '../../components/Icon'
import { Button } from '../../components/Modal'
import { ROOT } from '../../lib/path'
import { sortEntries } from '../../lib/sort'
import { tabLabelOf } from '../../lib/stage2'
import { SELF_DEVICE, isOnline, useDevice, useDevices } from '../../store/devices'
import { keyOf, paneOf, useFiles, viewOf, type PaneSide } from '../../store/files'
import { useUi } from '../../store/ui'
import { FileList } from './FileList'
import { FileSearchPanel } from './FileSearchPanel'
import { PathBar } from './PathBar'
import { RootsView } from './RootsView'

export function FilesPage({ device }: { device: DeviceId }) {
  const dualPane = useUi((s) => s.dualPane)
  const rightDevice = useUi((s) => s.rightDevice)
  const setRightDevice = useUi((s) => s.setRightDevice)
  const devices = useDevices((s) => s.devices)
  const [dropHoverSide, setDropHoverSide] = useState<PaneSide | null>(null)

  const prevDualRef = useRef(dualPane)

  // 刚打开双栏且左右两栏刚好选了同一台设备时，右栏默认挑另一台设备；之后允许用户在右栏自由切换到同设备
  useEffect(() => {
    const justOpened = dualPane && !prevDualRef.current
    prevDualRef.current = dualPane
    if (!justOpened) return
    if (rightDevice !== device) return
    const alt = device === SELF_ID ? (devices[0]?.id ?? SELF_ID) : SELF_ID
    if (alt !== rightDevice) setRightDevice(alt)
  }, [dualPane, device, rightDevice, devices, setRightDevice])

  // 任务 2.5：监听从 Windows 资源管理器拖进窗口的文件/文件夹（tauri://drag-drop）
  useEffect(() => {
    if (backend.kind !== 'tauri') return
    let off: (() => void) | null = null
    let cancelled = false

    const resolveHoveredPane = (physX: number, physY: number): { device: DeviceId; side: PaneSide } => {
      const dpr = window.devicePixelRatio || 1
      const el = document
        .elementFromPoint(physX / dpr, physY / dpr)
        ?.closest<HTMLElement>('[data-pane-side][data-pane-device]')
      const side = (el?.dataset.paneSide as PaneSide | undefined) ?? 'left'
      const dev = el?.dataset.paneDevice ?? device
      return { device: dev, side }
    }

    void getCurrentWebview()
      .onDragDropEvent((ev) => {
        const p = ev.payload
        if (p.type === 'enter' || p.type === 'over') {
          setDropHoverSide(resolveHoveredPane(p.position.x, p.position.y).side)
        } else if (p.type === 'leave') {
          setDropHoverSide(null)
        } else if (p.type === 'drop') {
          const target = resolveHoveredPane(p.position.x, p.position.y)
          setDropHoverSide(null)
          if (p.paths.length > 0) {
            void useFiles.getState().dropLocalPaths(target.device, p.paths, target.side)
          }
        }
      })
      .then((unlisten) => {
        if (cancelled) unlisten()
        else off = unlisten
      })
      .catch(() => {})

    return () => {
      cancelled = true
      off?.()
    }
  }, [device])

  return (
    <div className="flex min-h-0 flex-1">
      <FilePane
        device={device}
        side="left"
        showDualToggle
        externalDropActive={dropHoverSide === 'left'}
      />
      {dualPane && (
        <div className="flex min-h-0 min-w-0 flex-1 border-l border-line">
          <FilePane
            device={rightDevice}
            side="right"
            showDualToggle={false}
            externalDropActive={dropHoverSide === 'right'}
          />
        </div>
      )}
    </div>
  )
}

function FilePane({
  device,
  side,
  showDualToggle,
  externalDropActive,
}: {
  device: DeviceId
  side: PaneSide
  showDualToggle: boolean
  externalDropActive: boolean
}) {
  const paneRef = useRef<HTMLElement>(null)
  const dev = useDevice(device)
  const allDevices = useDevices((s) => s.devices)
  const online = isOnline(dev)
  const pane = useFiles((s) => paneOf(s, device, side))
  const view = useFiles((s) => viewOf(s, device, side))
  const key = keyOf(device, view.path)
  const listing = useFiles((s) => s.listings[key])
  const streaming = useFiles((s) => s.streaming[key])
  const roots = useFiles((s) => s.roots[device])
  const loading = useFiles((s) => !!s.loading[key])
  const error = useFiles((s) => s.errors[key])
  const sort = useFiles((s) => s.sort)
  const hasClip = useFiles((s) => !!s.clipboard)
  const dualPane = useUi((s) => s.dualPane)
  const setDualPane = useUi((s) => s.setDualPane)
  const setRightDevice = useUi((s) => s.setRightDevice)
  const act = useFiles.getState
  const reconnect = useDevices((s) => s.reconnect)
  const [editPath, setEditPath] = useState(0)
  // 用当前目录键绑定搜索栏开关：切目录、切设备或设备离线时自动重置并关闭旧结果
  const [searchScope, setSearchScope] = useState<string | null>(null)

  const hasData = view.path === ROOT ? !!roots : !!listing

  // 初次进入或设备重连上线时刷新当前标签目录
  useEffect(() => {
    if (online) act().refresh(device, side)
  }, [device, side, online, act])

  // 任务 2.2：进入目录开启变化监听，离开目录或切换设备时自动解除监听
  useEffect(() => {
    if (!online || view.path === ROOT) return
    const watchedPath = view.path
    void backend.fsWatch(device, watchedPath).catch(() => {})
    return () => {
      void backend.fsUnwatch(device, watchedPath).catch(() => {})
    }
  }, [device, online, view.path])

  // 任务 2.3：流式首屏加载中保持原到达顺序避免跳动，全部到齐后再使排序生效
  const entries = useMemo(() => {
    if (!listing) return []
    if (streaming) return listing.entries
    return sortEntries(listing.entries, sort)
  }, [listing, streaming, sort])

  const readOnly = !online
  const atRoot = view.path === ROOT
  const canSearch = !readOnly && !atRoot && !!dev?.features.includes('search')
  const searchOpen = canSearch && searchScope === key
  const toggleSearch = () => {
    if (!canSearch) return
    setSearchScope((cur) => (cur === key ? null : key))
  }

  const onKeyDown = (e: React.KeyboardEvent) => {
    if ((e.target as HTMLElement).tagName === 'INPUT') return
    const ctrl = e.ctrlKey || e.metaKey
    if ((ctrl && e.key.toLowerCase() === 'l') || (e.altKey && e.key.toLowerCase() === 'd') || e.key === 'F4') {
      e.preventDefault()
      setEditPath((n) => n + 1)
    } else if (ctrl && e.key.toLowerCase() === 't') {
      e.preventDefault()
      act().newTab(device, undefined, side)
    } else if (ctrl && e.key.toLowerCase() === 'w') {
      e.preventDefault()
      act().closeTab(device, pane.activeId, side)
    } else if (e.key === 'F5' || (ctrl && e.key.toLowerCase() === 'r')) {
      e.preventDefault()
      act().refresh(device, side)
    } else if (e.altKey && e.key === 'ArrowLeft') {
      e.preventDefault()
      act().goBack(device, side)
    } else if (e.altKey && e.key === 'ArrowRight') {
      e.preventDefault()
      act().goForward(device, side)
    } else if (ctrl && e.key.toLowerCase() === 'f') {
      if (canSearch) {
        e.preventDefault()
        toggleSearch()
      }
    } else if ((e.altKey && e.key === 'ArrowUp') || e.key === 'Backspace') {
      e.preventDefault()
      act().goUp(device, side)
    }
  }

  const tool = (
    icon: IconName,
    label: string,
    onClick: () => void,
    disabled = false,
    shortcut?: string,
    toggleState?: boolean,
  ) => (
    <button
      type="button"
      aria-label={label}
      aria-pressed={toggleState}
      title={shortcut ? `${label}（${shortcut}）` : label}
      disabled={disabled}
      onClick={onClick}
      className={`grid size-8 place-items-center rounded-md disabled:opacity-35 disabled:hover:bg-transparent ${
        toggleState ? 'bg-accent-soft text-accent' : 'text-muted hover:bg-hover hover:text-fg'
      }`}
    >
      <Icon name={icon} size={17} />
    </button>
  )

  const deviceOptions = [SELF_DEVICE, ...allDevices]

  return (
    <section
      ref={paneRef}
      aria-label={side === 'left' ? '主文件栏' : '右侧对比文件栏'}
      data-pane-device={device}
      data-pane-side={side}
      className={`file-pane flex min-h-0 min-w-0 flex-1 flex-col ${externalDropActive ? 'ring-2 ring-accent ring-inset' : ''}`}
      onKeyDown={onKeyDown}
    >
      {/* 任务 2.6：单行水平滚动文件夹多标签栏 + 右栏设备切换 */}
      <div className="flex h-9 shrink-0 items-center gap-1 border-b border-line bg-sidebar/60 px-2 text-xs">
        {side === 'right' && (
          <select
            aria-label="右侧栏设备"
            value={device}
            onChange={(e) => setRightDevice(e.target.value)}
            className="mr-1 h-6 max-w-36 rounded border border-line bg-panel px-1.5 text-xs font-medium text-fg outline-none focus:border-accent"
          >
            {deviceOptions.map((d) => (
              <option key={d.id} value={d.id}>
                {d.name}
              </option>
            ))}
          </select>
        )}
        <div role="tablist" aria-label="文件夹标签" className="folder-tablist flex-1 items-stretch gap-0.5">
          {pane.tabs.map((t) => {
            const selected = t.id === pane.activeId
            return (
              <div key={t.id} className="flex shrink-0 items-center">
                <button
                  type="button"
                  role="tab"
                  aria-selected={selected}
                  title={t.path || '此电脑'}
                  onClick={() => act().activateTab(device, t.id, side)}
                  className="folder-tab flex h-8 max-w-44 items-center gap-1.5 rounded-t-md px-2.5 text-muted hover:text-fg"
                >
                  <Icon name={t.path === ROOT ? 'drive' : 'folder'} size={13} className="shrink-0" />
                  <span className="truncate">{tabLabelOf(t.path)}</span>
                </button>
                {pane.tabs.length > 1 && (
                  <button
                    type="button"
                    aria-label={`关闭标签 ${tabLabelOf(t.path)}`}
                    onClick={() => act().closeTab(device, t.id, side)}
                    className="-ml-1 mr-0.5 rounded p-0.5 text-faint hover:bg-hover hover:text-fg"
                  >
                    <Icon name="x" size={12} />
                  </button>
                )}
              </div>
            )
          })}
        </div>
        <button
          type="button"
          aria-label="新建文件夹标签页"
          title="新建标签页（Ctrl+T）"
          onClick={() => act().newTab(device, undefined, side)}
          className="grid size-6 shrink-0 place-items-center rounded text-muted hover:bg-hover hover:text-fg"
        >
          <Icon name="plus" size={14} />
        </button>
      </div>

      {/* 工具栏 */}
      <div className="flex items-center gap-1 border-b border-line px-2.5 py-1.5">
        {tool('back', '后退', () => act().goBack(device, side), !view.back.length, 'Alt+←')}
        {tool('forward', '前进', () => act().goForward(device, side), !view.fwd.length, 'Alt+→')}
        {tool('up', '上一级', () => act().goUp(device, side), atRoot, 'Backspace')}
        {tool('refresh', '刷新', () => act().refresh(device, side), !online, 'F5')}
        <div className="mx-1 flex min-w-0 flex-1 items-center gap-2">
          <PathBar
            path={view.path}
            editRequest={editPath}
            paneRef={paneRef}
            onNavigate={(p) => act().open(device, p, 'push', side)}
          />
          {loading && hasData && <Icon name="loader" size={16} className="spin shrink-0 text-accent" aria-label="正在刷新" />}
        </div>
        {tool('folderPlus', '新建文件夹', () => void act().newFolder(device, side), readOnly || atRoot, 'Ctrl+Shift+N')}
        {tool('upload', '上传文件', () => void act().upload(device, side), readOnly || atRoot || device === SELF_ID)}
        {tool('folder', '上传整个文件夹', () => void act().uploadFolder(device, side), readOnly || atRoot || device === SELF_ID)}
        {tool('paste', '粘贴', () => void act().paste(device, side), readOnly || atRoot || !hasClip, 'Ctrl+V')}
        {tool(
          'search',
          '在此目录中搜索文件',
          toggleSearch,
          !canSearch,
          'Ctrl+F',
          searchOpen,
        )}
        {showDualToggle &&
          tool('columns', '双栏对拷视图', () => setDualPane(!dualPane), false, undefined, dualPane)}
      </div>

      {searchOpen && (
        <FileSearchPanel
          key={`${side}|${key}`}
          device={device}
          side={side}
          rootPath={view.path}
          onClose={() => setSearchScope(null)}
        />
      )}

      {streaming && (
        <div role="status" className="flex items-center gap-2 border-b border-line bg-accent-soft/50 px-3 py-1 text-xs text-fg">
          <Icon name="loader" size={13} className="spin text-accent" />
          <span>
            大目录分批读取中：已先显示 {streaming.loaded}
            {streaming.totalHint ? ` / ${streaming.totalHint}` : ''} 项，全部到齐后自动应用排序…
          </span>
        </div>
      )}

      {!online && dev && (
        <div role="status" className="flex items-center gap-3 border-b border-line bg-warn/10 px-4 py-2 text-sm">
          <Icon name="alert" className="shrink-0 text-warn" />
          <span className="flex-1">
            “{dev.name}”{dev.status.state === 'connecting' ? '正在重连…' : '已离线'}
            {hasData ? '，下面显示的是本地缓存的目录内容，暂时不能修改。' : '，等它上线后才能浏览文件。'}
          </span>
          <Button onClick={() => void reconnect(device)} disabled={dev.status.state === 'connecting'}>
            重试
          </Button>
        </div>
      )}

      {error && online && !(hasData && error.code === 'offline') ? (
        <ErrorView
          msg={error.msg}
          onRetry={() => act().refresh(device, side)}
          onBack={view.back.length ? () => act().goBack(device, side) : undefined}
        />
      ) : !hasData ? (
        <div className="grid flex-1 place-items-center text-sm text-muted">
          {online || loading ? (
            <span className="flex items-center gap-2">
              <Icon name="loader" className="spin text-accent" /> 正在读取…
            </span>
          ) : (
            '没有可显示的内容'
          )}
        </div>
      ) : atRoot ? (
        <RootsView roots={roots!} readOnly={readOnly} onOpen={(p) => act().open(device, p, 'push', side)} />
      ) : (
        <FileList device={device} side={side} entries={entries} readOnly={readOnly} />
      )}
    </section>
  )
}

function ErrorView({ msg, onRetry, onBack }: { msg: string; onRetry: () => void; onBack?: () => void }) {
  return (
    <div className="grid flex-1 place-items-center p-8">
      <div className="max-w-sm text-center">
        <Icon name="alert" size={28} className="mx-auto mb-3 text-warn" />
        <p className="mb-4 text-sm leading-relaxed">{msg}</p>
        <div className="flex justify-center gap-2">
          {onBack && <Button onClick={onBack}>返回</Button>}
          <Button variant="primary" onClick={onRetry}>
            重试
          </Button>
        </div>
      </div>
    </div>
  )
}
