import { isTauri } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from 'react'
import { backend } from '../../api'
import { errorCode, humanError } from '../../api/errors'
import type {
  DesktopFrameMeta,
  DesktopInputEvent,
  DesktopMouseButton,
  DeviceId,
  ShotTargets,
  ShotTransform,
} from '../../api/types'
import { Icon } from '../../components/Icon'
import { Button } from '../../components/Modal'
import { Toolbar } from '../../components/Toolbar'
import { formatSize } from '../../lib/format'
import {
  DesktopInputQueue,
  LONG_PRESS_MS,
  TouchGesture,
  canDecodeVideo,
  describeFit,
  localScreenPixels,
  mapPointToRemotePixel,
  parseSourceChoice,
  pickSourceChoice,
  vkFromCode,
  wheelToWindows,
} from '../../lib/desktop'
import { RemoteInputBar, SourceSelect } from './RemoteControlBars'

type ClickMode = 'left' | 'right' | 'double'
type ViewMode = 'fit' | 'actual'
type Phase = { kind: 'opening' } | { kind: 'running' } | { kind: 'error'; msg: string }

interface Stats {
  meta: DesktopFrameMeta
  bytes: number
  fps: number
}

/** 独立统计状态容器：每 500ms 更新帧率与码率时只触发底部小状态行重渲，避免整页重绘。 */
class StatsChannel {
  private value: Stats | null = null
  private listeners = new Set<() => void>()

  get = () => this.value

  set(next: Stats | null) {
    this.value = next
    for (const fn of this.listeners) fn()
  }

  subscribe = (fn: () => void) => {
    this.listeners.add(fn)
    return () => {
      this.listeners.delete(fn)
    }
  }
}

function FullscreenStatsBadge({ channel }: { channel: StatsChannel }) {
  const stats = useSyncExternalStore(channel.subscribe, channel.get)
  if (!stats) return null
  return (
    <span className={`text-xs ${stats.meta.mode === 'live' ? 'text-ok' : 'text-warn'}`}>
      {stats.meta.mode === 'live' ? `实时 ${stats.fps} 帧/秒` : '逐步模式'}
    </span>
  )
}

function DesktopStatsFooter({
  channel,
  interactive,
  rawInput,
}: {
  channel: StatsChannel
  interactive: boolean
  rawInput: boolean
}) {
  const stats = useSyncExternalStore(channel.subscribe, channel.get)
  if (!stats) return null
  const meta = stats.meta
  const t = meta.transform
  return (
    <div className="flex flex-wrap items-center gap-3 text-xs text-muted">
      {meta.mode === 'live' ? (
        <span className="text-ok">
          实时画面 · {stats.fps} 帧/秒 · 编码器 {meta.encoder}
        </span>
      ) : (
        <span className="text-warn">逐步模式{meta.reason ? `：${meta.reason}` : ''}</span>
      )}
      <span>
        原图 {t.sourceWidth}×{t.sourceHeight} → 传输 {t.imageWidth}×{t.imageHeight}（
        {meta.format === 'jpeg' ? 'JPEG' : 'H.264'} {formatSize(stats.bytes)}）
      </span>
      {interactive && (
        <span className="text-accent">
          {rawInput
            ? '点一下画面后，鼠标、滚轮和键盘都会直接发往对方；触屏轻点是单击、长按是右键、按住移动是拖动；中文请用上方文字框'
            : '对方程序较旧，只能单击、双击、发文字和组合键'}
        </span>
      )}
    </div>
  )
}

const BUTTONS: DesktopMouseButton[] = ['left', 'middle', 'right']

/** “对方分辨率跟随本机”开关，记在本机，默认打开。 */
const FOLLOW_KEY = 'pcshare.desktop.followResolution'
function loadFollow(): boolean {
  try {
    return localStorage.getItem(FOLLOW_KEY) !== '0'
  } catch {
    return true
  }
}

/** 解码配置是否可用，按编码字符串缓存。 */
const codecSupport = new Map<string, Promise<boolean>>()
function codecSupported(codec: string): Promise<boolean> {
  let p = codecSupport.get(codec)
  if (!p) {
    p = VideoDecoder.isConfigSupported({ codec, optimizeForLatency: true })
      .then((r) => !!r.supported)
      .catch(() => false)
    codecSupport.set(codec, p)
  }
  return p
}

