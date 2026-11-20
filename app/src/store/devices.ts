import { create } from 'zustand'
import { backend } from '../api'
import { humanError } from '../api/errors'
import { SELF_ID, type Device, type DeviceId, type LanPeer, type PairRequest, type SelfInfo } from '../api/types'
import { toast } from './toasts'
import { useUi } from './ui'

export const SELF_DEVICE: Device = {
  id: SELF_ID,
  name: '本机',
  os: 'windows',
  status: { state: 'online', link: 'local', rttMs: 0 },
  features: ['fs', 'transfer', 'apps', 'procs', 'shot', 'term', 'search'],
}

/** 已有前端专属状态 toast 的系统通知标题，收到 app-notice 时去重不二次弹窗。 */
const DUPLICATE_NOTICE_TITLES = new Set(['设备已上线', '设备已离线', '传输已完成'])

interface DevicesState {
  ready: boolean
  loadError: string | null
  self: SelfInfo | null
  hostPaused: boolean
  /** 已配对的其他设备（不含本机） */
  devices: Device[]
  /** 同一局域网自动发现的未配对设备（任务 2.9） */
  lanPeers: LanPeer[]
  selected: DeviceId
  pairRequests: PairRequest[]
  init(): Promise<void>
  select(id: DeviceId): void
  upsert(d: Device): void
  remove(id: DeviceId): void
  mergePairRequests(list: PairRequest[]): void
  removePairRequest(requestId: string): void
  shiftPairRequest(): void
  reconnect(id: DeviceId): Promise<void>
  setHostPaused(paused: boolean): Promise<void>
  refreshLan(): Promise<void>
}

export const useDevices = create<DevicesState>()((set, get) => ({
  ready: false,
  loadError: null,
  self: null,
  hostPaused: false,
  devices: [],
  lanPeers: [],
  selected: SELF_ID,
  pairRequests: [],

  async init() {
    try {
      const [self, devices, hostSt, lanPeers, settings] = await Promise.all([
        backend.selfInfo(),
        backend.listDevices(),
        backend.hostStatus().catch(() => ({ paused: false, workingSetBytes: 0 })),
        backend.lanList().catch(() => []),
        backend.getSettings().catch(() => null),
      ])
      if (settings) {
        const hk = settings.quickHotkey === undefined ? 'Alt+Space' : settings.quickHotkey.trim()
        useUi.getState().setQuickHotkey(hk)
      }
      const firstOnline = devices.find((d) => d.status.state === 'online')
      set({
        self,
        devices,
        hostPaused: hostSt.paused,
        lanPeers,
        ready: true,
        loadError: null,
        selected: firstOnline?.id ?? SELF_ID,
      })
    } catch (e) {
      set({ loadError: humanError(e), ready: true })
    }
  },
  select(id) {
    set({ selected: id })
  },
  upsert(d) {
    const prev = get().devices.find((x) => x.id === d.id)
    set((s) => {
      const i = s.devices.findIndex((x) => x.id === d.id)
      if (i < 0) return { devices: [...s.devices, d] }
      const devices = s.devices.slice()
      devices[i] = d
      return { devices }
    })
    if (prev && prev.status.state !== d.status.state) notifyStatus(prev, d)
  },
  remove(id) {
    const nextDevices = get().devices.filter((d) => d.id !== id)
    const nextSelected = get().selected === id ? SELF_ID : get().selected
    set({
      devices: nextDevices,
      selected: nextSelected,
    })
    const ui = useUi.getState()
    if (ui.rightDevice === id) {
      const fallback = nextSelected === SELF_ID ? (nextDevices[0]?.id ?? SELF_ID) : SELF_ID
      ui.setRightDevice(fallback)
    }
    if (ui.editDevice === id) {
      ui.openEditDevice(null)
    }
  },
  mergePairRequests(list) {
    if (!list.length) return
    set((s) => {
      const seen = new Set(s.pairRequests.map((r) => r.requestId))
      const added = list.filter((r) => {
        if (seen.has(r.requestId)) return false
        seen.add(r.requestId)
        return true
      })
      return added.length ? { pairRequests: [...s.pairRequests, ...added] } : s
    })
  },
  removePairRequest(requestId) {
    set((s) => ({ pairRequests: s.pairRequests.filter((r) => r.requestId !== requestId) }))
  },
  shiftPairRequest() {
    set((s) => ({ pairRequests: s.pairRequests.slice(1) }))
  },
  async reconnect(id) {
    try {
      await backend.reconnect(id)
    } catch (e) {
      toast.error(humanError(e))
    }
  },
  async setHostPaused(paused) {
    try {
      await backend.hostSetPaused(paused)
      set({ hostPaused: paused })
      toast.info(paused ? '已暂停被控，别的电脑暂时不能操作这台' : '已恢复被控')
    } catch (e) {
      toast.error(humanError(e))
    }
  },
  async refreshLan() {
    try {
      const lanPeers = await backend.lanList()
      set({ lanPeers })
    } catch {
      // 忽略局域网刷新瞬态失败
    }
  },
}))

function notifyStatus(prev: Device, next: Device) {
  if (next.status.state === 'online' && prev.status.state !== 'online') toast.success(`“${next.name}”已上线`)
  if (next.status.state === 'offline' && prev.status.state === 'online') toast.warn(`“${next.name}”已离线`)
}

export function useDevice(id: DeviceId): Device | undefined {
  return useDevices((s) => (id === SELF_ID ? SELF_DEVICE : s.devices.find((d) => d.id === id)))
}

export function useDeviceName(id: DeviceId): string {
  return useDevices((s) => {
    if (id === SELF_ID) return '本机'
    return s.devices.find((d) => d.id === id)?.name ?? '未知设备'
  })
}

export function deviceName(id: DeviceId): string {
  if (id === SELF_ID) return '本机'
  return useDevices.getState().devices.find((d) => d.id === id)?.name ?? '未知设备'
}

export function isOnline(d: Device | undefined): boolean {
  return d?.status.state === 'online'
}

/** 订阅核心层推送的设备、局域网与被控状态事件，并补拉冷启前挂起的配对请求；返回取消订阅函数。 */
export function wireDeviceEvents(): () => void {
  let active = true
  const offs = [
    backend.on('device-status', (d) => useDevices.getState().upsert(d)),
    backend.on('device-removed', ({ id }) => useDevices.getState().remove(id)),
    backend.on('pair-request', (r) => useDevices.getState().mergePairRequests([r])),
    backend.on('lan-changed', (lanPeers) => useDevices.setState({ lanPeers })),
    backend.on('host-paused', ({ paused }) => useDevices.setState({ hostPaused: paused })),
    backend.on('app-notice', (n) => {
      if (DUPLICATE_NOTICE_TITLES.has(n.title)) return
      if (n.kind === 'warn') toast.warn(`${n.title}：${n.body}`)
      else if (n.kind === 'error') toast.error(`${n.title}：${n.body}`)
      else if (n.kind === 'success') toast.success(`${n.title}：${n.body}`)
      else toast.info(`${n.title}：${n.body}`)
    }),
  ]
  void backend
    .pairPendingList()
    .then((pending) => {
      if (active && pending.length > 0) {
        useDevices.getState().mergePairRequests(pending)
      }
    })
    .catch(() => {})
  return () => {
    active = false
    offs.forEach((f) => f())
  }
}
