import { FitAddon } from '@xterm/addon-fit'
import { Terminal } from '@xterm/xterm'
import '@xterm/xterm/css/xterm.css'
import { useCallback, useEffect, useRef, useState } from 'react'
import { backend } from '../../api'
import { humanError } from '../../api/errors'
import type { DeviceId, TermSession } from '../../api/types'
import { Icon } from '../../components/Icon'
import { Button } from '../../components/Modal'
import { Toolbar } from '../../components/Toolbar'
import { base64FromUtf8, bytesFromBase64 } from '../../lib/stage3'

const MAX_BUFFERED_CHUNKS = 512
const RESIZE_DEBOUNCE_MS = 100

/** 任务 3.6：交互式伪终端（被控端通过系统 ConPTY 启动 PowerShell 或 cmd，前端用 xterm.js 渲染并同步尺寸）。 */
export function TerminalPanel({ device }: { device: DeviceId }) {
  const hostRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<Terminal | null>(null)
  const fitRef = useRef<FitAddon | null>(null)
  const ctrlRef = useRef({
    sessionId: null as string | null,
    seq: 0,
    connecting: false,
    unmounted: false,
    lastSize: null as { cols: number; rows: number } | null,
    pendingOutput: new Map<string, string[]>(),
    pendingExit: new Map<string, number>(),
  })

  const [shell, setShell] = useState<'powershell' | 'cmd'>('powershell')
  const [session, setSession] = useState<TermSession | null>(null)
  const [exitedCode, setExitedCode] = useState<number | null>(null)
  const [errorMsg, setErrorMsg] = useState<string | null>(null)
  const [connecting, setConnecting] = useState(false)

  const connectShell = useCallback(
    async (targetShell: 'powershell' | 'cmd') => {
      const term = termRef.current
      const fit = fitRef.current
      if (!term || !fit) return
      const ctrl = ctrlRef.current

      const seq = ++ctrl.seq
      if (ctrl.sessionId) {
        const old = ctrl.sessionId
        ctrl.sessionId = null
        await backend.termClose(old).catch(() => {})
        if (ctrl.unmounted || seq !== ctrl.seq) return
      }

      ctrl.connecting = true
      ctrl.pendingOutput.clear()
      ctrl.pendingExit.clear()
      ctrl.lastSize = null
      setConnecting(true)
      setErrorMsg(null)
      setExitedCode(null)
      term.clear()
      fit.fit()

      try {
        const cols = Math.max(40, term.cols || 100)
        const rows = Math.max(12, term.rows || 28)
        const opened = await backend.termOpen(device, targetShell, cols, rows)

        // 若返回前组件已卸载或用户已切换 shell，立即关闭新会话防远端 ConPTY 泄漏
        if (ctrl.unmounted || seq !== ctrl.seq) {
          ctrl.pendingOutput.delete(opened.sessionId)
          ctrl.pendingExit.delete(opened.sessionId)
          void backend.termClose(opened.sessionId).catch(() => {})
          return
        }

        ctrl.connecting = false
        ctrl.sessionId = opened.sessionId
        ctrl.lastSize = { cols, rows }
        setSession(opened)

        // 回放 termOpen 返回前早到的首批输出与退出事件
        const buffered = ctrl.pendingOutput.get(opened.sessionId)
        ctrl.pendingOutput.clear()
        if (buffered) {
          for (const chunk of buffered) term.write(bytesFromBase64(chunk))
        }
        const earlyExit = ctrl.pendingExit.get(opened.sessionId)
        ctrl.pendingExit.clear()
        if (earlyExit !== undefined) {
          setExitedCode(earlyExit)
          term.writeln(`\r\n\x1b[33m[终端进程已退出，退出码 ${earlyExit}]\x1b[0m`)
          ctrl.sessionId = null
        } else {
          term.focus()
        }
      } catch (err) {
        if (!ctrl.unmounted && seq === ctrl.seq) {
          ctrl.connecting = false
          ctrl.pendingOutput.clear()
          ctrl.pendingExit.clear()
          setSession(null)
          setErrorMsg(humanError(err))
        }
      } finally {
        if (!ctrl.unmounted && seq === ctrl.seq) {
          setConnecting(false)
        }
      }
    },
    [device],
  )

  useEffect(() => {
    const container = hostRef.current
    if (!container) return
    const ctrl = ctrlRef.current
    ctrl.unmounted = false

    const term = new Terminal({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: "'Cascadia Mono', Consolas, monospace",
      theme: {
        background: '#141619',
        foreground: '#e4e6e9',
        cursor: '#5b8def',
        selectionBackground: 'rgba(91, 141, 239, 0.32)',
      },
    })
    const fit = new FitAddon()
    term.loadAddon(fit)
    term.open(container)
    fit.fit()

    termRef.current = term
    fitRef.current = fit

    const dataDisp = term.onData((data) => {
      const sid = ctrl.sessionId
      if (!sid) return
      void backend.termWrite(sid, base64FromUtf8(data)).catch(() => {})
    })

    const offOut = backend.on('term-output', (ev) => {
      if (ev.sessionId === ctrl.sessionId) {
        term.write(bytesFromBase64(ev.dataB64))
        return
      }
      if (ctrl.connecting) {
        let queue = ctrl.pendingOutput.get(ev.sessionId)
        if (!queue) {
          queue = []
          ctrl.pendingOutput.set(ev.sessionId, queue)
        }
        if (queue.length < MAX_BUFFERED_CHUNKS) queue.push(ev.dataB64)
      }
    })

    const offExit = backend.on('term-exit', (ev) => {
      const code = ev.exitCode ?? 0
      if (ev.sessionId === ctrl.sessionId) {
        setExitedCode(code)
        term.writeln(`\r\n\x1b[33m[终端进程已退出，退出码 ${code}]\x1b[0m`)
        ctrl.sessionId = null
        return
      }
      if (ctrl.connecting) {
        ctrl.pendingExit.set(ev.sessionId, code)
      }
    })

    let resizeTimer: number | null = null
    const ro = new ResizeObserver(() => {
      if (resizeTimer !== null) window.clearTimeout(resizeTimer)
      resizeTimer = window.setTimeout(() => {
        resizeTimer = null
        try {
          fit.fit()
          const sid = ctrl.sessionId
          if (sid && term.cols >= 20 && term.rows >= 5) {
            const last = ctrl.lastSize
            if (!last || last.cols !== term.cols || last.rows !== term.rows) {
              ctrl.lastSize = { cols: term.cols, rows: term.rows }
              void backend.termResize(sid, term.cols, term.rows).catch(() => {})
            }
          }
        } catch {
          // Ignore transient layout measurements
        }
      }, RESIZE_DEBOUNCE_MS)
    })
    ro.observe(container)

    void connectShell('powershell')

    return () => {
      ctrl.unmounted = true
      ctrl.seq += 1
      ctrl.connecting = false
      if (resizeTimer !== null) window.clearTimeout(resizeTimer)
      ro.disconnect()
      offOut()
      offExit()
      dataDisp.dispose()
      ctrl.pendingOutput.clear()
      ctrl.pendingExit.clear()
      const sid = ctrl.sessionId
      ctrl.sessionId = null
      if (sid) void backend.termClose(sid).catch(() => {})
      term.dispose()
    }
  }, [connectShell])

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <Toolbar label="命令行工具栏" className="rounded-lg border border-line bg-bg/60 p-2">
        <div role="group" aria-label="终端类型" className="flex items-center gap-1">
          {(
            [
              ['powershell', 'PowerShell'],
              ['cmd', '命令提示符 (CMD)'],
            ] as const
          ).map(([id, label]) => (
            <button
              key={id}
              type="button"
              aria-pressed={shell === id}
              disabled={connecting}
              onClick={() => {
                setShell(id)
                void connectShell(id)
              }}
              className="h-8 rounded-md border border-line bg-panel px-3 text-xs hover:bg-hover disabled:opacity-40"
            >
              {label}
            </button>
          ))}
        </div>

        <span role="separator" aria-orientation="vertical" className="mx-1 h-5 border-l border-line" />

        <Button onClick={() => void connectShell(shell)} disabled={connecting}>
          <Icon name="refresh" size={14} className={connecting ? 'spin' : ''} />
          重新打开会话
        </Button>
        <Button onClick={() => termRef.current?.clear()}>清屏</Button>

        {session && (
          <span className="ml-auto text-xs text-muted">
            {session.shellName} · PID {session.pid}
            {exitedCode !== null ? `（已退出：${exitedCode}）` : ''}
          </span>
        )}
      </Toolbar>

      {errorMsg && (
        <div role="alert" className="flex items-center gap-2.5 rounded-lg border border-warn/50 bg-warn/10 px-3.5 py-2.5 text-xs">
          <Icon name="alert" size={16} className="shrink-0 text-warn" />
          <span className="flex-1">
            {errorMsg}（若需使用远端终端，请先在对方电脑的设备设置里开启“命令行”权限）
          </span>
        </div>
      )}

      <div
        ref={hostRef}
        data-capture-keys="true"
        onKeyDown={(e) => {
          e.nativeEvent.stopPropagation()
        }}
        className="min-h-[340px] flex-1 overflow-hidden rounded-lg border border-line bg-[#141619] p-2"
      />
    </div>
  )
}