/**
 * 实时桌面：打开会话后循环取帧。实时帧交给 WebCodecs 解码，逐步帧直接画 JPEG。
 * 对端支持原始键鼠时把鼠标、滚轮、键盘直接转发；旧版对端只能单击、双击、文字和组合键。
 * 页面隐藏、切换来源或离开时关闭会话，回来后重新打开。
 */
export function DesktopView({
  device,
  canInput,
  onFallback,
}: {
  device: DeviceId
  canInput: boolean
  /** 当前界面解不了视频时退回截图模式。 */
  onFallback: (reason: string) => void
}) {
  const [targets, setTargets] = useState<ShotTargets | null>(null)
  const [sourceChoice, setSourceChoice] = useState<string | null>(null)
  const [phase, setPhase] = useState<Phase>({ kind: 'opening' })
  const [hasFrame, setHasFrame] = useState(false)
  const [rawInput, setRawInput] = useState(false)
  const [warning, setWarning] = useState<string | null>(null)
  const [clickMode, setClickMode] = useState<ClickMode>('left')
  const [viewMode, setViewMode] = useState<ViewMode>('fit')
  const [textInput, setTextInput] = useState('')
  const [generation, setGeneration] = useState(0)
  const [visible, setVisible] = useState(() => !document.hidden)
  const [fullscreen, setFullscreen] = useState(false)
  const [barOpen, setBarOpen] = useState(true)
  /** 当前画面的像素宽高。对方传来的图最宽 1280，全屏时要据此放大铺满屏幕。 */
  const [frameSize, setFrameSize] = useState<{ w: number; h: number } | null>(null)
  /** 看整块显示器时让对方临时切到本机分辨率，关闭看屏后对方自动改回。 */
  const [followRes, setFollowRes] = useState(loadFollow)
  const [fitStatus, setFitStatus] = useState<{ text: string; warn: boolean } | null>(null)

  const [statsChannel] = useState(() => new StatsChannel())
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const sessionRef = useRef<number | null>(null)
  const queueRef = useRef<DesktopInputQueue | null>(null)
  const transformRef = useRef<ShotTransform | null>(null)
  /** 本机按住还没松开的键和鼠标按钮，失焦时据此决定要不要发 release。 */
  const heldRef = useRef(new Set<string>())
  const gestureRef = useRef(new TouchGesture())
  const pressTimerRef = useRef<number | null>(null)
  const onFallbackRef = useRef(onFallback)
  useEffect(() => {
    onFallbackRef.current = onFallback
  }, [onFallback])

  const loadTargets = useCallback(
    (isCancelled: () => boolean = () => false) =>
      backend.shotMonitors(device).then(
        (t) => {
          if (isCancelled()) return
          setTargets(t)
          setSourceChoice((cur) => pickSourceChoice(t, cur))
        },
        (err) => {
          if (isCancelled()) return
          // 拿不到列表也先看主显示器，真正的错误由打开会话时报告
          setSourceChoice((cur) => cur ?? 'mon:0')
          if (errorCode(err) !== 'forbidden') setWarning(humanError(err))
        },
      ),
    [device],
  )

  useEffect(() => {
    let cancelled = false
    void loadTargets(() => cancelled)
    return () => {
      cancelled = true
    }
  }, [loadTargets])

  useEffect(() => {
    const onVis = () => setVisible(!document.hidden)
    document.addEventListener('visibilitychange', onVis)
    return () => document.removeEventListener('visibilitychange', onVis)
  }, [])

  // 会话主循环
  useEffect(() => {
    if (!sourceChoice || !visible) return
    let stopped = false
    let sessionId: number | null = null
    let decoder: VideoDecoder | null = null
    let decoderEpoch = -1
    let codec: string | null = null
    let paintedFirstFrame = false
    const transforms = new Map<number, ShotTransform>()
    const drawnAt: number[] = []
    let latest: { meta: DesktopFrameMeta; bytes: number } | null = null
    const held = heldRef.current

    const pruneFpsWindow = (now: number) => {
      while (drawnAt.length > 0 && drawnAt[0] < now - 1000) drawnAt.shift()
    }

    const draw = (img: CanvasImageSource, w: number, h: number, t: ShotTransform) => {
      const c = canvasRef.current
      if (!c || stopped) return
      if (c.width !== w || c.height !== h) {
        c.width = w
        c.height = h
        setFrameSize({ w, h })
      }
      c.getContext('2d')?.drawImage(img, 0, 0, w, h)
      transformRef.current = t
      const now = performance.now()
      drawnAt.push(now)
      pruneFpsWindow(now)
      if (!paintedFirstFrame) {
        paintedFirstFrame = true
        setHasFrame(true)
        if (latest) statsChannel.set({ ...latest, fps: drawnAt.length })
      }
    }

    const fallback = (reason: string) => {
      if (stopped) return
      stopped = true
      onFallbackRef.current(reason)
    }

    const ensureDecoder = async (meta: DesktopFrameMeta): Promise<VideoDecoder | null> => {
      if (decoder && decoder.state === 'configured' && meta.epoch === decoderEpoch) return decoder
      // 新的编码周期从关键帧开始，之前的增量帧丢掉
      if (!meta.key) return null
      const c = meta.codec ?? codec
      if (!c) return null
      if (!canDecodeVideo() || !(await codecSupported(c))) {
        fallback(`这台电脑的界面无法解码对方的视频（${c}），已改用截图模式`)
        return null
      }
      if (stopped) return null
      if (!decoder || decoder.state === 'closed') {
        decoder = new VideoDecoder({
          output: (frame) => {
            const t = transforms.get(frame.timestamp) ?? transformRef.current
            transforms.delete(frame.timestamp)
            if (t) draw(frame, frame.displayWidth, frame.displayHeight, t)
            frame.close()
          },
          error: () => {
            // 解码出错：等下一个关键帧重新配置
            decoderEpoch = -1
          },
        })
      } else {
        decoder.reset()
      }
      decoder.configure({ codec: c, optimizeForLatency: true })
      decoderEpoch = meta.epoch
      codec = c
      transforms.clear()
      return decoder
    }

    const statsTimer = window.setInterval(() => {
      if (!latest) return
      pruneFpsWindow(performance.now())
      statsChannel.set({ ...latest, fps: drawnAt.length })
    }, 500)

    const run = async () => {
      setPhase({ kind: 'opening' })
      setHasFrame(false)
      statsChannel.set(null)
      setWarning(null)
      setFitStatus(null)
      try {
        const source = parseSourceChoice(sourceChoice)
        // 只有看整块显示器才调整分辨率；看单个窗口时没有意义
        const fit =
          followRes && source.monitorIndex != null
            ? localScreenPixels(window.screen.width, window.screen.height, window.devicePixelRatio)
            : null
        const opened = await backend.desktopOpen({
          device,
          ...source,
          ...(fit ? { fitWidth: fit.width, fitHeight: fit.height } : {}),
        })
        if (stopped) {
          void backend.desktopClose(opened.sessionId).catch(() => undefined)
          return
        }
        const id = opened.sessionId
        sessionId = id
        sessionRef.current = id
        setRawInput(opened.rawInput)
        setFitStatus(describeFit(opened.fit, opened.fitError))
        // 对方分辨率变了，来源列表里显示的尺寸也跟着刷新
        if (opened.fit?.applied) void loadTargets(() => stopped)
        queueRef.current = new DesktopInputQueue(
          (events) => backend.desktopInput(id, events),
          (err) => {
            if (errorCode(err) !== 'not_found') setWarning(humanError(err))
          },
        )
        setPhase({ kind: 'running' })

        while (!stopped) {
          const frame = await backend.desktopNext(id)
          if (stopped) break
          latest = { meta: frame.meta, bytes: frame.data.byteLength }
          if (frame.meta.format === 'jpeg') {
            const bmp = await createImageBitmap(new Blob([frame.data], { type: 'image/jpeg' }))
            draw(bmp, bmp.width, bmp.height, frame.meta.transform)
            bmp.close()
            if (!transformRef.current) transformRef.current = frame.meta.transform
            continue
          }
          const dec = await ensureDecoder(frame.meta)
          if (!dec || stopped) continue
          transforms.set(frame.meta.timestampUs, frame.meta.transform)
          if (transforms.size > 32) transforms.delete(transforms.keys().next().value as number)
          dec.decode(
            new EncodedVideoChunk({
              type: frame.meta.key ? 'key' : 'delta',
              timestamp: frame.meta.timestampUs,
              data: frame.data,
            }),
          )
          // 解码跟不上时先别取下一帧，让后端据此降档
          while (!stopped && dec.state === 'configured' && dec.decodeQueueSize > 2) {
            await new Promise((r) => setTimeout(r, 4))
          }
        }
      } catch (err) {
        if (!stopped) setPhase({ kind: 'error', msg: humanError(err) })
      }
    }
    void run()

    return () => {
      stopped = true
      window.clearInterval(statsTimer)
      queueRef.current?.close()
      queueRef.current = null
      sessionRef.current = null
      if (decoder && decoder.state !== 'closed') decoder.close()
      if (sessionId != null) {
        const id = sessionId
        const needRelease = held.size > 0
        held.clear()
        const release = needRelease
          ? backend.desktopInput(id, [{ kind: 'release' }]).catch(() => undefined)
          : Promise.resolve()
        void release.then(() => backend.desktopClose(id).catch(() => undefined))
      }
    }
  }, [device, sourceChoice, visible, generation, statsChannel, followRes, loadTargets])

  const interactive = canInput && phase.kind === 'running'
  const pointerOnly = interactive && rawInput

  const push = (ev: DesktopInputEvent) => queueRef.current?.push(ev)

  const releaseAll = useCallback(() => {
    if (heldRef.current.size === 0) return
    heldRef.current.clear()
    queueRef.current?.push({ kind: 'release' })
  }, [])

  useEffect(() => {
    window.addEventListener('blur', releaseAll)
    return () => window.removeEventListener('blur', releaseAll)
  }, [releaseAll])

  const remotePoint = (e: { clientX: number; clientY: number }) => {
    const c = canvasRef.current
    const t = transformRef.current
    if (!c || !t) return null
    return mapPointToRemotePixel(e.clientX, e.clientY, c.getBoundingClientRect(), t)
  }

  // 滚轮要能阻止页面滚动，只能手动挂非被动监听
  useEffect(() => {
    const c = canvasRef.current
    if (!c || !pointerOnly) return
    const onWheel = (e: WheelEvent) => {
      const p = remotePoint(e)
      if (!p) return
      e.preventDefault()
      const d = wheelToWindows(e.deltaX, e.deltaY, e.deltaMode)
      if (d.deltaX === 0 && d.deltaY === 0) return
      queueRef.current?.push({ kind: 'wheel', ...p, ...d })
    }
    c.addEventListener('wheel', onWheel, { passive: false })
    return () => c.removeEventListener('wheel', onWheel)
  }, [pointerOnly])

  /** 发送输入并记下按住的鼠标按钮，失焦或触摸被打断时据此让对方松开。 */
  const send = (events: DesktopInputEvent[]) => {
    for (const ev of events) {
      if (ev.kind === 'button') {
        if (ev.down) heldRef.current.add(`b:${ev.button}`)
        else heldRef.current.delete(`b:${ev.button}`)
      }
      push(ev)
    }
  }

  const clearPressTimer = () => {
    if (pressTimerRef.current != null) {
      window.clearTimeout(pressTimerRef.current)
      pressTimerRef.current = null
    }
  }
  useEffect(
    () => () => {
      if (pressTimerRef.current != null) window.clearTimeout(pressTimerRef.current)
    },
    [],
  )

  const clientOf = (e: React.PointerEvent) => ({ x: e.clientX, y: e.clientY })

  /** 抓住指针，拖出画面也能收到松开；指针已经抬起时浏览器会抛错，忽略即可。 */
  const capture = (e: React.PointerEvent<HTMLCanvasElement>) => {
    e.currentTarget.focus()
    try {
      e.currentTarget.setPointerCapture(e.pointerId)
    } catch {
      // 指针已失效
    }
  }

  // 鼠标和笔直接转发按下、移动、松开；手指走手势：轻点左键、长按右键、移动后拖动
  const onPointerDown = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (!pointerOnly) return
    const p = remotePoint(e)
    if (!p) return
    if (e.pointerType === 'touch') {
      if (!e.isPrimary) return
      e.preventDefault()
      capture(e)
      gestureRef.current.down(p, clientOf(e), e.timeStamp)
      clearPressTimer()
      pressTimerRef.current = window.setTimeout(() => {
        pressTimerRef.current = null
        send(gestureRef.current.longPress())
      }, LONG_PRESS_MS)
      return
    }
    const button = BUTTONS[e.button]
    if (!button) return
    e.preventDefault()
    capture(e)
    send([{ kind: 'button', ...p, button, down: true }])
  }

  const onPointerMove = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (!pointerOnly) return
    const p = remotePoint(e)
    if (!p) return
    if (e.pointerType === 'touch') {
      if (!e.isPrimary) return
      const events = gestureRef.current.move(p, clientOf(e))
      if (events.length > 0) clearPressTimer()
      send(events)
      return
    }
    send([{ kind: 'move', ...p }])
  }

  const onPointerUp = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (!pointerOnly) return
    if (e.currentTarget.hasPointerCapture(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId)
    const p = remotePoint(e)
    if (!p) return
    if (e.pointerType === 'touch') {
      if (!e.isPrimary) return
      e.preventDefault()
      clearPressTimer()
      send(gestureRef.current.up(p, e.timeStamp))
      return
    }
    const button = BUTTONS[e.button]
    if (!button) return
    e.preventDefault()
    send([{ kind: 'button', ...p, button, down: false }])
  }

  /** 系统接管了指针（触摸被打断、拖出窗口等），让对方松开所有仍按着的键和按钮。 */
  const onPointerCancel = () => {
    if (!pointerOnly) return
    clearPressTimer()
    gestureRef.current.cancel()
    releaseAll()
  }

  const onKey = (e: React.KeyboardEvent<HTMLCanvasElement>, down: boolean) => {
    if (!pointerOnly) return
    // 输入法组字交给下方文字框，这里不转发
    if (e.nativeEvent.isComposing || e.key === 'Process') return
    // F11 留给本机切换全屏
    if (e.code === 'F11') return
    const vk = vkFromCode(e.code)
    if (vk == null) return
    e.preventDefault()
    // 使用原生事件阻止冒泡，避免触发 window 上的全局快捷键（Ctrl+1..5 / Ctrl+K）
    e.nativeEvent.stopPropagation()
    if (down) heldRef.current.add(`k:${vk}`)
    else heldRef.current.delete(`k:${vk}`)
    push({ kind: 'key', vk, down })
  }

  const onLegacyClick = (e: React.MouseEvent<HTMLCanvasElement>, button: DesktopMouseButton, double: boolean) => {
    if (!interactive || rawInput) return
    const p = remotePoint(e)
    if (p) push({ kind: 'click', ...p, button, double })
  }

  const sendTextOrCombo = (ev: DesktopInputEvent) => {
    if (!interactive) return
    setWarning(null)
    push(ev)
    if (ev.kind === 'text') setTextInput('')
  }

  const refresh = () => {
    const id = sessionRef.current
    if (id != null) void backend.desktopRefresh(id).catch((err) => setWarning(humanError(err)))
    void loadTargets()
  }

  // 全屏：画面铺满窗口；在桌面程序里同时让窗口占满整个屏幕，退出时还原
  useEffect(() => {
    if (!fullscreen || !isTauri()) return
    const win = getCurrentWindow()
    const entered = win
      .isFullscreen()
      .then(async (already) => {
        if (already) return false
        await win.setFullscreen(true)
        return true
      })
      .catch(() => false)
    return () => {
      void entered.then((did) => (did ? win.setFullscreen(false) : undefined)).catch(() => undefined)
    }
  }, [fullscreen])

  // F11 切换全屏；Esc 正在发往对方时不拦截，否则用来退出全屏
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.code === 'F11') {
        e.preventDefault()
        setFullscreen((f) => !f)
        return
      }
      if (e.key !== 'Escape' || !fullscreen) return
      if (pointerOnly && document.activeElement === canvasRef.current) return
      e.preventDefault()
      setFullscreen(false)
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [fullscreen, pointerOnly])

  const inputPlaceholder = canInput
    ? '输入中文等文字后发往对方当前焦点窗口…'
    : '对方尚未开启“点按”权限，当前只能看画面'

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <Toolbar label="实时桌面控制栏" className="rounded-lg border border-line bg-bg/60 p-2">
        <div role="group" aria-label="画面来源" className="flex flex-wrap items-center gap-2">
          <SourceSelect
            targets={targets}
            value={sourceChoice ?? ''}
            onChange={setSourceChoice}
            fallbackOption
          />

          <Button onClick={refresh} disabled={phase.kind !== 'running'} title="刷新窗口列表；逐步模式下立即取一张新画面">
            <Icon name="refresh" size={14} />
            刷新
          </Button>

          <button
            type="button"
            aria-pressed={viewMode === 'actual'}
            onClick={() => setViewMode((m) => (m === 'fit' ? 'actual' : 'fit'))}
            title="原始大小时可以滚动查看，字更清楚"
            className="h-8 rounded-md border border-line bg-panel px-2.5 text-xs hover:bg-hover aria-pressed:border-accent aria-pressed:text-accent"
          >
            {viewMode === 'fit' ? '按原始大小看' : '缩到窗口内'}
          </button>

          <button
            type="button"
            onClick={() => {
              setBarOpen(true)
              setFullscreen(true)
            }}
            title="画面铺满整个屏幕，小按钮更好点；按 F11 也可以切换"
            className="h-8 rounded-md border border-line bg-panel px-2.5 text-xs hover:bg-hover"
          >
            全屏看屏
          </button>

          <button
            type="button"
            aria-pressed={followRes}
            onClick={() => {
              const next = !followRes
              setFollowRes(next)
              try {
                localStorage.setItem(FOLLOW_KEY, next ? '1' : '0')
              } catch {
                // 存不下也不影响这次使用
              }
            }}
            title="看整块显示器时，把对方屏幕临时切到和本机一样的分辨率，画面正好铺满；关闭看屏后对方自动改回。只能用对方显示器支持的分辨率"
            className="h-8 rounded-md border border-line bg-panel px-2.5 text-xs hover:bg-hover aria-pressed:border-accent aria-pressed:text-accent"
          >
            对方分辨率跟随本机
          </button>
        </div>

        {!rawInput && phase.kind === 'running' && (
          <>
            <span role="separator" aria-orientation="vertical" className="mx-1 h-5 border-l border-line" />
            <div role="group" aria-label="点击方式" className="flex items-center gap-1">
              {(
                [
                  ['left', '左键单击'],
                  ['double', '左键双击'],
                  ['right', '右键单击'],
                ] as const
              ).map(([id, label]) => (
                <button
                  key={id}
                  type="button"
                  aria-pressed={clickMode === id}
                  disabled={!canInput}
                  onClick={() => setClickMode(id)}
                  className="h-8 rounded-md border border-line bg-panel px-2.5 text-xs hover:bg-hover disabled:opacity-40"
                >
                  {label}
                </button>
              ))}
            </div>
          </>
        )}
      </Toolbar>

      {fitStatus && (
        <p role="status" className={`px-1 text-xs ${fitStatus.warn ? 'text-warn' : 'text-muted'}`}>
          {fitStatus.text}
        </p>
      )}

      {warning && (
        <div role="alert" className="flex items-start gap-2.5 rounded-lg border border-warn/50 bg-warn/10 px-3.5 py-2.5 text-xs leading-relaxed">
          <Icon name="alert" size={16} className="mt-0.5 shrink-0 text-warn" />
          <div className="flex-1">{warning}</div>
          <button
            type="button"
            aria-label="关闭提示"
            onClick={() => setWarning(null)}
            className="rounded p-0.5 text-muted hover:text-fg"
          >
            <Icon name="x" size={14} />
          </button>
        </div>
      )}

      <RemoteInputBar
        text={textInput}
        onTextChange={setTextInput}
        disabled={!interactive}
        placeholder={inputPlaceholder}
        onSendText={(text) => sendTextOrCombo({ kind: 'text', text })}
        onSendShortcut={(combo) => sendTextOrCombo({ kind: 'shortcut', combo })}
        className="rounded-lg border border-line bg-bg/40 px-3 py-2"
      />

      <div
        className={`scroll-thin flex min-h-0 flex-col overflow-auto ${
          fullscreen ? 'fixed inset-0 z-50 bg-bg' : 'flex-1 rounded-lg border border-line bg-bg p-3'
        } ${viewMode === 'fit' ? 'items-center justify-center' : 'items-start justify-start'}`}
      >
        {fullscreen && (
          <div className="fixed top-2 left-1/2 z-10 flex max-w-[calc(100vw-1rem)] -translate-x-1/2 flex-col items-center">
            {barOpen && (
              <div className="flex flex-wrap items-center gap-2 rounded-t-lg border border-line bg-panel/95 p-2 shadow-lg">
                <RemoteInputBar
                  text={textInput}
                  onTextChange={setTextInput}
                  disabled={!interactive}
                  placeholder={inputPlaceholder}
                  onSendText={(text) => sendTextOrCombo({ kind: 'text', text })}
                  onSendShortcut={(combo) => sendTextOrCombo({ kind: 'shortcut', combo })}
                />
                <FullscreenStatsBadge channel={statsChannel} />
                <Button onClick={() => setFullscreen(false)} title="也可以按 F11；画面没有键盘焦点时按 Esc">
                  退出全屏
                </Button>
              </div>
            )}
            <button
              type="button"
              onClick={() => setBarOpen((o) => !o)}
              className="rounded-b-md border border-t-0 border-line bg-panel/90 px-3 py-0.5 text-[11px] text-muted hover:text-fg"
            >
              {barOpen ? '收起工具条' : '展开工具条'}
            </button>
          </div>
        )}
        {phase.kind === 'error' ? (
          <div className="max-w-md text-center">
            <Icon name="alert" size={28} className="mx-auto mb-2 text-warn" />
            <p className="mb-3 text-sm leading-relaxed">{phase.msg}</p>
            <p className="mb-4 text-xs text-muted">
              若尚未授权，请在对方电脑的设备设置中勾选“截图”权限；若对方处于锁屏或权限确认界面，解锁后即可查看。
            </p>
            <Button variant="primary" onClick={() => setGeneration((g) => g + 1)}>
              重新连接
            </Button>
          </div>
        ) : (
          <div
            className={`flex flex-col gap-2 ${viewMode === 'fit' ? 'max-h-full max-w-full items-center' : 'items-start'}`}
          >
            {!hasFrame && (
              <span className="flex items-center gap-2 text-sm text-muted">
                <Icon name="loader" className="spin text-accent" /> 正在打开对方桌面…
              </span>
            )}
            <canvas
              ref={canvasRef}
              hidden={!hasFrame}
              tabIndex={pointerOnly ? 0 : -1}
              data-capture-keys={pointerOnly ? 'true' : undefined}
              aria-label="对方桌面画面"
              onPointerMove={onPointerMove}
              onPointerDown={onPointerDown}
              onPointerUp={onPointerUp}
              onPointerCancel={onPointerCancel}
              onKeyDown={(e) => onKey(e, true)}
              onKeyUp={(e) => onKey(e, false)}
              onBlur={releaseAll}
              onClick={(e) => onLegacyClick(e, clickMode === 'right' ? 'right' : 'left', clickMode === 'double')}
              onContextMenu={(e) => {
                if (!interactive) return
                e.preventDefault()
                onLegacyClick(e, 'right', false)
              }}
              // h-auto w-auto 让画面按原比例缩放；只写最大宽高会被横竖分别压扁。
              // 全屏时画面可能比屏幕小（对方传来的图最宽 1280），按比例放大到贴住屏幕的宽或高；
              // 画布框和画面大小一致，点按坐标按画布框换算仍然准确，所以不用 object-fit。
              style={
                fullscreen && viewMode === 'fit' && frameSize
                  ? { width: `min(100vw, calc(100vh * ${frameSize.w} / ${frameSize.h}))` }
                  : undefined
              }
              className={`outline-none ${fullscreen ? '' : 'rounded border border-line shadow-sm focus:border-accent'} ${
                viewMode !== 'fit'
                  ? 'h-auto w-auto max-w-none'
                  : fullscreen
                    ? 'h-auto max-w-none'
                    : 'h-auto max-h-[calc(100vh-300px)] w-auto max-w-full'
              } ${interactive ? 'cursor-crosshair' : ''} ${pointerOnly ? 'touch-none select-none' : ''}`}
            />
            {!fullscreen && (
              <DesktopStatsFooter channel={statsChannel} interactive={interactive} rawInput={rawInput} />
            )}
          </div>
        )}
      </div>
    </div>
  )
}
