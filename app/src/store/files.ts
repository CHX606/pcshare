import { create } from 'zustand'
import { backend } from '../api'
import { errorCode, humanError } from '../api/errors'
import {
  SELF_ID,
  type ConflictPolicy,
  type DeviceId,
  type ErrorCode,
  type FsEntry,
  type FsRoot,
} from '../api/types'
import { askConflictChoice, confirmDialog } from './ui'
import { ROOT, baseName, joinPath, parentPath } from '../lib/path'
import type { SortKey, SortSpec } from '../lib/sort'
import {
  activateTabInPane,
  applyListingDiff,
  closeTabInPane,
  createPaneTabsState,
  defaultTab,
  openTabInPane,
  paneKeyOf,
  resolvePaneTabsState,
  type FolderTab,
  type PaneSide,
  type PaneTabsState,
} from '../lib/stage2'
import { deviceName } from './devices'
import { registerTask, setTaskDoneHandler } from './tasks'
import { toast } from './toasts'

export type { PaneSide }
export type View = FolderTab

export interface Listing {
  entries: FsEntry[]
  at: number
}

export interface StreamingProgress {
  loaded: number
  totalHint?: number
}

export interface LoadError {
  code: ErrorCode
  msg: string
}

export interface FsClipboard {
  device: DeviceId
  dir: string
  names: string[]
  cut: boolean
}

interface FilesState {
  panes: Record<string, PaneTabsState>
  listings: Record<string, Listing>
  streaming: Record<string, StreamingProgress | undefined>
  loading: Record<string, boolean>
  errors: Record<string, LoadError | undefined>
  roots: Record<DeviceId, FsRoot[] | undefined>
  sort: SortSpec
  clipboard: FsClipboard | null
  renaming: { side: PaneSide; device: DeviceId; name: string } | null

  open(device: DeviceId, path: string, mode?: 'push' | 'back' | 'fwd' | 'replace', side?: PaneSide): void
  newTab(device: DeviceId, path?: string, side?: PaneSide): void
  activateTab(device: DeviceId, tabId: string, side?: PaneSide): void
  closeTab(device: DeviceId, tabId: string, side?: PaneSide): void
  refresh(device: DeviceId, side?: PaneSide): void
  refreshPath(device: DeviceId, path: string): void
  goBack(device: DeviceId, side?: PaneSide): void
  goForward(device: DeviceId, side?: PaneSide): void
  goUp(device: DeviceId, side?: PaneSide): void
  select(
    device: DeviceId,
    selected: ReadonlySet<string>,
    focus: string | null,
    anchor?: string | null,
    side?: PaneSide,
  ): void
  setSort(key: SortKey): void
  copy(device: DeviceId, names: string[], cut: boolean, side?: PaneSide): void
  paste(device: DeviceId, side?: PaneSide): Promise<void>
  remove(device: DeviceId, names: string[], permanent: boolean, side?: PaneSide): Promise<void>
  startRename(device: DeviceId, name: string, side?: PaneSide): void
  cancelRename(): void
  commitRename(device: DeviceId, oldName: string, newName: string, side?: PaneSide): Promise<boolean>
  newFolder(device: DeviceId, side?: PaneSide): Promise<void>
  upload(device: DeviceId, side?: PaneSide): Promise<void>
  uploadFolder(device: DeviceId, side?: PaneSide): Promise<void>
  download(device: DeviceId, names: string[], side?: PaneSide): Promise<void>
  dropLocalPaths(device: DeviceId, localPaths: string[], side?: PaneSide): Promise<void>
  dropBetweenPanes(
    srcDevice: DeviceId,
    srcDir: string,
    names: string[],
    dstDevice: DeviceId,
    dstDir: string,
  ): Promise<void>
  openEntry(device: DeviceId, entry: FsEntry, side?: PaneSide): Promise<void>
}

export const keyOf = (device: DeviceId, path: string) =>
  `${device}|${path.replace(/\\+$/, path.length <= 3 ? '\\' : '').toLowerCase()}`

const requestSeq: Record<string, number> = {}

interface StreamLoadState {
  token: number
  receivedFirst: boolean
  entries: FsEntry[]
  seen: Set<string>
}

const streamStates: Record<string, StreamLoadState | undefined> = {}

export function paneOf(s: Pick<FilesState, 'panes'>, device: DeviceId, side: PaneSide = 'left'): PaneTabsState {
  return resolvePaneTabsState(s.panes, paneKeyOf(side, device), device)
}

