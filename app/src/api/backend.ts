import type {
  AppInfo,
  BatchDeviceResult,
  CapturedShot,
  ClipboardPreview,
  ConflictPolicy,
  Device,
  DesktopFrame,
  DesktopInputEvent,
  DesktopOpened,
  DeviceId,
  DiagLogs,
  DiagSummary,
  EditSyncPrompt,
  EditSyncResult,
  FsCacheHit,
  FsChangedEvent,
  FsListBatchEvent,
  FsListing,
  FsRoot,
  FsSearchBatchEvent,
  HostStatus,
  InputOutcome,
  LanPeer,
  LaunchResult,
  LocalEditSession,
  NetcheckReport,
  PairOffer,
  PairRequest,
  Permissions,
  PowerActionKind,
  PowerResult,
  ProcInfo,
  QuickHit,
  SelfInfo,
  Settings,
  ShotTargets,
  SystemNotice,
  TaskProgress,
  TermExitEvent,
  TermOutputEvent,
  TermSession,
  WakeResult,
} from './types'

/** 核心层推送给界面的事件。 */
export interface BackendEvents {
  'device-status': Device
  'device-removed': { id: DeviceId }
  'pair-request': PairRequest
  'task-progress': TaskProgress
  'fs-list-batch': FsListBatchEvent
  'fs-changed': FsChangedEvent
  'lan-changed': LanPeer[]
  'host-paused': { paused: boolean }
  'launcher-toggle': undefined
  'fs-search-batch': FsSearchBatchEvent
  'term-output': TermOutputEvent
  'term-exit': TermExitEvent
  'edit-prompt': EditSyncPrompt
  'batch-item': BatchDeviceResult
  'app-notice': SystemNotice
}

export type Unlisten = () => void

/**
 * 界面能调用的全部功能。真实实现走 Tauri 命令（tauri.ts），
 * 没有 Tauri 时（浏览器里开发界面）用模拟实现（mock.ts）。
 * 所有方法失败时抛出 ApiError。
 */
export interface Backend {
  readonly kind: 'tauri' | 'mock'

  // 本机与后台被控状态
  selfInfo(): Promise<SelfInfo>
  getSettings(): Promise<Settings>
  setSettings(settings: Settings): Promise<void>
  hostStatus(): Promise<HostStatus>
  hostSetPaused(paused: boolean): Promise<void>

  // 设备与局域网发现
  listDevices(): Promise<Device[]>
  renameDevice(id: DeviceId, name: string): Promise<void>
  removeDevice(id: DeviceId): Promise<void>
  setPermissions(id: DeviceId, permissions: Permissions): Promise<void>
  reconnect(id: DeviceId): Promise<void>
  lanList(): Promise<LanPeer[]>

  // 配对
  pairBegin(): Promise<PairOffer>
  pairCancel(): Promise<void>
  /** target 可以是身份码，也可以是整串邀请码（此时 code 可留空）。 */
  pairConnect(target: string, code: string): Promise<Device>
  pairRespond(requestId: string, allow: boolean, permissions: Permissions): Promise<void>
  pairPendingList(): Promise<PairRequest[]>

  // 文件（device 为 SELF_ID 时操作本机）
  fsRoots(device: DeviceId): Promise<FsRoot[]>
  fsCacheGet(device: DeviceId, path: string): Promise<FsCacheHit | null>
  fsList(device: DeviceId, path: string): Promise<FsListing>
  fsWatch(device: DeviceId, path: string): Promise<void>
  fsUnwatch(device: DeviceId, path: string): Promise<void>
  fsCheckConflicts(device: DeviceId, destDir: string, names: string[]): Promise<string[]>
  fsMkdir(device: DeviceId, path: string): Promise<void>
  fsRename(device: DeviceId, path: string, newName: string): Promise<void>
  /** 在同一台设备内复制或移动，返回任务号，进度走 task-progress 事件。 */
  fsCopy(device: DeviceId, sources: string[], destDir: string): Promise<string>
  fsMove(device: DeviceId, sources: string[], destDir: string): Promise<string>
  fsDelete(device: DeviceId, paths: string[], permanent: boolean): Promise<void>

  /** 跨设备传输（支持文件与文件夹、4 并发队列、断点续传、甲→乙直传）。返回任务号。 */
  transfer(
    src: DeviceId,
    sources: string[],
    dst: DeviceId,
    destDir: string,
    conflict: ConflictPolicy,
  ): Promise<string>
  pauseTask(taskId: string): Promise<void>
  resumeTask(taskId: string): Promise<void>
  cancelTask(taskId: string): Promise<void>
  taskList(): Promise<TaskProgress[]>
  getSpeedLimit(): Promise<number | null>
  setSpeedLimit(bytesPerSec: number | null): Promise<void>

