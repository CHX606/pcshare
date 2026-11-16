import { Suspense, lazy } from 'react'
import { SELF_ID, type DeviceId, type Feature } from '../api/types'
import { Icon, type IconName } from '../components/Icon'
import { Toolbar } from '../components/Toolbar'
import { useDevice } from '../store/devices'
import { useUi, type MoreFeatureTab } from '../store/ui'
import { BatchPanel } from './more/BatchPanel'
import { ClipboardPanel } from './more/ClipboardPanel'
import { PowerPanel } from './more/PowerPanel'
import { ScreenPanel } from './more/ScreenPanel'

const TerminalPanel = lazy(async () => {
  const m = await import('./more/TerminalPanel')
  return { default: m.TerminalPanel }
})

const SUB_TABS: Array<{
  id: MoreFeatureTab
  label: string
  icon: IconName
  feature?: Feature
  remoteOnly?: boolean
}> = [
  { id: 'shot', label: '看一眼与点按', icon: 'image', feature: 'shot' },
  { id: 'term', label: '命令行', icon: 'terminal', feature: 'term' },
  { id: 'clip', label: '剪贴板互通', icon: 'clipboard', feature: 'clip', remoteOnly: true },
  { id: 'power', label: '电源与唤醒', icon: 'power', feature: 'power', remoteOnly: true },
  { id: 'batch', label: '批量操作', icon: 'grid' },
]

/** “更多”页：第 3 阶段完整工作台（看一眼与截图点按、交互式伪终端、剪贴板互通、电源与网络唤醒、批量操作）。 */
export function MorePage({ device }: { device: DeviceId }) {
  const dev = useDevice(device)
  const moreTab = useUi((s) => s.moreTab)
  const setMoreTab = useUi((s) => s.setMoreTab)

  const isSupported = (f?: Feature, remoteOnly?: boolean) => {
    if (remoteOnly && device === SELF_ID) return true
    if (!f || !dev) return true
    return dev.features.includes(f)
  }

  const activeSub = isSupported(
    SUB_TABS.find((t) => t.id === moreTab)?.feature,
    SUB_TABS.find((t) => t.id === moreTab)?.remoteOnly,
  )
    ? moreTab
    : 'shot'

  return (
    <section aria-label="更多功能" className="scroll-thin flex min-h-0 flex-1 flex-col overflow-y-auto p-4">
      <Toolbar label="更多功能分类切换" className="mb-3 shrink-0 border-b border-line pb-3">
        {SUB_TABS.map((t) => {
          const ok = isSupported(t.feature, t.remoteOnly)
          return (
            <button
              key={t.id}
              type="button"
              aria-pressed={activeSub === t.id}
              disabled={!ok}
              title={ok ? t.label : '对方的程序版本暂不支持此功能，请升级对方程序'}
              onClick={() => setMoreTab(t.id)}
              className="inline-flex h-8 items-center gap-1.5 rounded-md border border-line bg-panel px-3 text-xs hover:bg-hover disabled:opacity-40"
            >
              <Icon name={t.icon} size={15} />
              <span>{t.label}</span>
            </button>
          )
        })}
      </Toolbar>

      <div className="flex min-h-0 flex-1 flex-col">
        {activeSub === 'shot' && (
          <ScreenPanel
            device={device}
            canInput={device !== SELF_ID && !!dev?.features.includes('input')}
          />
        )}
        {activeSub === 'term' && (
          <Suspense
            fallback={
              <div className="grid flex-1 place-items-center text-sm text-muted">
                正在加载终端组件…
              </div>
            }
          >
            <TerminalPanel device={device} />
          </Suspense>
        )}
        {activeSub === 'clip' && (
          <ClipboardPanel device={device} deviceName={dev?.name ?? '远端设备'} />
        )}
        {activeSub === 'power' && <PowerPanel dev={dev} />}
        {activeSub === 'batch' && <BatchPanel />}
      </div>
    </section>
  )
}