export function viewOf(s: Pick<FilesState, 'panes'>, device: DeviceId, side: PaneSide = 'left'): FolderTab {
  const pane = s.panes[paneKeyOf(side, device)]
  if (!pane) return defaultTab()
  return pane.tabs.find((t) => t.id === pane.activeId) ?? pane.tabs[0] ?? defaultTab()
}

function activeTabOfPane(pane: PaneTabsState): FolderTab {
  return pane.tabs.find((t) => t.id === pane.activeId) ?? pane.tabs[0] ?? defaultTab()
}

export const useFiles = create<FilesState>()((set, get) => {
  const ensurePane = (s: FilesState, device: DeviceId, side: PaneSide): PaneTabsState =>
    s.panes[paneKeyOf(side, device)] ?? createPaneTabsState(device, ROOT)

  const patchActiveTab = (device: DeviceId, side: PaneSide, patch: Partial<FolderTab>) =>
    set((s) => {
      const pk = paneKeyOf(side, device)
      const pane = ensurePane(s, device, side)
      const tabs = pane.tabs.map((t) => (t.id === pane.activeId ? { ...t, ...patch } : t))
      return { panes: { ...s.panes, [pk]: { ...pane, tabs } } }
    })

  async function load(device: DeviceId, path: string) {
    const key = keyOf(device, path)
    const token = (requestSeq[key] ?? 0) + 1
    requestSeq[key] = token
    if (path !== ROOT) {
      streamStates[key] = { token, receivedFirst: false, entries: [], seen: new Set() }
    }
    set((s) => ({ loading: { ...s.loading, [key]: true }, streaming: { ...s.streaming, [key]: undefined } }))

    // 任务 2.1：二次进入目录时先尝试 < 16ms 读持久缓存显示，再后台拉最新目录做增量差异合并
    if (path !== ROOT && !get().listings[key]) {
      try {
        const hit = await backend.fsCacheGet(device, path)
        if (
          hit &&
          requestSeq[key] === token &&
          !streamStates[key]?.receivedFirst &&
          !get().listings[key]
        ) {
          set((s) => ({
            listings: { ...s.listings, [key]: { entries: hit.entries, at: hit.cachedAt } },
          }))
        }
      } catch {
        // 缓存未命中忽略
      }
    }

    try {
      if (path === ROOT) {
        const roots = await backend.fsRoots(device)
        if (requestSeq[key] !== token) return
        set((s) => ({ roots: { ...s.roots, [device]: roots }, errors: { ...s.errors, [key]: undefined } }))
      } else {
        const listing = await backend.fsList(device, path)
        if (requestSeq[key] !== token) return
        set((s) => {
          const prev = s.listings[key]?.entries ?? []
          const merged = applyListingDiff(prev, listing.entries)
          return {
            listings: { ...s.listings, [key]: { entries: merged, at: Date.now() } },
            streaming: { ...s.streaming, [key]: undefined },
            errors: { ...s.errors, [key]: undefined },
          }
        })
      }
    } catch (e) {
      if (requestSeq[key] !== token) return
      set((s) => ({
        streaming: { ...s.streaming, [key]: undefined },
        errors: { ...s.errors, [key]: { code: errorCode(e), msg: humanError(e) } },
      }))
    } finally {
      if (requestSeq[key] === token) {
        delete streamStates[key]
        set((s) => ({ loading: { ...s.loading, [key]: false } }))
      }
    }
  }

  const fullPaths = (dir: string, names: string[]) => names.map((n) => joinPath(dir, n))

  async function guarded<T>(fn: () => Promise<T>): Promise<T | undefined> {
    try {
      return await fn()
    } catch (e) {
      toast.error(humanError(e))
      return undefined
    }
  }

  async function transferWithConflictResolution(
    src: DeviceId,
    sources: string[],
    dst: DeviceId,
    destDir: string,
  ) {
    if (!sources.length) return
    const itemNames = sources.map(baseName)
    const conflictNames = await guarded(() => backend.fsCheckConflicts(dst, destDir, itemNames))
    // 冲突检查失败时直接中止，不吞错盲传
    if (conflictNames === undefined) return

    const dstLabel = dst === SELF_ID ? '本机' : `“${deviceName(dst)}”`
    if (conflictNames.length === 0) {
      const id = await guarded(() => backend.transfer(src, sources, dst, destDir, 'rename'))
      if (id) registerTask(id, { device: dst, dirs: [destDir] })
      return
    }

    const conflictSet = new Set(conflictNames.map((n) => n.toLowerCase()))
    const buckets: Record<ConflictPolicy, string[]> = {
      overwrite: [],
      skip: [],
      rename: [],
    }
    const cleanSources: string[] = []
    const conflictingSources: Array<{ path: string; name: string }> = []

    for (const s of sources) {
      const n = baseName(s)
      if (conflictSet.has(n.toLowerCase())) conflictingSources.push({ path: s, name: n })
      else cleanSources.push(s)
    }

    let decidedAll: ConflictPolicy | null = null
    for (let i = 0; i < conflictingSources.length; i++) {
      const item = conflictingSources[i]
      if (decidedAll) {
        buckets[decidedAll].push(item.path)
        continue
      }
      const choice = await askConflictChoice({
        fileName: item.name,
        destDeviceName: dstLabel,
        remainingCount: conflictingSources.length - i,
      })
      if (!choice) return
      if (choice.applyToAll) decidedAll = choice.policy
      buckets[choice.policy].push(item.path)
    }

    // 无冲突项合并进 rename 或 overwrite 批次，减少任务拆分
    if (cleanSources.length) buckets.rename.push(...cleanSources)

    for (const policy of ['overwrite', 'rename'] as const) {
      const list = buckets[policy]
      if (!list.length) continue
      const id = await guarded(() => backend.transfer(src, list, dst, destDir, policy))
      if (id) registerTask(id, { device: dst, dirs: [destDir] })
    }
    if (buckets.skip.length && !buckets.overwrite.length && !buckets.rename.length) {
      toast.info(`已跳过 ${buckets.skip.length} 个同名项`)
    }
  }

  return {
    panes: {},
    listings: {},
    streaming: {},
    loading: {},
    errors: {},
    roots: {},
    sort: { key: 'name', asc: true },
    clipboard: null,
    renaming: null,

    open(device, path, mode = 'push', side = 'left') {
      const v = viewOf(get(), device, side)
      if (mode === 'push' && path === v.path) return get().refresh(device, side)
      const patch: Partial<FolderTab> = { path, selected: new Set(), anchor: null, focus: null }
      if (mode === 'push') Object.assign(patch, { back: [...v.back, v.path].slice(-50), fwd: [] })
      if (mode === 'back') Object.assign(patch, { back: v.back.slice(0, -1), fwd: [v.path, ...v.fwd] })
      if (mode === 'fwd') Object.assign(patch, { back: [...v.back, v.path], fwd: v.fwd.slice(1) })
      if (parentPath(v.path) === path && v.path !== ROOT) patch.focus = baseName(v.path)
      patchActiveTab(device, side, patch)
      set({ renaming: null })
      void load(device, path)
    },

    newTab(device, path, side = 'left') {
      const target = path ?? viewOf(get(), device, side).path
      const pk = paneKeyOf(side, device)
      set((s) => {
        const pane = openTabInPane(ensurePane(s, device, side), target)
        return { panes: { ...s.panes, [pk]: pane }, renaming: null }
      })
      void load(device, target)
    },

    activateTab(device, tabId, side = 'left') {
      const pk = paneKeyOf(side, device)
      set((s) => {
        const pane = activateTabInPane(ensurePane(s, device, side), tabId)
        return { panes: { ...s.panes, [pk]: pane }, renaming: null }
      })
      const active = viewOf(get(), device, side)
      void load(device, active.path)
    },

    closeTab(device, tabId, side = 'left') {
      const pk = paneKeyOf(side, device)
      set((s) => {
        const pane = closeTabInPane(ensurePane(s, device, side), tabId)
        return { panes: { ...s.panes, [pk]: pane }, renaming: null }
      })
      const active = viewOf(get(), device, side)
      void load(device, active.path)
    },

    refresh(device, side = 'left') {
      void load(device, viewOf(get(), device, side).path)
    },
    refreshPath(device, path) {
      void load(device, path)
    },
    goBack(device, side = 'left') {
      const v = viewOf(get(), device, side)
      if (v.back.length) get().open(device, v.back[v.back.length - 1], 'back', side)
    },
    goForward(device, side = 'left') {
      const v = viewOf(get(), device, side)
      if (v.fwd.length) get().open(device, v.fwd[0], 'fwd', side)
    },
    goUp(device, side = 'left') {
      const v = viewOf(get(), device, side)
      if (v.path !== ROOT) get().open(device, parentPath(v.path), 'push', side)
    },
    select(device, selected, focus, anchor, side = 'left') {
      patchActiveTab(device, side, anchor === undefined ? { selected, focus } : { selected, focus, anchor })
    },
    setSort(key) {
      set((s) => ({ sort: { key, asc: s.sort.key === key ? !s.sort.asc : true } }))
    },

    copy(device, names, cut, side = 'left') {
      if (!names.length) return
      const dir = viewOf(get(), device, side).path
      set({ clipboard: { device, dir, names, cut } })
      toast.info(`已${cut ? '剪切' : '复制'} ${names.length} 项，到目标文件夹按 Ctrl+V 粘贴`)
    },

    async paste(device, side = 'left') {
      const clip = get().clipboard
      const dir = viewOf(get(), device, side).path
      if (!clip) return toast.info('剪贴板是空的，先选中文件按 Ctrl+C 复制')
      if (dir === ROOT) return toast.warn('请先进入一个文件夹再粘贴')
      const sources = fullPaths(clip.dir, clip.names)
      if (clip.device === device) {
        if (clip.cut && keyOf(device, clip.dir) === keyOf(device, dir)) return
        const id = await guarded(() => (clip.cut ? backend.fsMove(device, sources, dir) : backend.fsCopy(device, sources, dir)))
        if (!id) return
        registerTask(id, { device, dirs: [dir, clip.dir] })
        if (clip.cut) set({ clipboard: null })
        return
      }
      if (clip.cut) toast.info('跨设备粘贴只做复制，原来的文件保留在原处')
      await transferWithConflictResolution(clip.device, sources, device, dir)
    },

    async remove(device, names, permanent, side = 'left') {
      if (!names.length) return
      const dir = viewOf(get(), device, side).path
      const what = names.length === 1 ? `“${names[0]}”` : `这 ${names.length} 项`
      const where = device === SELF_ID ? '本机' : `“${deviceName(device)}”`
      const ok = await confirmDialog(
        permanent
          ? { title: `永久删除${what}？`, body: `将从${where}上直接删除，不进回收站，无法恢复。`, okText: '永久删除', danger: true }
          : { title: `删除${what}？`, body: `会移到${where}的回收站，需要时可以从回收站还原。`, okText: '删除' },
      )
      if (!ok) return
      const deleted = await guarded(async () => {
        await backend.fsDelete(device, fullPaths(dir, names), permanent)
        return true
      })
      if (!deleted) return
      get().select(device, new Set(), null, undefined, side)
      await load(device, dir)
    },

    startRename(device, name, side = 'left') {
      set({ renaming: { side, device, name } })
    },
    cancelRename() {
      set({ renaming: null })
    },
    async commitRename(device, oldName, newName, side = 'left') {
      const dir = viewOf(get(), device, side).path
      if (newName === oldName) {
        set({ renaming: null })
        return true
      }
      const ok = await guarded(async () => {
        await backend.fsRename(device, joinPath(dir, oldName), newName)
        return true
      })
      if (!ok) return false
      set({ renaming: null })
      get().select(device, new Set([newName]), newName, newName, side)
      await load(device, dir)
      return true
    },

    async newFolder(device, side = 'left') {
      const dir = viewOf(get(), device, side).path
      if (dir === ROOT) return toast.warn('请先进入一个磁盘或文件夹')
      const existing = new Set((get().listings[keyOf(device, dir)]?.entries ?? []).map((e) => e.name.toLowerCase()))
      let name = '新建文件夹'
      for (let i = 2; existing.has(name.toLowerCase()); i++) name = `新建文件夹 (${i})`
      const ok = await guarded(async () => {
        await backend.fsMkdir(device, joinPath(dir, name))
        return true
      })
      if (!ok) return
      await load(device, dir)
      get().select(device, new Set([name]), name, name, side)
      set({ renaming: { side, device, name } })
    },

    async upload(device, side = 'left') {
      const dir = viewOf(get(), device, side).path
      if (dir === ROOT) return toast.warn('请先进入要上传到的文件夹')
      const files = await guarded(() => backend.pickLocalFiles())
      if (!files?.length) return
      await transferWithConflictResolution(SELF_ID, files, device, dir)
    },

    async uploadFolder(device, side = 'left') {
      const dir = viewOf(get(), device, side).path
      if (dir === ROOT) return toast.warn('请先进入要上传到的文件夹')
      const folder = await guarded(() => backend.pickLocalDir())
      if (!folder) return
      await transferWithConflictResolution(SELF_ID, [folder], device, dir)
    },

    async download(device, names, side = 'left') {
      if (!names.length) return
      const dir = viewOf(get(), device, side).path
      const local = await guarded(() => backend.pickLocalDir())
      if (!local) return
      await transferWithConflictResolution(device, fullPaths(dir, names), SELF_ID, local)
    },

    async dropLocalPaths(device, localPaths, side = 'left') {
      if (!localPaths.length) return
      const dir = viewOf(get(), device, side).path
      if (dir === ROOT) return toast.warn('请先进入一个文件夹，再拖入文件')
      if (device === SELF_ID) {
        const id = await guarded(() => backend.fsCopy(SELF_ID, localPaths, dir))
        if (id) registerTask(id, { device: SELF_ID, dirs: [dir] })
        return
      }
      await transferWithConflictResolution(SELF_ID, localPaths, device, dir)
    },

    async dropBetweenPanes(srcDevice, srcDir, names, dstDevice, dstDir) {
      if (!names.length) return
      if (dstDir === ROOT) return toast.warn('请先在目标栏进入一个文件夹再松手')
      if (srcDevice === dstDevice && keyOf(srcDevice, srcDir) === keyOf(dstDevice, dstDir)) return
      const sources = fullPaths(srcDir, names)
      if (srcDevice === dstDevice) {
        const id = await guarded(() => backend.fsCopy(dstDevice, sources, dstDir))
        if (id) registerTask(id, { device: dstDevice, dirs: [dstDir] })
        return
      }
      await transferWithConflictResolution(srcDevice, sources, dstDevice, dstDir)
    },

    async openEntry(device, entry, side = 'left') {
      const dir = viewOf(get(), device, side).path
      const path = joinPath(dir, entry.name)
      if (entry.dir) return get().open(device, path, 'push', side)
      const res = await guarded(() => backend.fsOpenRemote(device, path))
      if (!res) return
      const where = device === SELF_ID ? '本机' : `“${deviceName(device)}”`
      if (res.elevationPrompt) {
        toast.warn(
          `打开“${entry.name}”需要管理员权限，${where}的屏幕上弹出了确认框，需要有人在那台电脑前点“是”`,
        )
      } else if (res.desktop.place === 'locked') {
        toast.warn(`已在${where}上打开“${entry.name}”，但对方现在是锁屏状态，屏幕上看不到`)
      } else if (res.desktop.place !== 'normal') {
        toast.warn(`已在${where}上打开“${entry.name}”，但对方现在没有可见的桌面`)
      } else {
        toast.success(`已在${where}上打开“${entry.name}”`)
      }
    },
  }
})

