import { create } from 'zustand'
import { backend } from '../api'
import { humanError } from '../api/errors'
import { SELF_ID, type Device, type DeviceId, type LanPeer, type PairRequest, type SelfInfo } from '../api/types'
import { toast } from './toasts'

export const SELF_DEVICE: Device = {
  id: SELF_ID,
  name: '本机',
  os: 'windows',
  status: { state: 'online', link: 'local', rttMs: 0 },
  features: ['fs', 'transfer', 'apps', 'procs', 'shot', 'term', 'search'],
}

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
  shiftPairRequest(): void
  reconnect(id: DeviceId): Promise<void>
  setHostPaused(paused: boolean): Promise<void>
  refreshLan(): Promise<void>
}

export const useDevices = create<DevicesState>()((set) => ({
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
      const [self, devices, hostSt, lanPeers] = await Promise.all([
        backend.selfInfo(),
        backend.listDevices(),
        backend.hostStatus().catch(() => ({ paused: false, workingSetBytes: 0 })),
        backend.lanList().catch(() => []),
      ])
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
    set((s) => {
      const i = s.devices.findIndex((x) => x.id === d.id)
      if (i < 0) return { devices: [...s.devices, d] }
      const prev = s.devices[i]
      if (prev.status.state !== d.status.state) notifyStatus(prev, d)
      const devices = s.devices.slice()
      devices[i] = d
      return { devices }
    })
  },
  remove(id) {
    set((s) => ({
      devices: s.devices.filter((d) => d.id !== id),
      selected: s.selected === id ? SELF_ID : s.selected,
    }))
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

export function deviceName(id: DeviceId): string {
  if (id === SELF_ID) return '本机'
  return useDevices.getState().devices.find((d) => d.id === id)?.name ?? '未知设备'
}

export function isOnline(d: Device | undefined): boolean {
  return d?.status.state === 'online'
}

/** 订阅核心层推送的设备、局域网与被控状态事件；返回取消订阅函数。 */
export function wireDeviceEvents(): () => void {
  const offs = [
    backend.on('device-status', (d) => useDevices.getState().upsert(d)),
    backend.on('device-removed', ({ id }) => useDevices.getState().remove(id)),
    backend.on('pair-request', (r) => useDevices.setState((s) => ({ pairRequests: [...s.pairRequests, r] }))),
    backend.on('lan-changed', (lanPeers) => useDevices.setState({ lanPeers })),
    backend.on('host-paused', ({ paused }) => useDevices.setState({ hostPaused: paused })),
    backend.on('app-notice', (n) => {
      if (n.kind === 'warn') toast.warn(`${n.title}：${n.body}`)
      else if (n.kind === 'error') toast.error(`${n.title}：${n.body}`)
      else if (n.kind === 'success') toast.success(`${n.title}：${n.body}`)
      else toast.info(`${n.title}：${n.body}`)
    }),
  ]
  return () => offs.forEach((f) => f())
}
