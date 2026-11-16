import { create } from 'zustand'
import { backend } from '../api'
import { humanError } from '../api/errors'
import { SELF_ID, type AppInfo, type DeviceId, type ProcInfo } from '../api/types'
import { deviceName } from './devices'
import { toast } from './toasts'
import { confirmDialog } from './ui'

interface Loadable<T> {
  data?: T
  loading: boolean
  error?: string
  at?: number
}

interface RemoteState {
  apps: Record<DeviceId, Loadable<AppInfo[]>>
  procs: Record<DeviceId, Loadable<ProcInfo[]>>
  favorites: Record<DeviceId, string[]>
  launching: string | null
  loadApps(device: DeviceId, force?: boolean): Promise<void>
  loadProcs(device: DeviceId): Promise<void>
  toggleFavorite(device: DeviceId, appId: string): void
  launch(device: DeviceId, app: AppInfo): Promise<void>
  kill(device: DeviceId, p: ProcInfo): Promise<void>
}

const FAV_KEY = 'pcshare.favorites'
const APPS_TTL = 5 * 60_000 // 与核心层一致：软件列表缓存 5 分钟

function loadFavorites(): Record<DeviceId, string[]> {
  try {
    return JSON.parse(localStorage.getItem(FAV_KEY) ?? '{}') as Record<DeviceId, string[]>
  } catch {
    return {}
  }
}

const where = (device: DeviceId) => (device === SELF_ID ? '本机' : `“${deviceName(device)}”`)

export const useRemote = create<RemoteState>()((set, get) => ({
  apps: {},
  procs: {},
  favorites: loadFavorites(),
  launching: null,

  async loadApps(device, force = false) {
    const cur = get().apps[device]
    if (!force && cur?.data && cur.at && Date.now() - cur.at < APPS_TTL) return
    set((s) => ({ apps: { ...s.apps, [device]: { ...cur, loading: true } } }))
    try {
      const data = await backend.appList(device)
      set((s) => ({ apps: { ...s.apps, [device]: { data, loading: false, at: Date.now() } } }))
    } catch (e) {
      set((s) => ({ apps: { ...s.apps, [device]: { ...s.apps[device], loading: false, error: humanError(e) } } }))
    }
  },

  async loadProcs(device) {
    set((s) => ({ procs: { ...s.procs, [device]: { ...s.procs[device], loading: true } } }))
    try {
      const data = await backend.procList(device)
      set((s) => ({ procs: { ...s.procs, [device]: { data, loading: false, at: Date.now() } } }))
    } catch (e) {
      set((s) => ({ procs: { ...s.procs, [device]: { ...s.procs[device], loading: false, error: humanError(e) } } }))
    }
  },

  toggleFavorite(device, appId) {
    set((s) => {
      const list = s.favorites[device] ?? []
      const next = list.includes(appId) ? list.filter((x) => x !== appId) : [...list, appId]
      const favorites = { ...s.favorites, [device]: next }
      localStorage.setItem(FAV_KEY, JSON.stringify(favorites))
      return { favorites }
    })
  },

  async launch(device, app) {
    if (get().launching) return
    set({ launching: app.id })
    try {
      const r = await backend.appLaunch(device, app.id)
      if (r.elevationPrompt)
        toast.warn(`“${app.name}”需要管理员权限，${where(device)}的屏幕上弹出了确认框，需要有人在那台电脑前点“是”。也可以在对方的设置里打开“以管理员权限随登录启动”`)
      else if (r.desktop.place === 'locked') toast.warn(`已在${where(device)}上启动“${app.name}”，但对方现在是锁屏状态，屏幕上看不到`)
      else if (r.desktop.place !== 'normal') toast.warn(`已在${where(device)}上启动“${app.name}”，但对方现在没有可见的桌面`)
      else if (r.newWindows.length === 0) toast.info(`已在${where(device)}上打开“${app.name}”，没检测到新窗口，可能本来就开着`)
      else toast.success(`已在${where(device)}上打开“${app.name}”`)
    } catch (e) {
      toast.error(`打开“${app.name}”失败：${humanError(e)}`)
    } finally {
      set({ launching: null })
    }
  },

  async kill(device, p) {
    const ok = await confirmDialog({
      title: `结束“${p.name}”？`,
      body: `${p.title ? `窗口：${p.title}。` : ''}这个程序里没保存的内容会丢失。`,
      okText: '结束进程',
      danger: true,
    })
    if (!ok) return
    try {
      await backend.procKill(device, p.pid)
      toast.success(`已结束“${p.name}”`)
    } catch (e) {
      toast.error(humanError(e))
    }
    await get().loadProcs(device)
  },
}))