/** 按归一化路径刷新所有正在浏览该目录的栏位，否则仅失效缓存。 */
function refreshOrInvalidateDir(device: DeviceId, path: string) {
  const s = useFiles.getState()
  const key = keyOf(device, path)
  let viewingPath: string | null = null
  for (const pane of Object.values(s.panes)) {
    if (pane.device !== device) continue
    const tab = activeTabOfPane(pane)
    if (keyOf(device, tab.path) === key) {
      viewingPath = tab.path
      break
    }
  }
  if (viewingPath != null) {
    s.refreshPath(device, viewingPath)
  } else {
    useFiles.setState((st) => {
      if (!(key in st.listings)) return st
      const listings = { ...st.listings }
      delete listings[key]
      return { listings }
    })
  }
}

/** 订阅大目录渐进首屏批次与目录变化实时推送（任务 2.2、2.3）。 */
export function wireFileEvents(): () => void {
  const offs = [
    backend.on('fs-list-batch', (ev) => {
      const key = keyOf(ev.device, ev.path)
      const stream = streamStates[key]
      if (!stream || stream.token !== requestSeq[key]) return

      useFiles.setState((s) => {
        if (!s.loading[key]) return s
        if (!stream.receivedFirst) {
          stream.receivedFirst = true
          stream.entries = []
          stream.seen.clear()
        }
        for (const item of ev.entries) {
          if (stream.seen.has(item.name)) continue
          stream.seen.add(item.name)
          stream.entries.push(item)
        }
        const snapshot = stream.entries.slice()
        return {
          listings: { ...s.listings, [key]: { entries: snapshot, at: Date.now() } },
          streaming: {
            ...s.streaming,
            [key]: ev.done ? undefined : { loaded: snapshot.length, totalHint: ev.totalHint },
          },
        }
      })
    }),
    backend.on('fs-changed', ({ device, path }) => {
      refreshOrInvalidateDir(device, path)
    }),
  ]
  return () => offs.forEach((f) => f())
}

setTaskDoneHandler(({ device, dirs }) => {
  const seenKeys = new Set<string>()
  for (const dir of dirs) {
    const k = keyOf(device, dir)
    if (seenKeys.has(k)) continue
    seenKeys.add(k)
    refreshOrInvalidateDir(device, dir)
  }
})
