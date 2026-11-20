import { Suspense, lazy, useCallback, useEffect, useRef, useState } from 'react'
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
import { canDecodeVideo, parseSourceChoice, pickSourceChoice } from '../../lib/desktop'
import { formatSize } from '../../lib/format'
import { mapClientPointToRemoteScreen } from '../../lib/stage3'
import { toast } from '../../store/toasts'
import { RemoteInputBar, SourceSelect } from './RemoteControlBars'

const DesktopView = lazy(async () => {
  const m = await import('./DesktopView')
  return { default: m.DesktopView }
})

type ClickMode = 'left' | 'right' | 'double'
type ViewMode = 'fit' | 'actual'

const DESKTOP_PLACE_TEXT: Record<DesktopState['place'], string> = {
  normal: '正常桌面',
  locked: '锁屏或权限确认界面',
  service: '系统服务环境（看不到桌面）',
  remote: '远程桌面会话',
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

type PanelMode = 'live' | 'shot'

/**
 * 看屏入口：远程设备默认用实时桌面会话（后端对旧版对端自动走逐步截图）；
 * 界面解不了视频或用户手动切换时用截图模式；查看本机始终用截图。
 */
export function ScreenPanel({ device, canInput }: { device: DeviceId; canInput: boolean }) {
  const remote = device !== SELF_ID
  const [mode, setMode] = useState<PanelMode>(() => (canDecodeVideo() ? 'live' : 'shot'))
  const [fallbackNote, setFallbackNote] = useState<string | null>(null)

  if (!remote) return <ShotView device={device} canInput={false} />

  const switchTo = (next: PanelMode) => {
    setFallbackNote(null)
    setMode(next)
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <div role="radiogroup" aria-label="看屏方式" className="flex items-center gap-1">
          {(
            [
              ['live', '实时画面'],
              ['shot', '截图模式'],
            ] as const
          ).map(([id, label]) => (
            <button
              key={id}
              type="button"
              role="radio"
              aria-checked={mode === id}
              disabled={id === 'live' && !canDecodeVideo()}
              onClick={() => switchTo(id)}
              className="h-7 rounded-md border border-line bg-panel px-2.5 hover:bg-hover disabled:opacity-40 aria-checked:border-accent aria-checked:text-accent"
            >
              {label}
            </button>
          ))}
        </div>
        <span className="text-muted">
          {mode === 'live'
            ? '流畅的视频画面，可直接用鼠标键盘操作；网络差时自动降为逐张截图'
            : '按需截一张图，省流量；点击画面即可点按'}
        </span>
      </div>
      {fallbackNote && (
        <div role="status" className="rounded-lg border border-line bg-bg/40 px-3 py-2 text-xs text-muted">
          {fallbackNote}
        </div>
      )}
      {mode === 'live' ? (
        <Suspense
          fallback={
            <div className="grid flex-1 place-items-center text-sm text-muted">
              正在加载实时桌面组件…
            </div>
          }
        >
          <DesktopView
            key={device}
            device={device}
            canInput={canInput}
            onFallback={(reason) => {
              setFallbackNote(reason)
              setMode('shot')
            }}
          />
        </Suspense>
      ) : (
        <ShotView key={device} device={device} canInput={canInput} />
      )}
    </div>
  )
}

/** 任务 3.2 & 3.3：看一眼（显示器/窗口截图，JPEG 缩放省流量）+ 在截图上点按与键盘输入（含管理员窗口 UIPI 拦截提示）。 */
function ShotView({ device, canInput }: { device: DeviceId; canInput: boolean }) {
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
  /** 统一隔离首屏抓图、手动刷新、点按操作与自动刷新，防止旧帧覆盖新操作后的画面。 */
  const ctrlRef = useRef({ inflight: false, opSeq: 0, unmounted: false })

  useEffect(() => {
    const ctrl = ctrlRef.current
    ctrl.unmounted = false
    return () => {
      ctrl.unmounted = true
      ctrl.opSeq += 1
    }
  }, [])

  const captureNow = useCallback(
    async (choiceOverride?: string, widthOverride?: number, quiet = false) => {
      const ctrl = ctrlRef.current
      if (quiet && ctrl.inflight) return
      const seq = quiet ? ctrl.opSeq : ++ctrl.opSeq
      ctrl.inflight = true
      const src = parseSourceChoice(choiceOverride ?? sourceChoice)
      const mw = widthOverride ?? maxWidth
      // 自动刷新时不改 busy，免得输入框每隔几秒被禁用一次、丢掉焦点。
      if (!quiet) {
        setBusy(true)
        setErrorMsg(null)
      }
      try {
        if (device === SELF_ID) await waitForRepaint(quiet ? 0 : 200)
        if (ctrl.unmounted || seq !== ctrl.opSeq) return
        const res = await backend.shotCapture({
          device,
          monitorIndex: src.monitorIndex,
          hwnd: src.hwnd,
          maxWidth: mw > 0 ? mw : undefined,
          quality: 80,
        })
        if (ctrl.unmounted || seq !== ctrl.opSeq) return
        setShot(res)
        if (quiet) setErrorMsg(null)
      } catch (err) {
        if (!ctrl.unmounted && seq === ctrl.opSeq) {
          setErrorMsg(humanError(err))
        }
      } finally {
        if (seq === ctrl.opSeq) {
          ctrl.inflight = false
          if (!quiet) setBusy(false)
        }
      }
    },
    [device, maxWidth, sourceChoice],
  )

  const loadTargetsAndShot = useCallback(async () => {
    const ctrl = ctrlRef.current
    const seq = ++ctrl.opSeq
    ctrl.inflight = true
    setBusy(true)
    setErrorMsg(null)
    try {
      const t = await backend.shotMonitors(device)
      if (ctrl.unmounted || seq !== ctrl.opSeq) return
      setTargets(t)
      const choice = pickSourceChoice(t, sourceChoice)
      setSourceChoice(choice)
      const src = parseSourceChoice(choice)
      if (device === SELF_ID) await waitForRepaint(200)
      if (ctrl.unmounted || seq !== ctrl.opSeq) return
      const res = await backend.shotCapture({
        device,
        monitorIndex: src.monitorIndex,
        hwnd: src.hwnd,
        maxWidth: maxWidth > 0 ? maxWidth : undefined,
        quality: 80,
      })
      if (ctrl.unmounted || seq !== ctrl.opSeq) return
      setShot(res)
      setErrorMsg(null)
    } catch (err) {
      if (!ctrl.unmounted && seq === ctrl.opSeq) {
        setErrorMsg(humanError(err))
      }
    } finally {
      if (seq === ctrl.opSeq) {
        ctrl.inflight = false
        setBusy(false)
      }
    }
  }, [device, maxWidth, sourceChoice])

  useEffect(() => {
    const ctrl = ctrlRef.current
    let cancelled = false
    const seq = ++ctrl.opSeq
    ctrl.inflight = true
    void backend
      .shotMonitors(device)
      .then(async (t) => {
        if (cancelled || seq !== ctrl.opSeq) return
        setTargets(t)
        const defaultChoice = pickSourceChoice(t, null)
        setSourceChoice(defaultChoice)
        const src = parseSourceChoice(defaultChoice)
        if (device === SELF_ID) await waitForRepaint(200)
        if (cancelled || seq !== ctrl.opSeq) return
        const res = await backend.shotCapture({
          device,
          monitorIndex: src.monitorIndex,
          hwnd: src.hwnd,
          maxWidth: 1280,
          quality: 80,
        })
        if (!cancelled && seq === ctrl.opSeq) {
          setShot(res)
          setErrorMsg(null)
        }
      })
      .catch((err) => {
        if (!cancelled && seq === ctrl.opSeq) setErrorMsg(humanError(err))
      })
      .finally(() => {
        if (seq === ctrl.opSeq) ctrl.inflight = false
      })
    return () => {
      cancelled = true
    }
  }, [device])

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

  const onImageClick = async (e: React.MouseEvent<HTMLImageElement>, modeOverride?: ClickMode) => {
    if (!shot || device === SELF_ID || !canInput || busy) return
    const imgEl = imgRef.current
    if (!imgEl) return
    const activeMode = modeOverride ?? clickMode
    const rect = imgEl.getBoundingClientRect()
    const { screenX, screenY } = mapClientPointToRemoteScreen(
      e.clientX,
      e.clientY,
      rect,
      shot.transform,
    )
    const ctrl = ctrlRef.current
    const src = parseSourceChoice(sourceChoice)
    const seq = ++ctrl.opSeq
    ctrl.inflight = true
    setBusy(true)
    setWarning(null)
    let delegatedRefresh = false
    try {
      const res = await backend.inputClick({
        device,
        screenX,
        screenY,
        button: activeMode === 'right' ? 'right' : 'left',
        double: activeMode === 'double',
        targetHwnd: shot.transform.targetHwnd,
        refreshMonitor: src.monitorIndex,
        refreshHwnd: src.hwnd,
        maxWidth: maxWidth > 0 ? maxWidth : undefined,
      })
      if (ctrl.unmounted || seq !== ctrl.opSeq) return
      if (res.uipiBlocked && res.warning) {
        setWarning(res.warning)
      } else if (res.refreshedShot) {
        setShot(res.refreshedShot)
      } else {
        delegatedRefresh = true
        ctrl.inflight = false
        await captureNow()
      }
    } catch (err) {
      if (!ctrl.unmounted && seq === ctrl.opSeq) {
        toast.error(humanError(err))
      }
    } finally {
      if (!delegatedRefresh && seq === ctrl.opSeq) {
        ctrl.inflight = false
        setBusy(false)
      }
    }
  }

  const sendKeyOrText = async (opts: { text?: string; keyCombo?: string }) => {
    if (device === SELF_ID || !canInput || busy) return
    const ctrl = ctrlRef.current
    const src = parseSourceChoice(sourceChoice)
    const seq = ++ctrl.opSeq
    ctrl.inflight = true
    setBusy(true)
    setWarning(null)
    let delegatedRefresh = false
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
      if (ctrl.unmounted || seq !== ctrl.opSeq) return
      if (res.uipiBlocked && res.warning) {
        setWarning(res.warning)
      } else if (res.refreshedShot) {
        setShot(res.refreshedShot)
      } else {
        delegatedRefresh = true
        ctrl.inflight = false
        await captureNow()
      }
      if (opts.text) setTextInput('')
    } catch (err) {
      if (!ctrl.unmounted && seq === ctrl.opSeq) {
        toast.error(humanError(err))
      }
    } finally {
      if (!delegatedRefresh && seq === ctrl.opSeq) {
        ctrl.inflight = false
        setBusy(false)
      }
    }
  }

  const interactiveRemote = device !== SELF_ID && canInput

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <Toolbar label="截图与点按控制栏" className="rounded-lg border border-line bg-bg/60 p-2">
        <div role="group" aria-label="画面来源与清晰度" className="flex flex-wrap items-center gap-2">
          <SourceSelect
            targets={targets}
            value={sourceChoice}
            onChange={(v) => {
              setSourceChoice(v)
              void captureNow(v)
            }}
          />

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
        <RemoteInputBar
          text={textInput}
          onTextChange={setTextInput}
          disabled={!canInput || busy}
          placeholder={
            canInput
              ? '输入要发往对方当前焦点窗口的文字…'
              : '对方尚未开启“点按”权限，当前为只读看屏模式'
          }
          onSendText={(text) => void sendKeyOrText({ text })}
          onSendShortcut={(keyCombo) => void sendKeyOrText({ keyCombo })}
          className="rounded-lg border border-line bg-bg/40 px-3 py-2"
        />
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
                void onImageClick(e, 'right')
              }}
              // h-auto w-auto 让浏览器按原比例缩放；只写最大宽高会被横竖分别压扁。
              className={`rounded border border-line shadow-sm ${
                viewMode === 'fit'
                  ? 'h-auto max-h-[calc(100vh-300px)] w-auto max-w-full'
                  : 'h-auto w-auto max-w-none'
              } ${interactiveRemote ? 'cursor-crosshair' : ''}`}
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