  /** 让用户在本机选文件或文件夹，取消时返回空数组 / null。 */
  pickLocalFiles(): Promise<string[]>
  pickLocalDir(): Promise<string | null>

  // 软件与进程
  appList(device: DeviceId, force?: boolean): Promise<AppInfo[]>
  appIcon(device: DeviceId, appId: string): Promise<string | null>
  /** target 为软件标识或文件路径。 */
  appLaunch(device: DeviceId, target: string, args?: string): Promise<LaunchResult>
  procList(device: DeviceId): Promise<ProcInfo[]>
  procKill(device: DeviceId, pid: number): Promise<void>

  // 诊断与网络体检（任务 2.11）
  diagSummary(): Promise<DiagSummary>
  diagNetcheck(): Promise<NetcheckReport>
  diagLogs(limit?: number): Promise<DiagLogs>
  diagExport(destDir: string): Promise<string>

  // 第 3 阶段：快捷搜索、流式文件搜索、看一眼与点按、本地编辑回传、电源唤醒、伪终端、剪贴板、批量操作、拖出到桌面
  launcherConsumePending(): Promise<boolean>
  quickSearch(query: string, limit?: number): Promise<QuickHit[]>
  quickLaunch(device: DeviceId, target: string, kind: 'app_id' | 'app_name' | 'path'): Promise<LaunchResult>

  fsSearchStart(device: DeviceId, path: string, query: string): Promise<string>
  fsSearchCancel(searchId: string): Promise<void>
  fsOpenRemote(device: DeviceId, path: string): Promise<LaunchResult>
  fsOpenLocalEdit(device: DeviceId, path: string, launchApp?: boolean): Promise<LocalEditSession>
  fsEditSyncBack(sessionId: string, autoSync: boolean, forceOverwrite: boolean): Promise<EditSyncResult>
  editPendingList(): Promise<EditSyncPrompt[]>
  fsDragOut(device: DeviceId, paths: string[]): Promise<boolean>

  shotMonitors(device: DeviceId): Promise<ShotTargets>
  shotCapture(opts: {
    device: DeviceId
    monitorIndex?: number
    hwnd?: string
    maxWidth?: number
    quality?: number
  }): Promise<CapturedShot>
  inputClick(opts: {
    device: DeviceId
    screenX: number
    screenY: number
    button: 'left' | 'right' | 'middle'
    double: boolean
    targetHwnd?: string
    refreshMonitor?: number
    refreshHwnd?: string
    maxWidth?: number
  }): Promise<InputOutcome>
  inputKey(opts: {
    device: DeviceId
    text?: string
    keyCombo?: string
    targetHwnd?: string
    refreshMonitor?: number
    refreshHwnd?: string
    maxWidth?: number
  }): Promise<InputOutcome>

  // 实时桌面会话：远程设备用；查看本机仍用 shotCapture
  desktopOpen(opts: { device: DeviceId; monitorIndex?: number; hwnd?: string }): Promise<DesktopOpened>
  /** 取下一帧；逐步模式下会一直等到有输入、手动刷新或链路恢复。会话关闭时以 not_found 失败。 */
  desktopNext(sessionId: number): Promise<DesktopFrame>
  /** 每批 1～64 个事件，等上一批确认再发下一批。 */
  desktopInput(sessionId: number, events: DesktopInputEvent[]): Promise<void>
  desktopRefresh(sessionId: number): Promise<void>
  desktopClose(sessionId: number): Promise<void>

  powerExec(device: DeviceId, action: PowerActionKind, countdownSecs?: number): Promise<PowerResult>
  powerCancel(device: DeviceId): Promise<boolean>
  powerWake(device: DeviceId): Promise<WakeResult>

  termOpen(device: DeviceId, shell: 'powershell' | 'cmd', cols: number, rows: number, cwd?: string): Promise<TermSession>
  termWrite(sessionId: string, dataB64: string): Promise<void>
  termResize(sessionId: string, cols: number, rows: number): Promise<void>
  termClose(sessionId: string): Promise<void>

  clipPush(device: DeviceId): Promise<string>
  clipPull(device: DeviceId): Promise<ClipboardPreview>
  clipPeek(device: DeviceId): Promise<ClipboardPreview>

  batchExec(opts: {
    devices: DeviceId[]
    op: 'launch_app' | 'power' | 'run_command'
    target?: string
    powerAction?: PowerActionKind
    command?: string
    shell?: 'powershell' | 'cmd'
  }): Promise<BatchDeviceResult[]>

  on<K extends keyof BackendEvents>(event: K, cb: (payload: BackendEvents[K]) => void): Unlisten
}
