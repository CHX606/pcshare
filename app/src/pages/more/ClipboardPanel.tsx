import { useCallback, useEffect, useRef, useState } from 'react'
import { backend } from '../../api'
import { humanError } from '../../api/errors'
import { SELF_ID, type ClipboardPreview, type DeviceId } from '../../api/types'
import { Icon } from '../../components/Icon'
import { Button } from '../../components/Modal'
import { toast } from '../../store/toasts'

let lastAnnouncedCopyMsg = ''

/** 任务 3.7：剪贴板互通（手动把本机剪贴板发到对方 / 把对方的取过来，支持文字与图片，并提供自动同步开关）。 */
export function ClipboardPanel({ device, deviceName }: { device: DeviceId; deviceName: string }) {
  const [preview, setPreview] = useState<ClipboardPreview | null>(null)
  const [autoSync, setAutoSync] = useState(false)
  const [busy, setBusy] = useState(false)
  const [copiedState, setCopiedState] = useState<'idle' | 'ok' | 'fail'>('idle')
  const [copyStatusText, setCopyStatusText] = useState('')
  const copyTimer = useRef<number | null>(null)

  const loadSettingsAndPeek = useCallback(async () => {
    try {
      const s = await backend.getSettings()
      setAutoSync(!!s.clipboardAutoSync)
    } catch {
      // Ignore
    }
    try {
      const p = await backend.clipPeek(device)
      setPreview(p)
    } catch {
      // Might not have clipboard permission yet
      setPreview(null)
    }
  }, [device])

  useEffect(() => {
    let cancelled = false
    void backend
      .getSettings()
      .then((s) => {
        if (!cancelled) setAutoSync(!!s.clipboardAutoSync)
      })
      .catch(() => {})
    void backend
      .clipPeek(device)
      .then((p) => {
        if (!cancelled) setPreview(p)
      })
      .catch(() => {
        if (!cancelled) setPreview(null)
      })
    return () => {
      cancelled = true
      if (copyTimer.current) window.clearTimeout(copyTimer.current)
    }
  }, [device])

  const pushToPeer = async () => {
    setBusy(true)
    try {
      const kind = await backend.clipPush(device)
      toast.success(
        `已把本机剪贴板${kind === 'image_png' ? '图片' : '文字'}发送到「${deviceName}」`,
      )
      const p = await backend.clipPeek(device).catch(() => null)
      if (p) setPreview(p)
    } catch (err) {
      toast.error(humanError(err))
    } finally {
      setBusy(false)
    }
  }

  const pullFromPeer = async () => {
    setBusy(true)
    try {
      const pulled = await backend.clipPull(device)
      setPreview(pulled)
      toast.success(
        `已把「${deviceName}」的剪贴板${pulled.kind === 'image_png' ? '图片' : '文字'}取回本机，可直接粘贴`,
      )
    } catch (err) {
      toast.error(humanError(err))
    } finally {
      setBusy(false)
    }
  }

  const toggleAutoSync = async (checked: boolean) => {
    setAutoSync(checked)
    try {
      const cur = await backend.getSettings()
      await backend.setSettings({ ...cur, clipboardAutoSync: checked })
      toast.info(checked ? '已开启剪贴板自动同步' : '已关闭剪贴板自动同步')
    } catch (err) {
      setAutoSync(!checked)
      toast.error(humanError(err))
    }
  }

  // 遵循 copy-button 无障碍规范：不禁用按钮、切换对勾图标、气泡位于按钮外并用 opacity 控制显隐
  const copyPreviewText = async () => {
    if (!preview?.text) return
    if (copyTimer.current) window.clearTimeout(copyTimer.current)
    let ok = true
    try {
      await navigator.clipboard.writeText(preview.text)
    } catch {
      ok = false
    }
    let msg = ok ? '已复制' : '复制失败'
    if (msg === lastAnnouncedCopyMsg) msg += '\u00A0'
    lastAnnouncedCopyMsg = msg
    setCopiedState(ok ? 'ok' : 'fail')
    setCopyStatusText(msg)
    copyTimer.current = window.setTimeout(() => {
      setCopiedState('idle')
      setCopyStatusText('')
    }, 2000)
  }

  if (device === SELF_ID) {
    return (
      <div className="rounded-lg border border-line bg-bg/40 p-5 text-sm text-muted">
        请在左侧选择一台已配对的远端电脑，即可把本机复制的文字或截图发过去，或把对方的剪贴板取回本机。
      </div>
    )
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <button
          type="button"
          disabled={busy}
          onClick={() => void pushToPeer()}
          className="flex items-start gap-3 rounded-lg border border-line bg-panel p-4 text-left hover:bg-hover disabled:opacity-50"
        >
          <Icon name="upload" size={20} className="mt-0.5 shrink-0 text-accent" />
          <span>
            <span className="block font-medium">把本机剪贴板发到「{deviceName}」</span>
            <span className="mt-1 block text-xs leading-relaxed text-muted">
              支持普通文字与截图/复制的图片（PNG / 位图），发过去后对方可直接 Ctrl+V 粘贴。
            </span>
          </span>
        </button>

        <button
          type="button"
          disabled={busy}
          onClick={() => void pullFromPeer()}
          className="flex items-start gap-3 rounded-lg border border-line bg-panel p-4 text-left hover:bg-hover disabled:opacity-50"
        >
          <Icon name="download" size={20} className="mt-0.5 shrink-0 text-accent" />
          <span>
            <span className="block font-medium">把「{deviceName}」的剪贴板取过来</span>
            <span className="mt-1 block text-xs leading-relaxed text-muted">
              读取对方当前复制的文字或图片并写入本机剪贴板，取回后本机可直接粘贴。
            </span>
          </span>
        </button>
      </div>

      <label className="flex cursor-pointer items-center justify-between rounded-lg border border-line bg-panel px-4 py-3 hover:bg-hover">
        <span className="text-sm">
          自动同步剪贴板（默认关闭）
          <span className="mt-0.5 block text-xs text-muted">
            开启后，本机每次复制新文字或图片时，会自动推送给在线且开放剪贴板权限的电脑
          </span>
        </span>
        <input
          type="checkbox"
          checked={autoSync}
          onChange={(e) => void toggleAutoSync(e.target.checked)}
          className="size-4 accent-[var(--color-accent)]"
        />
      </label>

      <div className="rounded-lg border border-line bg-panel p-4">
        <div className="mb-2 flex items-center justify-between">
          <h3 className="text-sm font-medium">剪贴板内容预览</h3>
          <div className="flex items-center gap-2">
            {preview?.kind === 'text' && preview.text && (
              <span className="inline-flex items-center gap-1.5">
                <button
                  type="button"
                  aria-label="复制预览文字到本机剪贴板"
                  onClick={() => void copyPreviewText()}
                  className="inline-flex min-h-6 min-w-6 items-center gap-1 rounded border border-line px-2 py-1 text-xs hover:bg-hover"
                >
                  <Icon name={copiedState === 'ok' ? 'check' : 'copy'} size={13} />
                  <span>复制文字</span>
                </button>
                <span
                  role="status"
                  data-visible={copiedState !== 'idle' ? 'true' : 'false'}
                  className="copy-status-tip text-xs text-ok"
                >
                  {copyStatusText}
                </span>
              </span>
            )}
            <Button onClick={() => void loadSettingsAndPeek()} disabled={busy}>
              <Icon name="refresh" size={13} />
              读取对方剪贴板
            </Button>
          </div>
        </div>

        {!preview ? (
          <p className="text-xs text-muted">
            尚未读取（请确认对方已在设备设置中开启“剪贴板”权限，然后点击右上角读取或直接收发）。
          </p>
        ) : preview.kind === 'empty' ? (
          <p className="text-xs text-muted">对方剪贴板当前为空。</p>
        ) : preview.kind === 'text' ? (
          <pre
            tabIndex={0}
            role="region"
            aria-label="剪贴板文字内容"
            className="selectable scroll-thin max-h-48 overflow-auto rounded bg-bg p-3 font-mono text-xs whitespace-pre-wrap"
          >
            <code>{preview.text}</code>
          </pre>
        ) : (
          <div className="flex flex-col items-start gap-2">
            {preview.imageDataUrl && (
              <img
                src={preview.imageDataUrl}
                alt="剪贴板图片预览"
                width={preview.width ?? 320}
                height={preview.height ?? 180}
                className="max-h-60 w-auto rounded border border-line object-contain"
              />
            )}
            <span className="text-xs text-muted">
              图片尺寸：{preview.width ?? '?'} × {preview.height ?? '?'} PNG
            </span>
          </div>
        )}
      </div>
    </div>
  )
}
