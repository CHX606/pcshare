import type { DeviceId, FsEntry } from '../api/types'
import { ROOT, baseName } from './path'

export interface FolderTab {
  id: string
  path: string
  back: string[]
  fwd: string[]
  selected: ReadonlySet<string>
  anchor: string | null
  focus: string | null
}

export interface PaneTabsState {
  device: DeviceId
  activeId: string
  tabs: FolderTab[]
}

let tabSeq = 1

export function newFolderTab(path = ROOT): FolderTab {
  return {
    id: `tab-${tabSeq++}`,
    path,
    back: [],
    fwd: [],
    selected: new Set(),
    anchor: null,
    focus: null,
  }
}

export function createPaneTabsState(device: DeviceId, initialPath = ROOT): PaneTabsState {
  const first = newFolderTab(initialPath)
  return {
    device,
    activeId: first.id,
    tabs: [first],
  }
}

export function openTabInPane(pane: PaneTabsState, path = ROOT): PaneTabsState {
  const tab = newFolderTab(path)
  return {
    ...pane,
    activeId: tab.id,
    tabs: [...pane.tabs, tab],
  }
}

export function activateTabInPane(pane: PaneTabsState, tabId: string): PaneTabsState {
  if (!pane.tabs.some((t) => t.id === tabId)) return pane
  return { ...pane, activeId: tabId }
}

export function closeTabInPane(pane: PaneTabsState, tabId: string): PaneTabsState {
  const idx = pane.tabs.findIndex((t) => t.id === tabId)
  if (idx < 0) return pane
  if (pane.tabs.length <= 1) {
    const reset = newFolderTab(ROOT)
    return { ...pane, activeId: reset.id, tabs: [reset] }
  }
  const nextTabs = pane.tabs.filter((t) => t.id !== tabId)
  const nextActive =
    pane.activeId === tabId
      ? nextTabs[Math.min(idx, nextTabs.length - 1)].id
      : pane.activeId
  return {
    ...pane,
    activeId: nextActive,
    tabs: nextTabs,
  }
}

const DEFAULT_TAB: FolderTab = {
  id: 'default',
  path: ROOT,
  back: [],
  fwd: [],
  selected: new Set(),
  anchor: null,
  focus: null,
}

const defaultPaneCache = new Map<DeviceId, PaneTabsState>()

export function defaultTab(): FolderTab {
  return DEFAULT_TAB
}

export function resolvePaneTabsState(
  panes: Record<DeviceId, PaneTabsState>,
  device: DeviceId,
): PaneTabsState {
  const existing = panes[device]
  if (existing) return existing
  let fallback = defaultPaneCache.get(device)
  if (!fallback) {
    fallback = { device, activeId: DEFAULT_TAB.id, tabs: [DEFAULT_TAB] }
    defaultPaneCache.set(device, fallback)
  }
  return fallback
}

export function tabLabelOf(path: string): string {
  if (path === ROOT) return '此电脑'
  return baseName(path) || path
}

/**
 * 任务 2.1：按名称对比缓存目录项与最新目录项：
 * - 若长度与每项元数据一致，直接返回 prev 引用，避免整页重排；
 * - 否则对未变项复用原对象引用，仅替换有变化的项。
 */
export function applyListingDiff(prev: FsEntry[], next: FsEntry[]): FsEntry[] {
  if (prev.length === 0) return next
  const prevMap = new Map<string, FsEntry>()
  for (const item of prev) prevMap.set(item.name, item)

  let changed = prev.length !== next.length
  const out = new Array<FsEntry>(next.length)

  for (let i = 0; i < next.length; i++) {
    const n = next[i]
    const old = prevMap.get(n.name)
    if (old && old.dir === n.dir && old.size === n.size && old.mtime === n.mtime) {
      out[i] = old
      if (!changed && prev[i] !== old) changed = true
    } else {
      out[i] = n
      changed = true
    }
  }

  return changed ? out : prev
}
