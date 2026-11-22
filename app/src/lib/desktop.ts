import type {
  DesktopFit,
  DesktopFrame,
  DesktopFrameMeta,
  DesktopInputEvent,
  ShotTargets,
  ShotTransform,
} from '../api/types'

/**
 * 实时桌面会话的纯函数部分：拆二进制帧、坐标换算、按键码、滚轮单位、输入排队。
 * 接口约定见 docs/桌面会话接口.md。
 */

/** 前 4 字节为大端长度 N，随后 N 字节 UTF-8 JSON，其余为 JPEG 或 H.264 访问单元。 */
export function parseDesktopPacket(raw: ArrayBuffer | ArrayBufferView | number[]): DesktopFrame {
  const bytes =
    raw instanceof ArrayBuffer
      ? new Uint8Array(raw)
      : Array.isArray(raw)
        ? Uint8Array.from(raw)
        : new Uint8Array(raw.buffer, raw.byteOffset, raw.byteLength).slice()
  if (bytes.byteLength < 4) throw new Error('桌面帧太短')
  const len = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength).getUint32(0, false)
  if (4 + len > bytes.byteLength) throw new Error('桌面帧头长度不对')
  const meta = JSON.parse(new TextDecoder().decode(bytes.subarray(4, 4 + len))) as DesktopFrameMeta
  return { meta, data: bytes.subarray(4 + len) }
}

/** 把一帧拼回二进制（模拟后端和测试用）。 */
export function buildDesktopPacket(meta: DesktopFrameMeta, data: Uint8Array): ArrayBuffer {
  const header = new TextEncoder().encode(JSON.stringify(meta))
  const out = new Uint8Array(4 + header.byteLength + data.byteLength)
  new DataView(out.buffer).setUint32(0, header.byteLength, false)
  out.set(header, 4)
  out.set(data, 4 + header.byteLength)
  return out.buffer
}

/** 界面能否用 WebCodecs 解码实时视频（WebView2 / Chromium 可以）。 */
export function canDecodeVideo(): boolean {
  return typeof window !== 'undefined' && 'VideoDecoder' in window && 'EncodedVideoChunk' in window
}

/** 选中的来源还在就保留，否则退回第一块显示器或第一个窗口。来源写成 mon:序号 或 win:句柄。 */
export function pickSourceChoice(t: ShotTargets, current: string | null): string {
  if (current) {
    const stillThere = current.startsWith('win:')
      ? t.windows.some((w) => `win:${w.hwnd}` === current)
      : t.monitors.some((m) => `mon:${m.index}` === current)
    if (stillThere) return current
  }
  if (t.monitors.length > 0) return `mon:${t.monitors[0].index}`
  if (t.windows.length > 0) return `win:${t.windows[0].hwnd}`
  return 'mon:0'
}

export function parseSourceChoice(choice: string): { monitorIndex?: number; hwnd?: string } {
  if (choice.startsWith('win:')) return { hwnd: choice.slice(4) }
  const idx = Number(choice.slice(4))
  return { monitorIndex: Number.isFinite(idx) ? idx : 0 }
}

/** 本机屏幕的物理像素：浏览器给的是 CSS 像素，乘以系统缩放比还原。 */
export function localScreenPixels(cssWidth: number, cssHeight: number, ratio: number): { width: number; height: number } {
  const r = Number.isFinite(ratio) && ratio > 0 ? ratio : 1
  return { width: Math.round(cssWidth * r), height: Math.round(cssHeight * r) }
}

/** 把对方分辨率的调整结果写成一句话；warn 为真时用醒目颜色。 */
export function describeFit(
  fit: DesktopFit | undefined,
  error: string | undefined,
): { text: string; warn: boolean } | null {
  if (error) return { text: `没能调整对方分辨率：${error}`, warn: true }
  if (!fit) return null
  if (fit.applied) {
    return {
      text: `已把对方屏幕临时改成 ${fit.width}×${fit.height}（原来 ${fit.previousWidth}×${fit.previousHeight}），关闭看屏后自动改回`,
      warn: false,
    }
  }
  if (fit.note) return { text: fit.note, warn: true }
  return { text: `对方屏幕已经是 ${fit.width}×${fit.height}，和本机一致`, warn: false }
}

export const QUICK_KEYS = [
  { label: 'Enter', combo: 'Enter' },
  { label: 'Esc', combo: 'Esc' },
  { label: 'Tab', combo: 'Tab' },
  { label: 'Backspace', combo: 'Backspace' },
  { label: 'Ctrl+C', combo: 'Ctrl+C' },
  { label: 'Ctrl+V', combo: 'Ctrl+V' },
  { label: 'Alt+F4', combo: 'Alt+F4' },
  { label: 'Win+D', combo: 'Win+D' },
]

