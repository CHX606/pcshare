import { useState } from 'react'
import { backend } from '../api'
import { humanError } from '../api/errors'
import { DEFAULT_PERMISSIONS, type Permissions } from '../api/types'
import { useDevices } from '../store/devices'
import { toast } from '../store/toasts'
import { useUi } from '../store/ui'
import { Button, Modal } from '../components/Modal'
import { PermissionsEditor } from './PermissionsEditor'

/** 有设备请求配对：必须由坐在这台电脑前的人允许，并选择权限档。 */
export function PairRequestDialog() {
  const req = useDevices((s) => s.pairRequests[0])
  const shift = useDevices((s) => s.shiftPairRequest)
  const [perms, setPerms] = useState<Permissions>(DEFAULT_PERMISSIONS)
  const [busy, setBusy] = useState(false)

  const respond = async (allow: boolean) => {
    if (!req) return
    setBusy(true)
    try {
      await backend.pairRespond(req.requestId, allow, perms)
      if (allow) {
        toast.success(`已允许“${req.deviceName}”配对`)
        useUi.getState().openAddDevice(false)
      } else toast.info(`已拒绝“${req.deviceName}”的配对请求`)
    } catch (e) {
      toast.error(humanError(e))
    } finally {
      setBusy(false)
      setPerms(DEFAULT_PERMISSIONS)
      shift()
    }
  }

  return (
    <Modal open={!!req} onClose={() => void respond(false)} title="有设备请求配对" width={480}>
      {req && (
        <div className="flex flex-col gap-4 text-sm">
          <div className="rounded-lg bg-hover p-3">
            <div className="text-base font-medium">{req.deviceName}</div>
            <div className="mt-1 text-xs text-muted">
              身份码末 8 位：<code className="font-mono text-fg">{req.deviceId.slice(-8)}</code>
              ，请和对方屏幕上显示的核对一致
            </div>
          </div>
          <div>
            <div className="mb-1.5 text-muted">允许它对这台电脑做什么</div>
            <PermissionsEditor value={perms} onChange={setPerms} />
          </div>
          <div className="flex justify-end gap-2">
            <Button data-autofocus disabled={busy} onClick={() => void respond(false)}>
              拒绝
            </Button>
            <Button variant="primary" disabled={busy} onClick={() => void respond(true)}>
              允许
            </Button>
          </div>
        </div>
      )}
    </Modal>
  )
}
