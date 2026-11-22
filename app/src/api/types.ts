// 界面和核心层之间交换的数据类型。
// 字段名与《总体方案》第 6 节的协议保持一致；桥接层（src-tauri）负责把核心层的结构转成这些形状。

/** 设备身份码：64 位十六进制。本机固定用 SELF_ID 表示。 */
export type DeviceId = string
export const SELF_ID = 'self'

export type LinkKind = 'local' | 'direct' | 'relay'

export type DeviceStatus =
  | { state: 'offline'; lastSeen?: number }
  | { state: 'connecting' }
  | { state: 'online'; link: LinkKind; rttMs: number }

/** 第一册 7.3 节的权限档：这台设备能对本机做什么。 */
export interface Permissions {
  readFiles: boolean
  writeFiles: boolean
  apps: boolean // 开软件、管进程
  screenshot: boolean
  input: boolean
  terminal: boolean
  power: boolean
  clipboard: boolean
}

export const DEFAULT_PERMISSIONS: Permissions = {
  readFiles: true,
  writeFiles: true,
  apps: true,
  screenshot: false,
  input: false,
  terminal: false,
  power: false,
  clipboard: false,
}

/** 对方支持的功能（来自 sys.hello）。不支持的功能在界面上置灰。 */
export type Feature =
  | 'fs'
  | 'transfer'
  | 'apps'
  | 'procs'
  | 'shot'
  | 'input'
  | 'power'
  | 'clip'
  | 'term'
  | 'search'

export interface Device {
  id: DeviceId
  name: string
  os: 'windows' | 'macos' | 'linux' | 'unknown'
  status: DeviceStatus
  features: Feature[]
  /** 是否已记录该电脑的物理网卡地址（支持睡眠时网络唤醒，任务 3.5）。 */
  hasMac?: boolean
  /** 我授予这台设备的权限；本机没有这个字段。 */
  permissions?: Permissions
}

export interface SelfInfo {
  id: DeviceId // 真实的 64 位身份码
  name: string
  version: string
}

export interface Settings {
  deviceName: string
  /** 普通权限随登录后台自启（任务 2.7，注册表 Run 附带 --background）。 */
  autostart?: boolean
  /** 以管理员权限随登录启动（任务 1.9 / 2.7，通过计划任务实现）。 */
  autostartElevated: boolean
  /** 是否已完成首次启动时的随登录自启询问（任务 2.7）。 */
  autostartPrompted?: boolean
  /** 全局呼出快捷搜索框热键（任务 3.1，默认 Alt+Space）。 */
  quickHotkey?: string
  /** 是否开启跨设备剪贴板自动同步（任务 3.7，默认 false）。 */
  clipboardAutoSync?: boolean
}

export interface HostStatus {
  paused: boolean
  workingSetBytes: number
}

// ───────────── 文件 ─────────────

export interface FsEntry {
  name: string
  dir: boolean
  size: number
  /** 修改时间，毫秒时间戳。 */
  mtime: number
}

export interface FsRoot {
  name: string
  path: string
  kind: 'drive' | 'desktop' | 'downloads' | 'documents' | 'home'
  total?: number
  free?: number
}

export interface FsListing {
  path: string
  entries: FsEntry[]
}

export interface FsCacheHit {
  path: string
  entries: FsEntry[]
  cachedAt: number
}

export interface FsListBatchEvent {
  device: DeviceId
  path: string
  entries: FsEntry[]
  totalHint?: number
  done: boolean
}

export interface FsChangedEvent {
  device: DeviceId
  path: string
}

// ───────────── 软件与进程 ─────────────

export interface AppInfo {
  id: string
  name: string
}

/** 被控端所处的桌面环境，决定打开的软件能不能被看到。 */
export interface DesktopState {
  /** normal：正常桌面；locked：锁屏或权限确认界面；service：系统服务环境；remote：非控制台会话 */
  place: 'normal' | 'locked' | 'service' | 'remote'
  elevated: boolean
}

export interface LaunchResult {
  newWindows: string[]
  desktop: DesktopState
  /** 目标需要管理员权限，对方屏幕上正弹着确认框。 */
  elevationPrompt?: boolean
}

