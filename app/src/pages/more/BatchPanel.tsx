import { useEffect, useMemo, useState } from 'react'
import { backend } from '../../api'
import { humanError } from '../../api/errors'
import type { BatchDeviceResult, DeviceId, PowerActionKind } from '../../api/types'
import { Icon } from '../../components/Icon'
import { Button } from '../../components/Modal'
import { isOnline, useDevices } from '../../store/devices'
import { toast } from '../../store/toasts'
import { confirmDialog } from '../../store/ui'

/** 任务 3.8：批量操作（勾选多台已配对设备，并发执行启动软件、跑命令脚本或电源动作，按台展示实时结果）。 */
export function BatchPanel() {
  const devices = useDevices((s) => s.devices)
  const [checkedIds, setCheckedIds] = useState<Set<DeviceId>>(
    () => new Set(devices.filter((d) => isOnline(d)).map((d) => d.id)),
  )
  const [op, setOp] = useState<'run_command' | 'launch_app' | 'power'>('run_command')
  const [command, setCommand] = useState('hostname && whoami')
  const [shell, setShell] = useState<'powershell' | 'cmd'>('powershell')
  const [appTarget, setAppTarget] = useState('')
  const [powerAction, setPowerAction] = useState<PowerActionKind>('lock')
  const [running, setRunning] = useState(false)
  const [results, setResults] = useState<BatchDeviceResult[]>([])

  useEffect(() => {
    return backend.on('batch-item', (item) => {
      setResults((prev) => {
        const idx = prev.findIndex((p) => p.peerId === item.peerId)
        if (idx < 0) return [...prev, item]
        const copy = prev.slice()
        copy[idx] = item
        return copy
      })
    })
  }, [])

  // 自动过滤已被移除的设备 ID，确保计数与提交目标均为现存设备
  const existingIds = useMemo(() => new Set(devices.map((d) => d.id)), [devices])
  const validCheckedIds = useMemo(() => {
    const out = new Set<DeviceId>()
    for (const id of checkedIds) {
      if (existingIds.has(id)) out.add(id)
    }
    return out
  }, [checkedIds, existingIds])

  const toggleDevice = (id: DeviceId) => {
    setCheckedIds((prev) => {
      const next = new Set<DeviceId>()
      for (const x of prev) {
        if (existingIds.has(x)) next.add(x)
      }
      if (next.has(id)) next.delete(id)
      else if (existingIds.has(id)) next.add(id)
      return next
    })
  }

  const runBatch = async (e: React.FormEvent) => {
    e.preventDefault()
    const targets = [...validCheckedIds]
    if (!targets.length) {
      toast.warn('请先勾选至少一台设备')
      return
    }
    if (op === 'power' && (powerAction === 'shutdown' || powerAction === 'restart')) {
      const ok = await confirmDialog({
        title: `批量对 ${targets.length} 台设备执行${powerAction === 'shutdown' ? '关机' : '重启'}？`,
        body: '每台设备屏幕都会弹出 10 秒可取消倒计时，请确认已保存各设备上的工作。',
        okText: '确认批量发送',
        danger: true,
      })
      if (!ok) return
    }

    setRunning(true)
    setResults([])
    try {
      const finalResults = await backend.batchExec({
        devices: targets,
        op,
        target: op === 'launch_app' ? appTarget : undefined,
        powerAction: op === 'power' ? powerAction : undefined,
        command: op === 'run_command' ? command : undefined,
        shell: op === 'run_command' ? shell : undefined,
      })
      setResults(finalResults)
    } catch (err) {
      toast.error(humanError(err))
    } finally {
      setRunning(false)
    }
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="rounded-lg border border-line bg-panel p-4">
        <div className="mb-2 flex items-center justify-between">
          <h3 className="text-sm font-medium">1. 选择目标电脑（已选 {validCheckedIds.size} 台）</h3>
          <div className="flex gap-2 text-xs">
            <button
              type="button"
              onClick={() => setCheckedIds(new Set(devices.filter((d) => isOnline(d)).map((d) => d.id)))}
              className="text-accent hover:underline"
            >
              全选在线设备
            </button>
            <button
              type="button"
              onClick={() => setCheckedIds(new Set())}
              className="text-muted hover:underline"
            >
              清空
            </button>
          </div>
        </div>

        {devices.length === 0 ? (
          <p className="text-xs text-muted">尚未配对其他电脑，配对后即可在此多选并发执行操作。</p>
        ) : (
          <div className="grid grid-cols-[repeat(auto-fill,minmax(200px,1fr))] gap-2">
            {devices.map((d) => {
              const on = isOnline(d)
              return (
                <label
                  key={d.id}
                  className="flex cursor-pointer items-center gap-2.5 rounded-md border border-line px-3 py-2 text-sm hover:bg-hover"
                >
                  <input
                    type="checkbox"
                    checked={validCheckedIds.has(d.id)}
                    onChange={() => toggleDevice(d.id)}
                    className="size-4 accent-[var(--color-accent)]"
                  />
                  <span className="truncate font-medium">{d.name}</span>
                  <span
                    className={`ml-auto shrink-0 rounded px-1.5 py-0.5 text-[11px] ${
                      on ? 'bg-ok/15 text-ok' : 'bg-hover text-faint'
                    }`}
                  >
                    {on ? '在线' : '离线'}
                  </span>
                </label>
              )
            })}
          </div>
        )}
      </div>

      <form onSubmit={(e) => void runBatch(e)} className="flex flex-col gap-3 rounded-lg border border-line bg-panel p-4 text-sm">
        <h3 className="font-medium">2. 选择批量动作</h3>
        <div className="flex flex-wrap gap-2">
          {(
            [
              ['run_command', '批量执行命令 / 脚本'],
              ['launch_app', '批量打开软件或路径'],
              ['power', '批量电源操作'],
            ] as const
          ).map(([id, label]) => (
            <button
              key={id}
              type="button"
              aria-pressed={op === id}
              onClick={() => setOp(id)}
              className={`h-8 rounded-md border px-3 text-xs ${
                op === id
                  ? 'border-accent bg-accent-soft font-medium text-accent'
                  : 'border-line bg-panel hover:bg-hover'
              }`}
            >
              {label}
            </button>
          ))}
        </div>

        {op === 'run_command' && (
          <div className="flex flex-col gap-2">
            <div className="flex items-center gap-2">
              <span className="text-xs text-muted">解释器：</span>
              <select
                aria-label="批量命令解释器"
                value={shell}
                onChange={(e) => setShell(e.target.value as 'powershell' | 'cmd')}
                className="h-7 rounded border border-line bg-bg px-2 text-xs outline-none"
              >
                <option value="powershell">PowerShell</option>
                <option value="cmd">CMD</option>
              </select>
            </div>
            <input
              required
              value={command}
              onChange={(e) => setCommand(e.target.value)}
              placeholder="输入要在选中设备上并发执行的命令…"
              aria-label="批量命令内容"
              className="h-9 rounded-md border border-line bg-bg px-3 font-mono text-xs outline-none focus:border-accent"
            />
          </div>
        )}

        {op === 'launch_app' && (
          <input
            required
            value={appTarget}
            onChange={(e) => setAppTarget(e.target.value)}
            placeholder="输入软件名称（如 记事本 / 微信）或可执行文件路径…"
            aria-label="批量打开软件名称或路径"
            className="h-9 rounded-md border border-line bg-bg px-3 text-sm outline-none focus:border-accent"
          />
        )}

        {op === 'power' && (
          <select
            aria-label="批量电源动作"
            value={powerAction}
            onChange={(e) => setPowerAction(e.target.value as PowerActionKind)}
            className="h-9 max-w-xs rounded-md border border-line bg-bg px-2.5 text-sm outline-none focus:border-accent"
          >
            <option value="lock">批量锁屏</option>
            <option value="sleep">批量睡眠（10 秒可取消）</option>
            <option value="restart">批量重启（10 秒可取消）</option>
            <option value="shutdown">批量关机（10 秒可取消）</option>
          </select>
        )}

        <div className="flex justify-end">
          <Button type="submit" variant="primary" disabled={running || validCheckedIds.size === 0}>
            <Icon name={running ? 'loader' : 'play'} size={14} className={running ? 'spin' : ''} />
            {running ? '正在并发执行…' : `对 ${validCheckedIds.size} 台设备并发执行`}
          </Button>
        </div>
      </form>

      {results.length > 0 && (
        <div className="rounded-lg border border-line bg-panel p-4">
          <h3 className="mb-2.5 text-sm font-medium">执行结果（按台返回）</h3>
          <ul className="flex flex-col gap-2">
            {results.map((r) => (
              <li key={r.peerId} className="rounded-md border border-line bg-bg/50 p-3 text-xs">
                <div className="flex items-center gap-2">
                  <Icon
                    name={r.ok ? 'check' : 'alert'}
                    size={15}
                    className={r.ok ? 'text-ok' : 'text-bad'}
                  />
                  <span className="font-semibold text-sm">{r.peerName}</span>
                  <span className="text-muted">{r.summary}</span>
                  <span className="ml-auto text-muted">{r.elapsedMs} ms</span>
                </div>
                {r.output && (
                  <pre className="selectable scroll-thin mt-2 max-h-36 overflow-auto rounded bg-panel p-2 font-mono text-[11px] whitespace-pre-wrap">
                    <code>{r.output}</code>
                  </pre>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  )
}
