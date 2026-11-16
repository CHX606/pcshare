import { useEffect, useState } from 'react'
import { backend } from '../api'
import { humanError } from '../api/errors'
import { DEFAULT_PERMISSIONS, type Device, type Permissions, type Settings } from '../api/types'
import { Button, Modal } from '../components/Modal'
import { useDevices } from '../store/devices'
import { toast } from '../store/toasts'
import { confirmDialog, useUi } from '../store/ui'
import { PermissionsEditor } from './PermissionsEditor'

const closeEdit = () => useUi.getState().openEditDevice(null)

export function EditDeviceDialog() {
  const id = useUi((s) => s.editDevice)
  const dev = useDevices((s) => s.devices.find((d) => d.id === id))
  return (
    <Modal open={!!dev} onClose={closeEdit} title="设备设置" width={480}>
      {dev && <EditDeviceForm key={dev.id} dev={dev} />}
    </Modal>
  )
}

function EditDeviceForm({ dev }: { dev: Device }) {
  const [name, setName] = useState(dev.name)
  const [perms, setPerms] = useState<Permissions>(dev.permissions ?? DEFAULT_PERMISSIONS)
  const [busy, setBusy] = useState(false)

  const save = async (e: React.FormEvent) => {
    e.preventDefault()
    setBusy(true)
    try {
      if (name.trim() && name.trim() !== dev.name) await backend.renameDevice(dev.id, name.trim())
      await backend.setPermissions(dev.id, perms)
      toast.success('已保存')
      closeEdit()
    } catch (err) {
      toast.error(humanError(err))
    } finally {
      setBusy(false)
    }
  }

  const remove = async () => {
    const ok = await confirmDialog({
      title: `移除“${dev.name}”？`,
      body: '移除后双方都不能再互相操作，以后要用需要重新配对。',
      okText: '移除',
      danger: true,
    })
    if (!ok) return
    try {
      await backend.removeDevice(dev.id)
      closeEdit()
    } catch (err) {
      toast.error(humanError(err))
    }
  }

  return (
    <form onSubmit={save} className="flex flex-col gap-4 text-sm">
      <label className="flex flex-col gap-1">
        名称
        <input
          required
          value={name}
          maxLength={40}
          onChange={(e) => setName(e.target.value)}
          className="h-9 rounded-md border border-line bg-panel px-2.5 outline-none focus:border-accent"
        />
      </label>
      <div>
        <div className="mb-1 text-muted">身份码</div>
        <code className="selectable block rounded-md bg-hover px-3 py-2 font-mono text-[12px] break-all">{dev.id}</code>
      </div>
      <div>
        <div className="mb-1.5 text-muted">允许它对这台电脑做什么</div>
        <PermissionsEditor value={perms} onChange={setPerms} />
      </div>
      <div className="flex items-center gap-2">
        <Button variant="ghost" className="text-bad" onClick={() => void remove()}>
          移除这台设备
        </Button>
        <Button className="ml-auto" onClick={closeEdit}>
          取消
        </Button>
        <Button type="submit" variant="primary" disabled={busy}>
          保存
        </Button>
      </div>
    </form>
  )
}

const closeSettings = () => useUi.getState().openSettings(false)

export function SettingsDialog() {
  const open = useUi((s) => s.settingsOpen)
  return (
    <Modal open={open} onClose={closeSettings} title="设置" width={500}>
      <SettingsForm />
    </Modal>
  )
}

