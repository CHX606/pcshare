import { useCallback, useEffect, useRef, useState } from 'react'
import { backend } from '../../api'
import { humanError } from '../../api/errors'
import {
  SELF_ID,
  type CapturedShot,
  type DesktopState,
  type DeviceId,
  type ShotTargets,
} from '../../api/types'
import { Icon } from '../../components/Icon'
import { Button } from '../../components/Modal'
import { Toolbar } from '../../components/Toolbar'
import { formatSize } from '../../lib/format'
import { mapClientPointToRemoteScreen } from '../../lib/stage3'
import { toast } from '../../store/toasts'

type ClickMode = 'left' | 'right' | 'double'
type ViewMode = 'fit' | 'actual'

const DESKTOP_PLACE_TEXT: Record<DesktopState['place'], string> = {
  normal: '正常桌面',
  locked: '锁屏或权限确认界面',
  service: '系统服务环境（看不到桌面）',
  remote: '远程桌面会话',
}

/** 选中的来源还在就保留，否则退回第一块显示器或第一个窗口。 */
function pickSourceChoice(t: ShotTargets, current: string | null): string {
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

/** 截本机时先等界面重画到屏幕上，否则截到的是切换前的旧画面。 */
function waitForRepaint(ms: number): Promise<void> {
  return new Promise((resolve) => {
    requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(resolve, ms)))
  })
}

function formatClock(ms: number): string {
  const d = new Date(ms)
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
}

const QUICK_KEYS = [
  { label: 'Enter', combo: 'Enter' },
  { label: 'Esc', combo: 'Esc' },
  { label: 'Tab', combo: 'Tab' },
  { label: 'Backspace', combo: 'Backspace' },
  { label: 'Ctrl+C', combo: 'Ctrl+C' },
  { label: 'Ctrl+V', combo: 'Ctrl+V' },
  { label: 'Alt+F4', combo: 'Alt+F4' },
  { label: 'Win+D', combo: 'Win+D' },
]