export interface ProcInfo {
  pid: number
  name: string
  title: string
  memory: number // 字节
  cpu: number // 百分比
  critical: boolean
}

// ───────────── 配对与局域网发现 ─────────────

export interface PairOffer {
  code: string // 6 位数字
  expiresAt: number
  invite: string // 身份码 + 配对码打成的一串，可整串粘贴
}

export interface PairRequest {
  requestId: string
  deviceId: DeviceId
  deviceName: string
}

export interface LanPeer {
  endpointId: DeviceId
  deviceName: string
  os: string
  addrs: string[]
  seenAtMs: number
}

// ───────────── 任务（传输、对方本机复制移动） ─────────────

export type TaskKind = 'upload' | 'download' | 'relay' | 'copy' | 'move'
export type TaskState = 'running' | 'paused' | 'done' | 'failed' | 'cancelled'

export interface TaskProgress {
  id: string
  kind: TaskKind
  title: string
  srcDevice: DeviceId
  dstDevice: DeviceId
  bytesDone: number
  bytesTotal: number
  speed: number // 字节/秒
  state: TaskState
  error?: ApiErrorBody
}

export type ConflictPolicy = 'overwrite' | 'skip' | 'rename'

// ───────────── 诊断与网络体检（任务 2.11） ─────────────

export interface DiagPeerRoute {
  peerId: DeviceId
  name: string
  state: 'online_direct' | 'online_relay' | 'connecting' | 'offline'
  detail: string
  rttMs?: number
}

export interface DiagSummary {
  appVersion: string
  endpointId: string
  deviceName: string
  os: string
  workingSetBytes: number
  hostPaused: boolean
  speedLimitBps: number | null
  netcheckTargets: string[]
  peerRoutes: DiagPeerRoute[]
}

export interface RelayLatencyItem {
  url: string
  rttMs: number | null
}

export interface NetcheckReport {
  checkedAtMs: number
  udpIpv4: boolean
  udpIpv6: boolean
  mappingVariesByDestIp: boolean | null
  upnpAvailable: boolean | null
  pcpAvailable: boolean | null
  natPmpAvailable: boolean | null
  preferredRelay: string | null
  relayLatencies: RelayLatencyItem[]
  summary: string
}

export interface AuditEntry {
  ts: string
  peerId: string
  peerName: string
  cmd: string
  detail?: string
  ok: boolean
}

export interface DiagLogs {
  runtimeLogs: string[]
  audits: AuditEntry[]
}

// ───────────── 错误 ─────────────

export type ErrorCode =
  | 'unauthorized'
  | 'forbidden'
  | 'not_found'
  | 'already_exists'
  | 'busy'
  | 'invalid'
  | 'unsupported'
  | 'io'
  | 'cancelled'
  | 'internal'
  | 'offline'
  | 'timeout'
  | 'bad_code'
  | 'expired'

export interface ApiErrorBody {
  code: ErrorCode
  msg: string
}

/** 桥接层每条命令的统一返回结构（任务 1.11）。 */
export type Reply<T> = { ok: true; data: T } | { ok: false; err: ApiErrorBody }

// ───────────── 第 3 阶段：搜索、看一眼与点按、本地编辑回传、电源唤醒、终端、剪贴板、批量操作 ─────────────

export interface FsSearchHit {
  name: string
  path: string
  dir: boolean
  size: number
  mtime: number
}

export interface FsSearchBatchEvent {
  searchId: string
  device: DeviceId
  entries: FsSearchHit[]
  scannedDirs: number
  done: boolean
}

export interface QuickHit {
  peerId: DeviceId
  peerName: string
  online: boolean
  kind: 'app' | 'folder'
  title: string
  subtitle: string
  target: string
  launchKind: 'app_id' | 'app_name' | 'path'
}

export interface MonitorInfo {
  index: number
  name: string
  originX: number
  originY: number
  width: number
  height: number
  dpiScalePercent: number
  isPrimary: boolean
}

export interface CapturableWindow {
  hwnd: string
  pid: number
  title: string
  width: number
  height: number
}

export interface ShotTargets {
  monitors: MonitorInfo[]
  windows: CapturableWindow[]
  desktop: DesktopState
}