interface Box {
  left: number
  top: number
  width: number
  height: number
}

/**
 * 指针位置 → 远端虚拟桌面物理像素：先换成显示矩形内 0～1 的比例，
 * 乘以原图宽高后向下取整，限制在 0…尺寸−1，再加上原点（多屏原点可为负）。
 */
export function mapPointToRemotePixel(
  clientX: number,
  clientY: number,
  rect: Box,
  t: Pick<ShotTransform, 'originX' | 'originY' | 'sourceWidth' | 'sourceHeight'>,
): { x: number; y: number } {
  const rx = (clientX - rect.left) / Math.max(1, rect.width)
  const ry = (clientY - rect.top) / Math.max(1, rect.height)
  const axis = (ratio: number, size: number) => {
    const max = Math.max(1, size) - 1
    return Math.min(max, Math.max(0, Math.floor(ratio * size)))
  }
  return { x: axis(rx, t.sourceWidth) + t.originX, y: axis(ry, t.sourceHeight) + t.originY }
}

const VK_NAMED: Record<string, number> = {
  Backspace: 0x08,
  Tab: 0x09,
  Enter: 0x0d,
  NumpadEnter: 0x0d,
  ShiftLeft: 0xa0,
  ShiftRight: 0xa1,
  ControlLeft: 0xa2,
  ControlRight: 0xa3,
  AltLeft: 0xa4,
  AltRight: 0xa5,
  Pause: 0x13,
  CapsLock: 0x14,
  Escape: 0x1b,
  Space: 0x20,
  PageUp: 0x21,
  PageDown: 0x22,
  End: 0x23,
  Home: 0x24,
  ArrowLeft: 0x25,
  ArrowUp: 0x26,
  ArrowRight: 0x27,
  ArrowDown: 0x28,
  PrintScreen: 0x2c,
  Insert: 0x2d,
  Delete: 0x2e,
  MetaLeft: 0x5b,
  MetaRight: 0x5c,
  ContextMenu: 0x5d,
  NumpadMultiply: 0x6a,
  NumpadAdd: 0x6b,
  NumpadSubtract: 0x6d,
  NumpadDecimal: 0x6e,
  NumpadDivide: 0x6f,
  NumLock: 0x90,
  ScrollLock: 0x91,
  Semicolon: 0xba,
  Equal: 0xbb,
  Comma: 0xbc,
  Minus: 0xbd,
  Period: 0xbe,
  Slash: 0xbf,
  Backquote: 0xc0,
  BracketLeft: 0xdb,
  Backslash: 0xdc,
  BracketRight: 0xdd,
  Quote: 0xde,
  IntlBackslash: 0xe2,
}

/** KeyboardEvent.code（物理键位）→ Windows 虚拟键码；认不出的返回 null。 */
export function vkFromCode(code: string): number | null {
  if (code in VK_NAMED) return VK_NAMED[code]
  let m = /^Key([A-Z])$/.exec(code)
  if (m) return m[1].charCodeAt(0)
  m = /^Digit([0-9])$/.exec(code)
  if (m) return 0x30 + Number(m[1])
  m = /^Numpad([0-9])$/.exec(code)
  if (m) return 0x60 + Number(m[1])
  m = /^F([0-9]{1,2})$/.exec(code)
  if (m) {
    const n = Number(m[1])
    if (n >= 1 && n <= 24) return 0x70 + n - 1
  }
  return null
}

const I16 = 32767
const clampI16 = (v: number) => Math.max(-I16, Math.min(I16, Math.round(v)))

/**
 * 浏览器滚轮 → Windows 滚轮单位（每格 120）。浏览器纵向正值向下，Windows 正值向上；横向两边都是正值向右。
 * deltaMode 0 为像素（Chromium 每格约 100），1 为行（每格 3 行），2 为页。
 */
export function wheelToWindows(deltaX: number, deltaY: number, deltaMode: number): { deltaX: number; deltaY: number } {
  const factor = deltaMode === 1 ? 40 : deltaMode === 2 ? 360 : 1.2
  return { deltaX: clampI16(deltaX * factor), deltaY: clampI16(-deltaY * factor) }
}

export const MAX_INPUT_BATCH = 64

/**
 * 输入排队：同一时间只有一批在途，确认后再发下一批；
 * 连续的鼠标移动只留最后一个，连续滚轮合并；按下与松开保持原顺序。
 */
