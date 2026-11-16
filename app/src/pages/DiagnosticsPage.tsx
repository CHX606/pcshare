import { useCallback, useEffect, useState } from 'react'
import { backend } from '../api'
import { humanError } from '../api/errors'
import type { DiagLogs, DiagSummary, NetcheckReport } from '../api/types'
import { Icon } from '../components/Icon'
import { Button } from '../components/Modal'
import { formatSize } from '../lib/format'
import { toast } from '../store/toasts'

export function DiagnosticsPage() {
  const [summary, setSummary] = useState<DiagSummary | null>(null)
  const [logs, setLogs] = useState<DiagLogs | null>(null)
  const [report, setReport] = useState<NetcheckReport | null>(null)
  const [checking, setChecking] = useState(false)
  const [exporting, setExporting] = useState(false)
  const [logTab, setLogTab] = useState<'runtime' | 'audit'>('runtime')

  const refresh = useCallback(async () => {
    try {
      const [s, l] = await Promise.all([backend.diagSummary(), backend.diagLogs(200)])
      setSummary(s)
      setLogs(l)
    } catch (e) {
      toast.error(humanError(e))
    }
  }, [])

  useEffect(() => {
    let alive = true
    Promise.all([backend.diagSummary(), backend.diagLogs(200)]).then(
      ([s, l]) => {
        if (!alive) return
        setSummary(s)
        setLogs(l)
      },
      (e) => alive && toast.error(humanError(e)),
    )
    return () => {
      alive = false
    }
  }, [])

  const runCheck = async () => {
    setChecking(true)
    try {
      const r = await backend.diagNetcheck()
      setReport(r)
      await refresh()
    } catch (e) {
      toast.error(`网络体检失败：${humanError(e)}`)
    } finally {
      setChecking(false)
    }
  }

  const exportBundle = async () => {
    setExporting(true)
    try {
      const dir = await backend.pickLocalDir()
      if (!dir) return
      const outPath = await backend.diagExport(dir)
      toast.success(`诊断包已导出到：${outPath}`)
    } catch (e) {
      toast.error(`导出失败：${humanError(e)}`)
    } finally {
      setExporting(false)
    }
  }

  const yesNo = (v: boolean | null) => (v == null ? '未知' : v ? '支持' : '未检出')

  return (
    <section aria-label="诊断" className="scroll-thin flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto p-5 text-sm">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h2 className="font-semibold">运行状态与网络诊断</h2>
          <p className="text-xs text-muted">查看当前线路、本机资源占用、网络直连条件与最近日志，导出结果绝不包含本机私钥。</p>
        </div>
        <div className="flex items-center gap-2">
          <Button onClick={() => void refresh()}>
            <Icon name="refresh" /> 刷新状态
          </Button>
          <Button variant="primary" disabled={exporting} onClick={() => void exportBundle()}>
            {exporting ? <Icon name="loader" className="spin" /> : <Icon name="download" />}
            导出诊断包…
          </Button>
        </div>
      </div>

      {/* 本机运行概览与设备当前路由 */}
      <div className="grid grid-cols-[repeat(auto-fit,minmax(280px,1fr))] gap-3">
        <div className="rounded-lg border border-line bg-panel p-4">
          <h3 className="mb-2 text-xs font-medium text-muted">本机核心状态</h3>
          {summary ? (
            <dl className="grid grid-cols-[100px_1fr] gap-y-1.5 text-xs">
              <dt className="text-muted">程序版本</dt>
              <dd className="tabular-nums">v{summary.appVersion}（{summary.os}）</dd>
              <dt className="text-muted">核心进程内存</dt>
              <dd className="tabular-nums">{formatSize(summary.workingSetBytes)}</dd>
              <dt className="text-muted">被控状态</dt>
              <dd>{summary.hostPaused ? <span className="text-warn">已暂停被控</span> : <span className="text-ok">正常待命</span>}</dd>
              <dt className="text-muted">传输限速</dt>
              <dd className="tabular-nums">{summary.speedLimitBps ? `${formatSize(summary.speedLimitBps)}/秒` : '不限速'}</dd>
            </dl>
          ) : (
            <p className="text-xs text-muted">正在读取…</p>
          )}
        </div>

        <div className="rounded-lg border border-line bg-panel p-4">
          <h3 className="mb-2 text-xs font-medium text-muted">已配对设备当前连接方式</h3>
          {!summary || summary.peerRoutes.length === 0 ? (
            <p className="text-xs text-muted">暂无已配对设备</p>
          ) : (
            <ul className="flex flex-col gap-1.5 text-xs">
              {summary.peerRoutes.map((r) => (
                <li key={r.peerId} className="flex items-center justify-between gap-2 rounded bg-hover/60 px-2.5 py-1.5">
                  <span className="truncate font-medium">{r.name}</span>
                  <span className="shrink-0 tabular-nums text-muted">{r.detail}</span>
                </li>
              ))}
            </ul>
          )}
        </div>
      </div>

      {/* 网络体检：点之前明确告知会访问哪些服务器（任务 2.11 硬性要求） */}
      <div className="rounded-lg border border-line bg-panel p-4">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="max-w-2xl">
            <h3 className="font-medium">网络直连体检</h3>
            <p className="mt-1 text-xs leading-relaxed text-muted">
              本检测仅在你点击右侧“开始网络体检”时运行，不会在后台自动发起。运行时会向以下中转与探测服务器发送 UDP/HTTPS 测速包以检测 NAT 类型和延迟：
            </p>
            <ul className="mt-1.5 flex flex-wrap gap-1.5 text-xs">
              {(summary?.netcheckTargets ?? ['https://use1-1.relay.n0.iroh.link', 'https://euw1-1.relay.n0.iroh.link']).map((srv) => (
                <li key={srv} className="selectable rounded bg-hover px-2 py-0.5 font-mono text-[11.5px]">
                  {srv}
                </li>
              ))}
            </ul>
          </div>
          <Button variant="primary" disabled={checking} onClick={() => void runCheck()}>
            {checking ? <Icon name="loader" className="spin" /> : <Icon name="shield" />}
            {checking ? '正在检测网络…' : '开始网络体检'}
          </Button>
        </div>

        {report && (
          <div className="mt-4 border-t border-line pt-3 text-xs">
            <p className="mb-2 font-medium text-fg">结论：{report.summary}</p>
            <div className="grid grid-cols-[repeat(auto-fit,minmax(160px,1fr))] gap-2 text-muted">
              <div>UDP IPv4：{report.udpIpv4 ? '通畅' : '受阻'}</div>
              <div>UDP IPv6：{report.udpIpv6 ? '通畅' : '未启用'}</div>
              <div>端口随目标变化：{report.mappingVariesByDestIp == null ? '未知' : report.mappingVariesByDestIp ? '是（对称型 NAT）' : '否（易直连）'}</div>
              <div>UPnP / PCP / NAT-PMP：{yesNo(report.upnpAvailable)} / {yesNo(report.pcpAvailable)} / {yesNo(report.natPmpAvailable)}</div>
            </div>
            {report.relayLatencies.length > 0 && (
              <div className="mt-2 flex flex-wrap gap-2">
                {report.relayLatencies.map((item) => (
                  <span key={item.url} className="rounded bg-hover px-2 py-0.5 font-mono tabular-nums">
                    {item.url}：{item.rttMs != null ? `${item.rttMs} ms` : '超时'}
                  </span>
                ))}
              </div>
            )}
          </div>
        )}
      </div>

      {/* 最近日志与操作审计查看器 */}
      <div className="flex min-h-56 flex-1 flex-col rounded-lg border border-line bg-panel">
        <div className="flex items-center gap-1 border-b border-line px-3 pt-2 text-xs" role="tablist" aria-label="日志分类">
          <button
            type="button"
            role="tab"
            aria-selected={logTab === 'runtime'}
            onClick={() => setLogTab('runtime')}
            className={`border-b-2 px-3 pb-2 font-medium ${logTab === 'runtime' ? 'border-accent text-fg' : 'border-transparent text-muted hover:text-fg'}`}
          >
            最近运行日志（{logs?.runtimeLogs.length ?? 0}）
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={logTab === 'audit'}
            onClick={() => setLogTab('audit')}
            className={`border-b-2 px-3 pb-2 font-medium ${logTab === 'audit' ? 'border-accent text-fg' : 'border-transparent text-muted hover:text-fg'}`}
          >
            被控操作审计（{logs?.audits.length ?? 0}）
          </button>
        </div>
        <div className="selectable scroll-thin flex-1 overflow-y-auto p-3 font-mono text-xs leading-relaxed">
          {logTab === 'runtime' ? (
            !logs || logs.runtimeLogs.length === 0 ? (
              <p className="text-muted">暂无运行日志</p>
            ) : (
              logs.runtimeLogs.map((line, idx) => (
                <div key={idx} className="py-0.5 break-all text-muted">
                  {line}
                </div>
              ))
            )
          ) : !logs || logs.audits.length === 0 ? (
            <p className="text-muted">暂无被控操作记录</p>
          ) : (
            logs.audits.map((a, idx) => (
              <div key={idx} className="py-0.5 break-all text-muted">
                [{a.ts}] {a.peerName} ({a.peerId.slice(0, 8)}…) · {a.cmd} {a.detail ? `· ${a.detail}` : ''} · {a.ok ? '成功' : '拒绝/失败'}
              </div>
            ))
          )}
        </div>
      </div>
    </section>
  )
}