export interface ShotTransform {
  originX: number
  originY: number
  sourceWidth: number
  sourceHeight: number
  imageWidth: number
  imageHeight: number
  dpiScalePercent: number
  targetHwnd?: string
  monitorIndex?: number
}

export interface CapturedShot {
  imageDataUrl: string
  byteLen: number
  capturedAtMs: number
  transform: ShotTransform
  desktop: DesktopState
}

export interface InputOutcome {
  uipiBlocked: boolean
  warning?: string
  refreshedShot?: CapturedShot
}

export interface LocalEditSession {
  sessionId: string
  peerId: DeviceId
  peerName: string
  remotePath: string
  localPath: string
  fileName: string
}

export interface EditSyncPrompt {
  sessionId: string
  peerId: DeviceId
  peerName: string
  remotePath: string
  localPath: string
  fileName: string
  conflict: boolean
}

export interface EditSyncResult {
  conflict: boolean
  bytesWritten: number
}

export type PowerActionKind = 'lock' | 'sleep' | 'restart' | 'shutdown'

export interface PowerResult {
  action: PowerActionKind
  cancelled: boolean
  executed: boolean
  message?: string
}

export interface WakeResult {
  localPackets: number
  relayPeersNotified: number
  relayedPackets: number
}

export interface TermSession {
  sessionId: string
  pid: number
  shellName: string
}

export interface TermOutputEvent {
  sessionId: string
  dataB64: string
}

export interface TermExitEvent {
  sessionId: string
  exitCode?: number
}

export interface ClipboardPreview {
  kind: 'empty' | 'text' | 'image_png'
  text?: string
  imageDataUrl?: string
  width?: number
  height?: number
}

export interface BatchDeviceResult {
  peerId: DeviceId
  peerName: string
  ok: boolean
  summary: string
  output?: string
  elapsedMs: number
}

export interface SystemNotice {
  title: string
  body: string
  kind: 'info' | 'success' | 'warn' | 'error'
}

// ───────────── 实时桌面会话（H.264 视频 + 原始键鼠，旧版对端自动走逐步截图） ─────────────

export interface DesktopOpened {
  sessionId: number
  /** 对端支持原始鼠标键盘（移动、按下、松开、滚轮、按键）；否则只能单击、双击、文字和组合键。 */
  rawInput: boolean
  /** 请求了“对方分辨率跟随本机”时的结果 */
  fit?: DesktopFit
  /** 调整分辨率没成功的原因；会话本身照常可用 */
  fitError?: string
}

/** 对方显示器临时调整分辨率的结果；applied 为假时保持原样，note 说明原因。 */
export interface DesktopFit {
  applied: boolean
  width: number
  height: number
  previousWidth: number
  previousHeight: number
  note?: string | null
}

/** desktop_next 返回的二进制帧头。 */
export interface DesktopFrameMeta {
  mode: 'live' | 'step'
  format: 'h264-annexb' | 'jpeg'
  timestampUs: number
  key: boolean
  epoch: number
  /** 含 SPS 的 H.264 关键帧才有，例如 avc1.42C028 */
  codec?: string | null
  encoder: string
  /** 进入逐步模式的原因；正常首屏可为空 */
  reason?: string | null
  transform: ShotTransform
}

export interface DesktopFrame {
  meta: DesktopFrameMeta
  /** JPEG 图片或一个完整的 H.264 Annex B 访问单元 */
  data: Uint8Array<ArrayBuffer>
}

export type DesktopMouseButton = 'left' | 'right' | 'middle'

/** 所有 x、y 都是远端虚拟桌面物理像素。 */
export type DesktopInputEvent =
  | { kind: 'move'; x: number; y: number }
  | { kind: 'button'; x: number; y: number; button: DesktopMouseButton; down: boolean }
  | { kind: 'wheel'; x: number; y: number; deltaX: number; deltaY: number }
  | { kind: 'key'; vk: number; down: boolean }
  | { kind: 'click'; x: number; y: number; button: DesktopMouseButton; double?: boolean }
  | { kind: 'text'; text: string }
  | { kind: 'shortcut'; combo: string }
  | { kind: 'release' }
