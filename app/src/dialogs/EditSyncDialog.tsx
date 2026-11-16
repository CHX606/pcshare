import { useEffect, useState } from 'react'
import { backend } from '../api'
import { humanError } from '../api/errors'
import type { EditSyncPrompt } from '../api/types'
import { Button, Modal } from '../components/Modal'
import { toast } from '../store/toasts'

/** 任务 3.4：取回本机编辑的文件在本地保存后，询问是否存回远端，并在远端文件被改动过时明确提示冲突。 */
export function EditSyncDialog() {
  const [queue, setQueue] = useState<EditSyncPrompt[]>([])
  const [autoSync, setAutoSync] = useState(false)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    return backend.on('edit-prompt', (ev) => {
      setQueue((prev) => {
        const filtered = prev.filter((p) => p.sessionId !== ev.sessionId)
        return [...filtered, ev]
      })
    })
  }, [])

  const current = queue[0] ?? null
  const shift = () => {
    setAutoSync(false)
    setQueue((prev) => prev.slice(1))
  }

  const confirmUpload = async (forceOverwrite: boolean) => {
    if (!current) return
    setBusy(true)
    try {
      const res = await backend.fsEditSyncBack(current.sessionId, autoSync, forceOverwrite)
      if (res.conflict && !forceOverwrite) {
        // 后台刚刚检测到远端文件也被改过了，更新当前弹窗为冲突模式
        setQueue((prev) =>
          prev.map((item, idx) => (idx === 0 ? { ...item, conflict: true } : item)),
        )
      } else {
        toast.success(`已将「${current.fileName}」存回「${current.peerName}」`)
        shift()
      }
    } catch (err) {
      toast.error(humanError(err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Modal
      open={!!current}
      onClose={shift}
      title={current?.conflict ? '远端文件冲突提醒' : '要把修改存回对方电脑吗？'}
      width={480}
    >
      {current && (
        <div className="flex flex-col gap-4 text-sm">
          {current.conflict ? (
            <div className="rounded-lg border border-warn/40 bg-warn/10 p-3 text-sm leading-relaxed">
              <p className="font-medium text-warn">对方电脑上的这份文件在此期间也被修改过了！</p>
              <p className="mt-1 text-xs text-muted">
                文件：{current.remotePath}（位于「{current.peerName}」）。直接回传会覆盖对方刚才的改动。
              </p>
            </div>
          ) : (
            <p className="leading-relaxed text-muted">
              检测到你在本机保存了来自「<strong className="text-fg">{current.peerName}</strong>」的文件{' '}
              <strong className="text-fg">{current.fileName}</strong>，是否把最新修改上传存回原位置？
            </p>
          )}

          {!current.conflict && (
            <label className="flex cursor-pointer items-center gap-2.5 rounded-md border border-line p-2.5 hover:bg-hover">
              <input
                type="checkbox"
                checked={autoSync}
                onChange={(e) => setAutoSync(e.target.checked)}
                className="size-4 accent-[var(--color-accent)]"
              />
              <span className="text-xs">本次打开期间后续保存自动存回，不再每次询问</span>
            </label>
          )}

          <div className="flex items-center justify-end gap-2">
            <Button data-autofocus={current.conflict ? true : undefined} onClick={shift} disabled={busy}>
              {current.conflict ? '保留对方版本（不回传）' : '暂不存回'}
            </Button>
            {current.conflict ? (
              <Button variant="danger" onClick={() => void confirmUpload(true)} disabled={busy}>
                仍用本机版本覆盖对方
              </Button>
            ) : (
              <Button
                data-autofocus
                variant="primary"
                onClick={() => void confirmUpload(false)}
                disabled={busy}
              >
                存回对方电脑
              </Button>
            )}
          </div>
        </div>
      )}
    </Modal>
  )
}