export class DesktopInputQueue {
  private pending: DesktopInputEvent[] = []
  private sending = false
  private closed = false
  private readonly send: (events: DesktopInputEvent[]) => Promise<void>
  private readonly onError: (err: unknown, events: DesktopInputEvent[]) => void

  constructor(
    send: (events: DesktopInputEvent[]) => Promise<void>,
    onError: (err: unknown, events: DesktopInputEvent[]) => void,
  ) {
    this.send = send
    this.onError = onError
  }

  push(ev: DesktopInputEvent): void {
    if (this.closed) return
    const last = this.pending[this.pending.length - 1]
    if (ev.kind === 'move' && last?.kind === 'move') {
      this.pending[this.pending.length - 1] = ev
    } else if (ev.kind === 'wheel' && last?.kind === 'wheel') {
      this.pending[this.pending.length - 1] = {
        ...ev,
        deltaX: clampI16(last.deltaX + ev.deltaX),
        deltaY: clampI16(last.deltaY + ev.deltaY),
      }
    } else {
      this.pending.push(ev)
    }
    void this.pump()
  }

  /** 还有没发完或在途的输入。 */
  get busy(): boolean {
    return this.sending || this.pending.length > 0
  }

  close(): void {
    this.closed = true
    this.pending = []
  }

  private async pump(): Promise<void> {
    if (this.sending) return
    this.sending = true
    try {
      while (this.pending.length > 0 && !this.closed) {
        const batch = this.pending.splice(0, MAX_INPUT_BATCH)
        try {
          await this.send(batch)
        } catch (err) {
          if (!this.closed) this.onError(err, batch)
        }
      }
    } finally {
      this.sending = false
    }
  }
}

export const LONG_PRESS_MS = 500
/** 手指移动超过这么多屏幕像素才算拖动，否则算轻点。 */
export const TOUCH_SLOP_PX = 10
const DOUBLE_TAP_MS = 400

type Point = { x: number; y: number }
const dist = (a: Point, b: Point) => Math.hypot(a.x - b.x, a.y - b.y)

/**
 * 触摸手势 → 远端鼠标：轻点为左键单击，按住不动 0.5 秒为右键单击，
 * 移动超过阈值后按下左键拖动。两次轻点离得近时落在同一个远端像素，方便对方识别成双击。
 * 计时器由调用方管理：按下后 LONG_PRESS_MS 调 longPress()。
 */
export class TouchGesture {
  private state: 'idle' | 'pending' | 'drag' | 'consumed' = 'idle'
  private start: Point = { x: 0, y: 0 }
  private startClient: Point = { x: 0, y: 0 }
  private lastTap: { at: number; client: Point; remote: Point } | null = null

  get active(): boolean {
    return this.state !== 'idle'
  }

  down(remote: Point, client: Point, now: number): void {
    this.state = 'pending'
    this.startClient = client
    const t = this.lastTap
    this.start = t && now - t.at < DOUBLE_TAP_MS && dist(client, t.client) < TOUCH_SLOP_PX ? t.remote : remote
  }

  move(remote: Point, client: Point): DesktopInputEvent[] {
    if (this.state === 'pending') {
      if (dist(client, this.startClient) < TOUCH_SLOP_PX) return []
      this.state = 'drag'
      this.lastTap = null
      return [
        { kind: 'move', ...this.start },
        { kind: 'button', ...this.start, button: 'left', down: true },
        { kind: 'move', ...remote },
      ]
    }
    return this.state === 'drag' ? [{ kind: 'move', ...remote }] : []
  }

  up(remote: Point, now: number): DesktopInputEvent[] {
    const state = this.state
    this.state = 'idle'
    if (state === 'pending') {
      this.lastTap = { at: now, client: this.startClient, remote: this.start }
      return [
        { kind: 'move', ...this.start },
        { kind: 'button', ...this.start, button: 'left', down: true },
        { kind: 'button', ...this.start, button: 'left', down: false },
      ]
    }
    if (state === 'drag') return [{ kind: 'button', ...remote, button: 'left', down: false }]
    return []
  }

  longPress(): DesktopInputEvent[] {
    if (this.state !== 'pending') return []
    this.state = 'consumed'
    this.lastTap = null
    return [
      { kind: 'move', ...this.start },
      { kind: 'button', ...this.start, button: 'right', down: true },
      { kind: 'button', ...this.start, button: 'right', down: false },
    ]
  }

  /** 系统打断了触摸（例如来电、手势被接管）。返回是否正按着左键，需要让对方松开。 */
  cancel(): boolean {
    const dragging = this.state === 'drag'
    this.state = 'idle'
    return dragging
  }
}
