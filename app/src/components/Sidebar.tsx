import { useCallback, useState } from 'react'
import { backend } from '../api'
import { humanError } from '../api/errors'
import { SELF_ID, type Device } from '../api/types'
import { statusText } from '../lib/status'
import { SELF_DEVICE, useDevices } from '../store/devices'
import { toast } from '../store/toasts'
import { confirmDialog, useUi } from '../store/ui'
import { ContextMenu, type MenuState } from './ContextMenu'
import { StatusDot } from './DeviceStatus'
import { Icon } from './Icon'

export function Sidebar() {
  const devices = useDevices((s) => s.devices)
  const selected = useDevices((s) => s.selected)
  const hostPaused = useDevices((s) => s.hostPaused)
  const lanCount = useDevices((s) => s.lanPeers.length)
  const select = useDevices((s) => s.select)
  const reconnect = useDevices((s) => s.reconnect)
  const setHostPaused = useDevices((s) => s.setHostPaused)
  const openAdd = useUi((s) => s.openAddDevice)
  const openSettings = useUi((s) => s.openSettings)
  const openEdit = useUi((s) => s.openEditDevice)
  const [menu, setMenu] = useState<MenuState | null>(null)
  const closeMenu = useCallback(() => setMenu(null), [])

  const showMenu = (d: Device, x: number, y: number, fromKeyboard = false) => {
    if (d.id === SELF_ID) return
    setMenu({
      x,
      y,
      fromKeyboard,
      items: [
        { label: '设备设置…', icon: 'gear', onSelect: () => openEdit(d.id) },
        { label: '重新连接', icon: 'refresh', disabled: d.status.state === 'connecting', onSelect: () => void reconnect(d.id) },
        'sep',
        {
          label: '移除这台设备…',
          icon: 'trash',
          danger: true,
          onSelect: async () => {
            const ok = await confirmDialog({
              title: `移除“${d.name}”？`,
              body: '移除后它不能再操作这台电脑，这台电脑也不能再操作它。以后要用需要重新配对。',
              okText: '移除',
              danger: true,
            })
            if (!ok) return
            try {
              await backend.removeDevice(d.id)
              toast.info(`已移除“${d.name}”`)
            } catch (e) {
              toast.error(humanError(e))
            }
          },
        },
      ],
    })
  }

  const all = [SELF_DEVICE, ...devices]

  return (
    <aside className="flex w-60 shrink-0 flex-col border-r border-line bg-sidebar">
      <div className="flex h-12 items-center gap-2 px-4 font-semibold">
        <span className="grid size-6 place-items-center rounded-md bg-accent text-accent-fg">
          <Icon name="link" size={14} />
        </span>
        电脑共享
      </div>
      {hostPaused && (
        <div className="mx-2 mb-1 flex items-center justify-between gap-2 rounded-md bg-warn/15 px-2.5 py-1.5 text-xs text-warn">
          <span>已暂停被控</span>
          <button
            type="button"
            onClick={() => void setHostPaused(false)}
            className="font-medium underline hover:opacity-80"
          >
            恢复
          </button>
        </div>
      )}
      <nav aria-label="设备" className="scroll-thin flex-1 overflow-y-auto px-2 pb-2">
        <h2 className="px-2 pt-2 pb-1 text-xs text-muted">设备</h2>
        <ul className="flex flex-col gap-0.5">
          {all.map((d) => {
            const active = d.id === selected
            const offline = d.status.state !== 'online'
            return (
              <li key={d.id}>
                <button
                  type="button"
                  aria-current={active ? 'page' : undefined}
                  onClick={() => select(d.id)}
                  onContextMenu={(e) => {
                    e.preventDefault()
                    showMenu(d, e.clientX, e.clientY)
                  }}
                  onKeyDown={(e) => {
                    if (e.key === 'ContextMenu' || (e.shiftKey && e.key === 'F10')) {
                      e.preventDefault()
                      const r = e.currentTarget.getBoundingClientRect()
                      showMenu(d, r.left + 24, r.bottom, true)
                    }
                  }}
                  className={`group flex w-full items-center gap-2.5 rounded-md px-2 py-2 text-left ${
                    active ? 'bg-accent-soft' : 'hover:bg-hover'
                  }`}
                >
                  <Icon name="monitor" size={18} className={offline ? 'text-faint' : active ? 'text-accent' : 'text-muted'} />
                  <span className="min-w-0 flex-1">
                    <span className={`block truncate ${offline ? 'text-muted' : ''}`}>{d.name}</span>
                    <span className="flex items-center gap-1.5 text-xs text-muted">
                      <StatusDot device={d} />
                      <span className="truncate">{statusText(d)}</span>
                    </span>
                  </span>
                </button>
              </li>
            )
          })}
        </ul>
        {devices.length === 0 && (
          <p className="px-2 pt-3 text-xs leading-relaxed text-muted">还没有配对其他电脑。点下面的“添加设备”开始。</p>
        )}
      </nav>
      <div className="flex flex-col gap-0.5 border-t border-line p-2">
        <button type="button" onClick={() => openAdd(true)} className="flex items-center gap-2 rounded-md px-2 py-2 hover:bg-hover">
          <Icon name="plus" />
          <span>添加设备</span>
          {lanCount > 0 && (
            <span className="ml-auto rounded-full bg-accent-soft px-1.5 py-0.5 text-[11px] font-medium text-accent">
              附近 {lanCount}
            </span>
          )}
        </button>
        <button type="button" onClick={() => openSettings(true)} className="flex items-center gap-2 rounded-md px-2 py-2 hover:bg-hover">
          <Icon name="gear" /> 设置
        </button>
      </div>
      <ContextMenu menu={menu} onClose={closeMenu} label="设备操作" />
    </aside>
  )
}
