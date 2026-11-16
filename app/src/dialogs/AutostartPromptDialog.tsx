import { useEffect, useState } from 'react'
import { backend } from '../api'
import { humanError } from '../api/errors'
import type { Settings } from '../api/types'
import { Button, Modal } from '../components/Modal'
import { toast } from '../store/toasts'

/**
 * 任务 2.7：首次运行询问用户是否随登录后台自启。
 * 只在 settings.autostartPrompted 为 false 且尚未开启自启时弹出一次。
 */
export function AutostartPromptDialog() {
  const [pending, setPending] = useState<Settings | null>(null)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    let alive = true
    backend.getSettings().then(
      (s) => {
        if (alive && !s.autostartPrompted && !s.autostart && !s.autostartElevated) {
          setPending(s)
        }
      },
      () => {},
    )
    return () => {
      alive = false
    }
  }, [])

  const choose = async (enableAutostart: boolean) => {
    if (!pending) return
    setBusy(true)
    try {
      await backend.setSettings({
        ...pending,
        autostart: enableAutostart,
        autostartPrompted: true,
      })
      if (enableAutostart) {
        toast.success('已开启随登录后台启动，平时不弹窗，可随时在设置或托盘更改')
      }
      setPending(null)
    } catch (e) {
      toast.error(humanError(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Modal open={!!pending} onClose={() => void choose(false)} title="需要开机后在后台待命吗？" width={460}>
      <div className="flex flex-col gap-4 text-sm">
        <p className="leading-relaxed text-muted">
          开启后，电脑登录系统时会在右下角托盘静默运行（不弹窗口、不启动浏览器内核，后台内存低于 50 MB），方便你随时从另一台已配对的电脑连过来取文件。
        </p>
        <p className="text-xs text-faint">以后随时可以在左下角“设置”里开关。</p>
        <div className="flex justify-end gap-2">
          <Button disabled={busy} onClick={() => void choose(false)}>
            暂不开启
          </Button>
          <Button variant="primary" data-autofocus disabled={busy} onClick={() => void choose(true)}>
            随登录后台启动
          </Button>
        </div>
      </div>
    </Modal>
  )
}
