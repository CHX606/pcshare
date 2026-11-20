import { useCallback, useEffect, useRef, useState } from 'react'
import { backend } from '../../api'
import { humanError } from '../../api/errors'
import type { DeviceId, FsSearchBatchEvent, FsSearchHit } from '../../api/types'
import { Icon } from '../../components/Icon'
import { Button } from '../../components/Modal'
import { formatSize } from '../../lib/format'
import { parentPath } from '../../lib/path'
import { useFiles, type PaneSide } from '../../store/files'
import { toast } from '../../store/toasts'

interface Props {
  device: DeviceId
  side: PaneSide
  rootPath: string
  onClose: () => void
}

/** 任务 3.9：文件搜索栏（被控端遍历、边搜边回结果、支持随时取消）。 */
export function FileSearchPanel({ device, side, rootPath, onClose }: Props) {
  const [query, setQuery] = useState('')
  const [searchId, setSearchId] = useState<string | null>(null)
  const [running, setRunning] = useState(false)
  const [scannedDirs, setScannedDirs] = useState(0)
  const [hits, setHits] = useState<FsSearchHit[]>([])

  const activeSearchIdRef = useRef<string | null>(null)
  const awaitingStartRef = useRef(false)
  const requestSeqRef = useRef(0)
  const cancelledSeqRef = useRef(0)
  const unmountedRef = useRef(false)
  const pendingBatchesRef = useRef<Map<string, FsSearchBatchEvent[]>>(new Map())

  const applyBatch = useCallback((ev: FsSearchBatchEvent) => {
    setScannedDirs(ev.scannedDirs)
    if (ev.entries.length > 0) {
      setHits((prev) => [...prev, ...ev.entries])
    }
    if (ev.done) {
      setRunning(false)
    }
  }, [])

  // 挂载即订阅 fs-search-batch，缓冲早于 searchId 返回的批次，避免小目录漏批卡死
  useEffect(() => {
    unmountedRef.current = false
    const pendingMap = pendingBatchesRef.current
    const off = backend.on('fs-search-batch', (ev) => {
      if (ev.device !== device) return
      const curId = activeSearchIdRef.current
      if (curId && ev.searchId === curId) {
        applyBatch(ev)
        return
      }
      if (awaitingStartRef.current) {
        let queue = pendingMap.get(ev.searchId)
        if (!queue) {
          queue = []
          pendingMap.set(ev.searchId, queue)
        }
        queue.push(ev)
      }
    })
    return () => {
      unmountedRef.current = true
      cancelledSeqRef.current = requestSeqRef.current
      off()
      pendingMap.clear()
      const sid = activeSearchIdRef.current
      activeSearchIdRef.current = null
      if (sid) void backend.fsSearchCancel(sid).catch(() => {})
    }
  }, [device, applyBatch])

  const startSearch = async (e: React.FormEvent) => {
    e.preventDefault()
    const q = query.trim()
    if (!q) return
    const prevSid = activeSearchIdRef.current
    if (prevSid) {
      activeSearchIdRef.current = null
      await backend.fsSearchCancel(prevSid).catch(() => {})
    }
    const seq = ++requestSeqRef.current
    awaitingStartRef.current = true
    pendingBatchesRef.current.clear()
    setSearchId(null)
    setHits([])
    setScannedDirs(0)
    setRunning(true)
    try {
      const sid = await backend.fsSearchStart(device, rootPath, q)
      if (unmountedRef.current || seq !== requestSeqRef.current || cancelledSeqRef.current >= seq) {
        pendingBatchesRef.current.delete(sid)
        void backend.fsSearchCancel(sid).catch(() => {})
        return
      }
      awaitingStartRef.current = false
      activeSearchIdRef.current = sid
      setSearchId(sid)
      const buffered = pendingBatchesRef.current.get(sid)
      pendingBatchesRef.current.clear()
      if (buffered) {
        for (const ev of buffered) applyBatch(ev)
      }
    } catch (err) {
      if (seq === requestSeqRef.current) {
        awaitingStartRef.current = false
        pendingBatchesRef.current.clear()
        setRunning(false)
        toast.error(humanError(err))
      }
    }
  }

  const cancelSearch = async () => {
    cancelledSeqRef.current = requestSeqRef.current
    awaitingStartRef.current = false
    pendingBatchesRef.current.clear()
    setRunning(false)
    const sid = activeSearchIdRef.current
    if (sid) {
      activeSearchIdRef.current = null
      await backend.fsSearchCancel(sid).catch(() => {})
    }
  }

  const jumpToHit = (hit: FsSearchHit) => {
    if (hit.dir) {
      useFiles.getState().open(device, hit.path, 'push', side)
      onClose()
    } else {
      const dir = parentPath(hit.path)
      useFiles.getState().open(device, dir, 'push', side)
      useFiles.getState().select(device, new Set([hit.name]), hit.name, hit.name, side)
      onClose()
    }
  }

  return (
    <div className="border-b border-line bg-bg/70 px-3 py-2.5">
      <search>
        <form onSubmit={startSearch} className="flex items-center gap-2">
          <div className="flex flex-1 items-center gap-2 rounded-md border border-line bg-panel px-2.5 py-1 focus-within:border-accent">
            <Icon name="search" size={15} className="text-muted" />
            <input
              autoFocus
              type="search"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder={`在 ${rootPath} 下搜索文件名（支持中文与拼音首字母）…`}
              aria-label="在当前目录及子目录搜索文件"
              className="w-full bg-transparent text-sm outline-none"
            />
          </div>
          {running ? (
            <Button onClick={() => void cancelSearch()}>
              <Icon name="stop" size={13} />
              停止
            </Button>
          ) : (
            <Button type="submit" variant="primary" disabled={!query.trim()}>
              搜索
            </Button>
          )}
          <Button variant="ghost" aria-label="关闭搜索栏" onClick={onClose}>
            <Icon name="x" size={14} />
          </Button>
        </form>
      </search>

      {(running || hits.length > 0 || searchId) && (
        <div className="mt-2">
          <div className="mb-1.5 flex items-center justify-between text-xs text-muted">
            <span>
              {running
                ? `正在搜索子目录（已扫描 ${scannedDirs} 个文件夹），已找到 ${hits.length} 项…`
                : `搜索结束：共扫描 ${scannedDirs} 个文件夹，找到 ${hits.length} 项`}
            </span>
            {hits.length > 0 && <span>点击条目可直接定位所在目录</span>}
          </div>
          {hits.length > 0 && (
            <ul className="scroll-thin max-h-48 overflow-y-auto rounded-md border border-line bg-panel">
              {hits.map((h) => (
                <li key={h.path} className="border-b border-line/60 last:border-b-0">
                  <button
                    type="button"
                    onClick={() => jumpToHit(h)}
                    className="flex w-full items-center gap-2.5 px-3 py-1.5 text-left text-xs hover:bg-hover"
                  >
                    <Icon
                      name={h.dir ? 'folder' : 'file'}
                      size={14}
                      className={h.dir ? 'text-accent' : 'text-muted'}
                    />
                    <span className="font-medium">{h.name}</span>
                    <span className="min-w-0 flex-1 truncate text-muted">{h.path}</span>
                    {!h.dir && <span className="shrink-0 text-muted">{formatSize(h.size)}</span>}
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  )
}
