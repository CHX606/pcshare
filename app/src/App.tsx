import { useEffect } from 'react'
import { SELF_ID, type Feature } from './api/types'
import { ConfirmHost } from './components/ConfirmHost'
import { Icon, type IconName } from './components/Icon'
import { Sidebar } from './components/Sidebar'
import { StatusBar } from './components/StatusBar'
import { Toasts } from './components/Toasts'
import { TransferPanel } from './components/TransferPanel'
import { AddDeviceDialog } from './dialogs/AddDeviceDialog'
import { AutostartPromptDialog } from './dialogs/AutostartPromptDialog'
import { ConflictDialog } from './dialogs/ConflictDialog'
import { EditDeviceDialog, SettingsDialog } from './dialogs/DeviceDialogs'
import { EditSyncDialog } from './dialogs/EditSyncDialog'
import { PairRequestDialog } from './dialogs/PairRequestDialog'
import { QuickLauncherDialog } from './dialogs/QuickLauncherDialog'
import { AppsPage } from './pages/AppsPage'
import { DiagnosticsPage } from './pages/DiagnosticsPage'
import { FilesPage } from './pages/files/FilesPage'
import { MorePage } from './pages/MorePage'
import { ProcsPage } from './pages/ProcsPage'
import { useDevice, useDevices, wireDeviceEvents } from './store/devices'
import { wireFileEvents } from './store/files'
import { useTasks, wireTaskEvents } from './store/tasks'
import { useUi, type Tab } from './store/ui'

const TABS: Array<{ id: Tab; label: string; icon: IconName; feature?: Feature }> = [
  { id: 'files', label: '文件', icon: 'folder', feature: 'fs' },
  { id: 'apps', label: '软件', icon: 'grid', feature: 'apps' },
  { id: 'procs', label: '进程', icon: 'cpu', feature: 'procs' },
  { id: 'diag', label: '诊断', icon: 'activity' },
  { id: 'more', label: '更多', icon: 'dots' },
]

export default function App() {
  const ready = useDevices((s) => s.ready)
  const loadError = useDevices((s) => s.loadError)
  const selected = useDevices((s) => s.selected)
  const dev = useDevice(selected)
  const tab = useUi((s) => s.tab)
  const setTab = useUi((s) => s.setTab)
  const openLauncher = useUi((s) => s.openLauncher)
  const panelOpen = useTasks((s) => s.panelOpen)

  useEffect(() => {
    const offs = [wireDeviceEvents(), wireTaskEvents(), wireFileEvents()]
    void useDevices.getState().init()
    return () => offs.forEach((f) => f())
  }, [])

  // Ctrl+1…5 切换页签
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!e.ctrlKey || e.altKey || e.shiftKey) return
      const i = Number(e.key) - 1
      if (i >= 0 && i < TABS.length) {
        e.preventDefault()
        setTab(TABS[i].id)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [setTab])

  if (!ready) return <div className="grid h-full place-items-center text-muted">正在启动…</div>
  if (loadError)
    return (
      <div className="grid h-full place-items-center p-8 text-center">
        <div>
          <p className="mb-2 font-medium">程序核心没有正常启动</p>
          <p className="text-sm text-muted">{loadError}</p>
        </div>
      </div>
    )

  const supported = (f?: Feature) => !f || !dev || dev.features.includes(f)
  const activeTab = supported(TABS.find((t) => t.id === tab)?.feature) ? tab : 'files'

  return (
    <div className="flex h-full">
      <Sidebar />
      <main className="flex min-w-0 flex-1 flex-col bg-panel">
        <header className="flex h-12 shrink-0 items-center gap-6 border-b border-line px-4">
          <h1 className="max-w-60 truncate font-semibold" title={dev?.name}>
            {dev?.name ?? '未选择设备'}
            {selected === SELF_ID && <span className="ml-2 text-xs font-normal text-muted">就是你面前这台</span>}
          </h1>
          <nav role="tablist" aria-label="功能" className="flex h-full items-stretch gap-1">
            {TABS.map((t, i) => {
              const ok = supported(t.feature)
              return (
                <button
                  key={t.id}
                  type="button"
                  role="tab"
                  aria-selected={activeTab === t.id}
                  disabled={!ok}
                  title={ok ? `${t.label}（Ctrl+${i + 1}）` : '对方的程序版本不支持，请先升级对方'}
                  onClick={() => setTab(t.id)}
                  className={`flex items-center gap-1.5 border-b-2 px-3 text-sm disabled:opacity-40 ${
                    activeTab === t.id ? 'border-accent text-fg' : 'border-transparent text-muted hover:text-fg'
                  }`}
                >
                  <Icon name={t.icon} size={15} />
                  {t.label}
                </button>
              )
            })}
          </nav>
          <button
            type="button"
            onClick={() => openLauncher(true)}
            title="跨设备快捷搜索并启动软件或文件夹（Alt+Space 或 Ctrl+K）"
            className="ml-auto flex h-7 items-center gap-2 rounded-md border border-line bg-bg px-2.5 text-xs text-muted hover:border-accent hover:text-fg"
          >
            <Icon name="search" size={13} />
            <span>快捷启动…</span>
            <kbd className="rounded bg-hover px-1.5 py-0.5 font-mono text-[11px] text-faint">Alt+Space</kbd>
          </button>
        </header>
        <div className="flex min-h-0 flex-1 flex-col" key={`${selected}-${activeTab}`}>
          {activeTab === 'files' && <FilesPage device={selected} />}
          {activeTab === 'apps' && <AppsPage device={selected} />}
          {activeTab === 'procs' && <ProcsPage device={selected} />}
          {activeTab === 'diag' && <DiagnosticsPage />}
          {activeTab === 'more' && <MorePage device={selected} />}
        </div>
        {panelOpen && <TransferPanel />}
        <StatusBar device={selected} />
      </main>
      <Toasts />
      <ConfirmHost />
      <ConflictDialog />
      <AutostartPromptDialog />
      <AddDeviceDialog />
      <PairRequestDialog />
      <EditDeviceDialog />
      <SettingsDialog />
      <QuickLauncherDialog />
      <EditSyncDialog />
    </div>
  )
}