/** 任务 3.2 & 3.3：看一眼（显示器/窗口截图，JPEG 缩放省流量）+ 在截图上点按与键盘输入（含管理员窗口 UIPI 拦截提示）。 */
export function ScreenPanel({ device, canInput }: { device: DeviceId; canInput: boolean }) {
  const [targets, setTargets] = useState<ShotTargets | null>(null)
  const [sourceChoice, setSourceChoice] = useState<string>('mon:0')
  const [maxWidth, setMaxWidth] = useState<number>(1280)
  const [clickMode, setClickMode] = useState<ClickMode>('left')
  const [shot, setShot] = useState<CapturedShot | null>(null)
  const [busy, setBusy] = useState(false)
  const [warning, setWarning] = useState<string | null>(null)
  const [errorMsg, setErrorMsg] = useState<string | null>(null)
  const [textInput, setTextInput] = useState('')
  const [viewMode, setViewMode] = useState<ViewMode>('fit')
  const [autoRefreshSec, setAutoRefreshSec] = useState<number>(0)
  const imgRef = useRef<HTMLImageElement>(null)
  /** 防止手动截图、点按刷新和自动刷新同时向对方要图。 */
  const inflightRef = useRef(false)

  const parsedSource = useCallback((choice: string): { monitorIndex?: number; hwnd?: string } => {
    if (choice.startsWith('win:')) return { hwnd: choice.slice(4) }
    const idx = Number(choice.slice(4))
    return { monitorIndex: Number.isFinite(idx) ? idx : 0 }
  }, [])

  const captureNow = useCallback(
    async (choiceOverride?: string, widthOverride?: number, quiet = false) => {
      if (inflightRef.current) return
      inflightRef.current = true
      const src = parsedSource(choiceOverride ?? sourceChoice)
      const mw = widthOverride ?? maxWidth
      // 自动刷新时不改 busy，免得输入框每隔几秒被禁用一次、丢掉焦点。
      if (!quiet) {
        setBusy(true)
        setErrorMsg(null)
      }
      try {
        if (device === SELF_ID) await waitForRepaint(quiet ? 0 : 200)
        const res = await backend.shotCapture({
          device,
          monitorIndex: src.monitorIndex,
          hwnd: src.hwnd,
          maxWidth: mw > 0 ? mw : undefined,
          quality: 80,
        })
        setShot(res)
        if (quiet) setErrorMsg(null)
      } catch (err) {
        setErrorMsg(humanError(err))
      } finally {
        inflightRef.current = false
        if (!quiet) setBusy(false)
      }
    },
    [device, maxWidth, parsedSource, sourceChoice],
  )

  const loadTargetsAndShot = useCallback(async () => {
    setBusy(true)
    setErrorMsg(null)
    try {
      const t = await backend.shotMonitors(device)
      setTargets(t)
      const choice = pickSourceChoice(t, sourceChoice)
      setSourceChoice(choice)
      setBusy(false)
      await captureNow(choice)
    } catch (err) {
      setErrorMsg(humanError(err))
    } finally {
      setBusy(false)
    }
  }, [captureNow, device, sourceChoice])

  useEffect(() => {
    let cancelled = false
    void backend
      .shotMonitors(device)
      .then(async (t) => {
        if (cancelled) return
        setTargets(t)
        const defaultChoice = pickSourceChoice(t, null)
        setSourceChoice(defaultChoice)
        const src = parsedSource(defaultChoice)
        if (device === SELF_ID) await waitForRepaint(200)
        if (cancelled) return
        const res = await backend.shotCapture({
          device,
          monitorIndex: src.monitorIndex,
          hwnd: src.hwnd,
          maxWidth: 1280,
          quality: 80,
        })
        if (!cancelled) {
          setShot(res)
          setErrorMsg(null)
        }
      })
      .catch((err) => {
        if (!cancelled) setErrorMsg(humanError(err))
      })
    return () => {
      cancelled = true
    }
  }, [device, parsedSource])

  // 自动刷新：上一张到手后再等一个间隔；窗口不在前台时暂停。
  const captureRef = useRef(captureNow)
  useEffect(() => {
    captureRef.current = captureNow
  }, [captureNow])
  useEffect(() => {
    if (autoRefreshSec <= 0) return
    let stopped = false
    let timer: ReturnType<typeof setTimeout> | undefined
    const tick = async () => {
      if (stopped) return
      if (!document.hidden) await captureRef.current(undefined, undefined, true)
      if (!stopped) timer = setTimeout(() => void tick(), autoRefreshSec * 1000)
    }
    timer = setTimeout(() => void tick(), autoRefreshSec * 1000)
    return () => {
      stopped = true
      clearTimeout(timer)
    }
  }, [autoRefreshSec, device])

  const onImageClick = async (e: React.MouseEvent<HTMLImageElement>) => {
    if (!shot || device === SELF_ID || !canInput || busy) return
    const imgEl = imgRef.current
    if (!imgEl) return
    const rect = imgEl.getBoundingClientRect()
    const { screenX, screenY } = mapClientPointToRemoteScreen(
      e.clientX,
      e.clientY,
      rect,
      shot.transform,
    )
    const src = parsedSource(sourceChoice)
    setBusy(true)
    setWarning(null)
    try {
      const res = await backend.inputClick({
        device,
        screenX,
        screenY,
        button: clickMode === 'right' ? 'right' : 'left',
        double: clickMode === 'double',
        targetHwnd: shot.transform.targetHwnd,
        refreshMonitor: src.monitorIndex,
        refreshHwnd: src.hwnd,
        maxWidth: maxWidth > 0 ? maxWidth : undefined,
      })
      if (res.uipiBlocked && res.warning) {
        setWarning(res.warning)
      } else if (res.refreshedShot) {
        setShot(res.refreshedShot)
      } else {
        await captureNow()
      }
    } catch (err) {
      toast.error(humanError(err))
    } finally {
      setBusy(false)
    }
  }

  const sendKeyOrText = async (opts: { text?: string; keyCombo?: string }) => {
    if (device === SELF_ID || !canInput || busy) return
    const src = parsedSource(sourceChoice)
    setBusy(true)
    setWarning(null)
    try {
      const res = await backend.inputKey({
        device,
        text: opts.text,
        keyCombo: opts.keyCombo,
        targetHwnd: shot?.transform.targetHwnd,
        refreshMonitor: src.monitorIndex,
        refreshHwnd: src.hwnd,
        maxWidth: maxWidth > 0 ? maxWidth : undefined,
      })
      if (res.uipiBlocked && res.warning) {
        setWarning(res.warning)
      } else if (res.refreshedShot) {
        setShot(res.refreshedShot)
      } else {
        await captureNow()
      }
      if (opts.text) setTextInput('')
    } catch (err) {
      toast.error(humanError(err))
    } finally {
      setBusy(false)
    }
  }

  const interactiveRemote = device !== SELF_ID && canInput

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <Toolbar label="截图与点按控制栏" className="rounded-lg border border-line bg-bg/60 p-2">
        <div role="group" aria-label="画面来源与清晰度" className="flex flex-wrap items-center gap-2">
          <select
            aria-label="选择要查看的显示器或窗口"
            value={sourceChoice}
            onChange={(e) => {
              const v = e.target.value
              setSourceChoice(v)
              void captureNow(v)
            }}
            className="h-8 max-w-64 rounded-md border border-line bg-panel px-2 text-xs outline-none focus:border-accent"
          >
            {targets?.monitors.map((m) => (
              <option key={`mon:${m.index}`} value={`mon:${m.index}`}>
                {m.name}（缩放 {m.dpiScalePercent}%）
              </option>
            ))}
            {targets?.windows.map((w) => (
              <option key={`win:${w.hwnd}`} value={`win:${w.hwnd}`}>
                窗口：{w.title}（{w.width}×{w.height}）
              </option>
            ))}
          </select>

          <select
            aria-label="截图最大宽度"
            value={maxWidth}
            onChange={(e) => {
              const nw = Number(e.target.value)
              setMaxWidth(nw)
              void captureNow(undefined, nw)
            }}
            className="h-8 rounded-md border border-line bg-panel px-2 text-xs outline-none focus:border-accent"
          >
            <option value={1280}>省流缩放（宽 1280px）</option>
            <option value={1920}>高清（宽 1920px）</option>
            <option value={0}>原始分辨率</option>
          </select>

          <Button onClick={() => void loadTargetsAndShot()} disabled={busy}>
            <Icon name="refresh" size={14} className={busy ? 'spin' : ''} />
            刷新截图
          </Button>

          <select
            aria-label="自动刷新"
            value={autoRefreshSec}
            onChange={(e) => setAutoRefreshSec(Number(e.target.value))}
            className="h-8 rounded-md border border-line bg-panel px-2 text-xs outline-none focus:border-accent"
          >
            <option value={0}>不自动刷新</option>
            <option value={2}>每 2 秒刷新</option>
            <option value={5}>每 5 秒刷新</option>
          </select>

          <button
            type="button"
            aria-pressed={viewMode === 'actual'}
            onClick={() => setViewMode((m) => (m === 'fit' ? 'actual' : 'fit'))}
            title="原始大小时可以滚动查看，字更清楚"
            className="h-8 rounded-md border border-line bg-panel px-2.5 text-xs hover:bg-hover aria-pressed:border-accent aria-pressed:text-accent"
          >
            {viewMode === 'fit' ? '按原始大小看' : '缩到窗口内'}
          </button>
        </div>

        {device !== SELF_ID && (
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

      {warning && (
        <div role="alert" className="flex items-start gap-2.5 rounded-lg border border-warn/50 bg-warn/10 px-3.5 py-2.5 text-xs leading-relaxed">
          <Icon name="alert" size={16} className="mt-0.5 shrink-0 text-warn" />
          <div className="flex-1">{warning}</div>
          <button
            type="button"
            aria-label="关闭权限隔离提示"
            onClick={() => setWarning(null)}
            className="rounded p-0.5 text-muted hover:text-fg"
          >
            <Icon name="x" size={14} />
          </button>
        </div>
      )}

      {device !== SELF_ID && (
        <form
          onSubmit={(e) => {
            e.preventDefault()
            if (textInput) void sendKeyOrText({ text: textInput })
          }}
          className="flex flex-wrap items-center gap-2 rounded-lg border border-line bg-bg/40 px-3 py-2 text-xs"
        >
          <input
            value={textInput}
            disabled={!canInput || busy}
            onChange={(e) => setTextInput(e.target.value)}
            placeholder={
              canInput
                ? '输入要发往对方当前焦点窗口的文字…'
                : '对方尚未开启“点按”权限，当前为只读看屏模式'
            }
            aria-label="向对方当前窗口输入文字"
            className="h-8 min-w-48 flex-1 rounded-md border border-line bg-panel px-2.5 text-xs outline-none focus:border-accent disabled:opacity-50"
          />
          <Button type="submit" variant="primary" disabled={!canInput || busy || !textInput}>
            发送文字
          </Button>
          <div className="flex flex-wrap items-center gap-1">
            {QUICK_KEYS.map((k) => (
              <button
                key={k.combo}
                type="button"
                disabled={!canInput || busy}
                onClick={() => void sendKeyOrText({ keyCombo: k.combo })}
                className="h-7 rounded border border-line bg-panel px-2 font-mono text-[11px] hover:bg-hover disabled:opacity-40"
              >
                {k.label}
              </button>
            ))}
          </div>
        </form>
      )}

      <div
        className={`scroll-thin flex min-h-0 flex-1 flex-col overflow-auto rounded-lg border border-line bg-bg p-3 ${
          viewMode === 'fit' ? 'items-center justify-center' : 'items-start justify-start'
        }`}
      >
        {errorMsg ? (
          <div className="max-w-md text-center">
            <Icon name="alert" size={28} className="mx-auto mb-2 text-warn" />
            <p className="mb-3 text-sm leading-relaxed">{errorMsg}</p>
            <p className="mb-4 text-xs text-muted">
              若尚未授权，请在对方电脑的设备设置中勾选“截图”权限；若对方处于锁屏或权限确认界面，解锁后即可查看。
            </p>
            <Button variant="primary" onClick={() => void loadTargetsAndShot()}>
              重新获取
            </Button>
          </div>
        ) : shot ? (
          <div
            className={`flex flex-col gap-2 ${
              viewMode === 'fit' ? 'max-h-full max-w-full items-center' : 'items-start'
            }`}
          >
            <img
              ref={imgRef}
              src={shot.imageDataUrl}
              alt="对方桌面或窗口的截图"
              width={shot.transform.imageWidth}
              height={shot.transform.imageHeight}
              onClick={(e) => void onImageClick(e)}
              onContextMenu={(e) => {
                if (!interactiveRemote) return
                e.preventDefault()
                const prev = clickMode
                setClickMode('right')
                void onImageClick(e).finally(() => setClickMode(prev))
              }}
              // h-auto w-auto 让浏览器按原比例缩放；只写最大宽高会被横竖分别压扁。
              className={`rounded border border-line shadow-sm ${
                viewMode === 'fit'
                  ? 'h-auto max-h-[calc(100vh-300px)] w-auto max-w-full'
                  : 'h-auto w-auto max-w-none'
              } ${
                interactiveRemote ? 'cursor-crosshair' : ''
              }`}
            />
            <div className="flex flex-wrap items-center gap-3 text-xs text-muted">
              <span>
                原图 {shot.transform.sourceWidth}×{shot.transform.sourceHeight} → 传输{' '}
                {shot.transform.imageWidth}×{shot.transform.imageHeight}（JPEG{' '}
                {formatSize(shot.byteLen)}）
              </span>
              <span>截于 {formatClock(shot.capturedAtMs)}</span>
              <span>桌面：{DESKTOP_PLACE_TEXT[shot.desktop.place] ?? shot.desktop.place}</span>
              <span>被控程序：{shot.desktop.elevated ? '管理员权限' : '普通权限'}</span>
              {interactiveRemote && (
                <span className="text-accent">直接点击上方画面即可在对方对应屏幕位置触发操作并自动刷新</span>
              )}
            </div>
          </div>
        ) : (
          <span className="flex items-center gap-2 text-sm text-muted">
            <Icon name="loader" className="spin text-accent" /> 正在截取画面…
          </span>
        )}
      </div>
    </div>
  )
}