function SettingsForm() {
  const self = useDevices((s) => s.self)
  const hostPaused = useDevices((s) => s.hostPaused)
  const setHostPaused = useDevices((s) => s.setHostPaused)
  const [settings, setSettings] = useState<Settings | null>(null)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    backend.getSettings().then(setSettings, (e) => toast.error(humanError(e)))
  }, [])

  const save = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!settings) return
    setBusy(true)
    const deviceName = settings.deviceName.trim()
    try {
      await backend.setSettings({
        ...settings,
        deviceName,
        autostartPrompted: true,
      })
      useDevices.setState((s) => ({ self: s.self && { ...s.self, name: deviceName } }))
      toast.success('设置已保存')
      closeSettings()
    } catch (err) {
      toast.error(humanError(err))
    } finally {
      setBusy(false)
    }
  }

  if (!settings) return <p className="py-6 text-center text-sm text-muted">正在读取…</p>

  return (
    <form onSubmit={save} className="flex flex-col gap-3.5 text-sm">
      <label className="flex flex-col gap-1">
        本机名称（别的电脑上显示的名字）
        <input
          required
          maxLength={40}
          value={settings.deviceName}
          onChange={(e) => setSettings({ ...settings, deviceName: e.target.value })}
          className="h-9 rounded-md border border-line bg-panel px-2.5 outline-none focus:border-accent"
        />
      </label>
      <div>
        <div className="mb-1 text-muted">本机身份码</div>
        <code className="selectable block rounded-md bg-hover px-3 py-2 font-mono text-[12px] break-all">{self?.id}</code>
      </div>
      <label className="flex cursor-pointer items-start gap-3 rounded-lg border border-line p-3 hover:bg-hover">
        <input
          type="checkbox"
          checked={!!settings.autostart || settings.autostartElevated}
          onChange={(e) =>
            setSettings({
              ...settings,
              autostart: e.target.checked,
              autostartElevated: e.target.checked ? settings.autostartElevated : false,
            })
          }
          className="mt-0.5 size-4 accent-[var(--color-accent)]"
        />
        <span>
          随登录在后台静默启动（推荐）
          <span className="mt-0.5 block text-xs leading-relaxed text-muted">
            开机登录后自动缩在右下角托盘待命，不打开窗口、不启动浏览器内核（常驻内存 &lt; 50 MB）。关闭窗口也会继续留在托盘。
          </span>
        </span>
      </label>
      <label className="flex cursor-pointer items-start gap-3 rounded-lg border border-line p-3 hover:bg-hover">
        <input
          type="checkbox"
          checked={settings.autostartElevated}
          onChange={(e) =>
            setSettings({
              ...settings,
              autostartElevated: e.target.checked,
              autostart: e.target.checked ? false : settings.autostart,
            })
          }
          className="mt-0.5 size-4 accent-[var(--color-accent)]"
        />
        <span>
          以管理员权限随登录启动
          <span className="mt-0.5 block text-xs leading-relaxed text-muted">
            打开后，别的电脑在这台上启动需要管理员权限的软件时不会弹确认框卡住。请只在信任所有已配对设备时开启。
          </span>
        </span>
      </label>
      <label className="flex flex-col gap-1">
        全局快捷搜索热键（任务 3.1，留空表示关闭；若被占用保存时会提示更换）
        <input
          maxLength={32}
          value={settings.quickHotkey ?? 'Alt+Space'}
          placeholder="例如 Alt+Space 或 Ctrl+Alt+Space"
          onChange={(e) => setSettings({ ...settings, quickHotkey: e.target.value })}
          className="h-9 rounded-md border border-line bg-panel px-2.5 font-mono text-xs outline-none focus:border-accent"
        />
      </label>
      <label className="flex cursor-pointer items-center justify-between rounded-lg border border-line p-3 hover:bg-hover">
        <span>
          剪贴板自动同步（默认关闭）
          <span className="block text-xs text-muted">开启后，本机复制文字或图片时会自动发给当前在线且已授权剪贴板的电脑</span>
        </span>
        <input
          type="checkbox"
          checked={!!settings.clipboardAutoSync}
          onChange={(e) => setSettings({ ...settings, clipboardAutoSync: e.target.checked })}
          className="size-4 accent-[var(--color-accent)]"
        />
      </label>
      <label className="flex cursor-pointer items-center justify-between rounded-lg border border-line p-3 hover:bg-hover">
        <span>
          临时暂停被控
          <span className="block text-xs text-muted">暂停期间，已配对的电脑也无法读取或操作这台电脑（同托盘菜单“暂停被控”）</span>
        </span>
        <input
          type="checkbox"
          checked={hostPaused}
          onChange={(e) => void setHostPaused(e.target.checked)}
          className="size-4 accent-[var(--color-accent)]"
        />
      </label>
      <div className="flex items-center gap-2">
        <span className="text-xs text-muted">版本 {self?.version}</span>
        <Button className="ml-auto" onClick={closeSettings}>
          取消
        </Button>
        <Button type="submit" variant="primary" disabled={busy}>
          保存
        </Button>
      </div>
    </form>
  )
}
