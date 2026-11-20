import { useState } from 'react'
import { backend } from '../../api'
import { humanError } from '../../api/errors'
import { SELF_ID, type Device, type PowerActionKind } from '../../api/types'
import { Icon } from '../../components/Icon'
import { Button } from '../../components/Modal'
import { isOnline } from '../../store/devices'
import { toast } from '../../store/toasts'
import { confirmDialog } from '../../store/ui'

const ACTIONS: Array<{
  id: PowerActionKind
  title: string
  desc: string
  countdown: boolean
  danger?: boolean
}> = [
  { id: 'lock', title: '立即锁屏', desc: '锁定对方屏幕回到密码登录界面，正在运行的软件不会关闭', countdown: false },
  { id: 'sleep', title: '进入睡眠（10 秒可取消）', desc: '对方屏幕先弹出 10 秒倒计时提示，无人取消后进入低功耗睡眠', countdown: true },
  { id: 'restart', title: '重启电脑（10 秒可取消）', desc: '对方屏幕弹出 10 秒可取消倒计时，请先确认文档已保存', countdown: true, danger: true },
  { id: 'shutdown', title: '关闭电脑（10 秒可取消）', desc: '对方屏幕弹出 10 秒可取消倒计时，支持后续通过网络唤醒开机', countdown: true, danger: true },
]

/** 任务 3.5：电源与网络唤醒（关机/重启/睡眠前对方屏幕 10 秒可取消倒计时；支持本机 + 同网段在线设备代发网络唤醒包）。 */
export function PowerPanel({ dev }: { dev: Device | undefined }) {
  const [countingAction, setCountingAction] = useState<PowerActionKind | null>(null)
  const [waking, setWaking] = useState(false)

  if (!dev || dev.id === SELF_ID) {
    return (
      <div className="rounded-lg border border-line bg-bg/40 p-5 text-sm text-muted">
        请在左侧选择一台已配对的远端电脑，即可执行锁屏、睡眠、重启、关机或网络唤醒。
      </div>
    )
  }

  const online = isOnline(dev)

  const triggerPower = async (action: PowerActionKind, title: string, danger?: boolean) => {
    const ok = await confirmDialog({
      title: `对「${dev.name}」执行${title}？`,
      body:
        action === 'lock'
          ? '对方电脑将立即锁屏。'
          : '发送后，对方电脑屏幕中央会弹出 10 秒置顶倒计时窗口；如果那边正好有人在用，可随时点击取消，你也可以在下方取消。',
      okText: title,
      danger,
    })
    if (!ok) return

    setCountingAction(action)
    try {
      const res = await backend.powerExec(dev.id, action, action === 'lock' ? 0 : 10)
      if (res.cancelled) {
        toast.warn(`「${dev.name}」上的电源操作已在倒计时期间被取消`)
      } else {
        toast.success(res.message ?? `已对「${dev.name}」执行${title}`)
      }
    } catch (err) {
      toast.error(humanError(err))
    } finally {
      setCountingAction(null)
    }
  }

  const cancelRemoteCountdown = async () => {
    try {
      await backend.powerCancel(dev.id)
      toast.info('已发送取消倒计时请求')
    } catch (err) {
      toast.error(humanError(err))
    }
  }

  const wakeRemote = async () => {
    setWaking(true)
    try {
      const rep = await backend.powerWake(dev.id)
      toast.success(
        `已发送网络唤醒包（本机发出 ${rep.localPackets} 个，通知 ${rep.relayPeersNotified} 台在线设备代发 ${rep.relayedPackets} 个）`,
      )
    } catch (err) {
      toast.error(humanError(err))
    } finally {
      setWaking(false)
    }
  }

  return (
    <div className="flex flex-col gap-4">
      {/* 网络唤醒卡片（离线睡眠时依然可用！） */}
      <div className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-line bg-panel p-4">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2 font-medium">
            <Icon name="wifi" size={18} className="text-accent" />
            <span>网络唤醒（Wake-on-LAN）</span>
            <span
              className={`rounded px-1.5 py-0.5 text-[11px] ${
                dev.hasMac ? 'bg-ok/15 text-ok' : 'bg-hover text-muted'
              }`}
            >
              {dev.hasMac ? '已记录网卡物理地址' : '连接一次后自动记录网卡地址'}
            </span>
          </div>
          <p className="mt-1 text-xs leading-relaxed text-muted">
            当「{dev.name}」处于睡眠或关机状态时，本机会广播魔术唤醒包，并同时让其他在线伙伴电脑在同网段代发唤醒包（需目标主板网卡开启唤醒支持）。
          </p>
        </div>
        <Button variant="primary" onClick={() => void wakeRemote()} disabled={waking}>
          <Icon name="power" size={14} />
          {waking ? '正在发送…' : '发送唤醒包'}
        </Button>
      </div>

      {countingAction && countingAction !== 'lock' && (
        <div
          role="status"
          className="flex items-center justify-between gap-3 rounded-lg border border-warn/50 bg-warn/10 px-4 py-3 text-sm"
        >
          <span className="flex items-center gap-2">
            <Icon name="loader" className="spin text-warn" />
            正在等待「{dev.name}」屏幕上的 10 秒安全倒计时结束…
          </span>
          <Button onClick={() => void cancelRemoteCountdown()}>立即取消本次操作</Button>
        </div>
      )}

      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        {ACTIONS.map((a) => (
          <button
            key={a.id}
            type="button"
            disabled={!online || !!countingAction}
            onClick={() => void triggerPower(a.id, a.title, a.danger)}
            className="flex items-start gap-3 rounded-lg border border-line bg-panel p-4 text-left hover:bg-hover disabled:opacity-45"
          >
            <Icon
              name="power"
              size={20}
              className={`mt-0.5 shrink-0 ${a.danger ? 'text-bad' : 'text-accent'}`}
            />
            <span>
              <span className="block font-medium">{a.title}</span>
              <span className="mt-1 block text-xs leading-relaxed text-muted">{a.desc}</span>
            </span>
          </button>
        ))}
      </div>

      {!online && (
        <p className="text-xs text-muted">
          当前「{dev.name}」处于离线状态：关机/重启/锁屏需在线时使用，上方“网络唤醒”可随时尝试唤醒它。
        </p>
      )}
    </div>
  )
}
