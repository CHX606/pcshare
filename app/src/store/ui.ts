import { create } from 'zustand'
import { SELF_ID, type ConflictPolicy, type DeviceId } from '../api/types'

export type Tab = 'files' | 'apps' | 'procs' | 'diag' | 'more'

export interface ConfirmOptions {
  title: string
  body?: string
  okText?: string
  danger?: boolean
}

export interface ConflictChoice {
  policy: ConflictPolicy
  applyToAll: boolean
}

export interface ConflictPrompt {
  fileName: string
  destDeviceName: string
  remainingCount: number
  resolve: (choice: ConflictChoice | null) => void
}

export type MoreFeatureTab = 'shot' | 'term' | 'clip' | 'power' | 'batch'

interface UiState {
  tab: Tab
  moreTab: MoreFeatureTab
  dualPane: boolean
  rightDevice: DeviceId
  addDeviceOpen: boolean
  settingsOpen: boolean
  launcherOpen: boolean
  editDevice: DeviceId | null
  confirm: (ConfirmOptions & { resolve: (ok: boolean) => void }) | null
  conflictPrompt: ConflictPrompt | null
  setTab(tab: Tab): void
  setMoreTab(moreTab: MoreFeatureTab): void
  setDualPane(dualPane: boolean): void
  setRightDevice(rightDevice: DeviceId): void
  openAddDevice(open: boolean): void
  openSettings(open: boolean): void
  openLauncher(open: boolean): void
  toggleLauncher(): void
  openEditDevice(id: DeviceId | null): void
}

export const useUi = create<UiState>()((set) => ({
  tab: 'files',
  moreTab: 'shot',
  dualPane: false,
  rightDevice: SELF_ID,
  addDeviceOpen: false,
  settingsOpen: false,
  launcherOpen: false,
  editDevice: null,
  confirm: null,
  conflictPrompt: null,
  setTab: (tab) => set({ tab }),
  setMoreTab: (moreTab) => set({ moreTab }),
  setDualPane: (dualPane) => set({ dualPane }),
  setRightDevice: (rightDevice) => set({ rightDevice }),
  openAddDevice: (addDeviceOpen) => set({ addDeviceOpen }),
  openSettings: (settingsOpen) => set({ settingsOpen }),
  openLauncher: (launcherOpen) => set({ launcherOpen }),
  toggleLauncher: () => set((s) => ({ launcherOpen: !s.launcherOpen })),
  openEditDevice: (editDevice) => set({ editDevice }),
}))

/** 弹出确认框，用户点确定时返回 true。 */
export function confirmDialog(opts: ConfirmOptions): Promise<boolean> {
  return new Promise((resolve) => {
    useUi.getState().confirm?.resolve(false)
    useUi.setState({
      confirm: {
        ...opts,
        resolve: (ok) => {
          useUi.setState({ confirm: null })
          resolve(ok)
        },
      },
    })
  })
}

/** 弹出同名冲突选择框（覆盖 / 跳过 / 都保留，可选“应用到全部”）；取消返回 null。 */
export function askConflictChoice(opts: {
  fileName: string
  destDeviceName: string
  remainingCount: number
}): Promise<ConflictChoice | null> {
  return new Promise((resolve) => {
    useUi.getState().conflictPrompt?.resolve(null)
    useUi.setState({
      conflictPrompt: {
        ...opts,
        resolve: (choice) => {
          useUi.setState({ conflictPrompt: null })
          resolve(choice)
        },
      },
    })
  })
}
