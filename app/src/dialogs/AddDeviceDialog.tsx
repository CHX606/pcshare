import { useEffect, useState } from 'react'
import { backend } from '../api'
import { errorCode, humanError } from '../api/errors'
import type { PairOffer } from '../api/types'
import { Icon } from '../components/Icon'
import { Button, Modal } from '../components/Modal'
import { shortId } from '../lib/format'
import { useDevices } from '../store/devices'
import { toast } from '../store/toasts'
import { useUi } from '../store/ui'

type Mode = 'connect' | 'host'

export function AddDeviceDialog() {
  const open = useUi((s) => s.addDeviceOpen)
  const setOpen = useUi((s) => s.openAddDevice)

  const close = () => {
    setOpen(false)
    void backend.pairCancel()
  }

  return (
    <Modal open={open} onClose={close} title="添加设备" width={540}>
      <AddDeviceBody onClose={close} />
    </Modal>
  )
}

function AddDeviceBody({ onClose }: { onClose: () => void }) {
  const [mode, setMode] = useState<Mode>('connect')
  useEffect(() => {
    void useDevices.getState().refreshLan()
  }, [])

  return (
    <>
      <div role="tablist" aria-label="添加方式" className="mb-4 grid grid-cols-2 gap-1 rounded-lg bg-hover p-1 text-sm">
        {(
          [
            ['connect', '连接到别的电脑'],
            ['host', '让别的电脑连这台'],
          ] as const
        ).map(([m, label]) => (
          <button
            key={m}
            type="button"
            role="tab"
            aria-selected={mode === m}
            onClick={() => setMode(m)}
            className={`rounded-md py-1.5 ${mode === m ? 'bg-panel font-medium shadow-sm' : 'text-muted hover:text-fg'}`}
          >
            {label}
          </button>
        ))}
      </div>
      {mode === 'connect' ? <ConnectForm onDone={onClose} /> : <HostPanel />}
    </>
  )
}

function ConnectForm({ onDone }: { onDone: () => void }) {
  const lanPeers = useDevices((s) => s.lanPeers)
  const [target, setTarget] = useState('')
  const [code, setCode] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const isInvite = /^pcshare:/i.test(target.trim())

  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      const d = await backend.pairConnect(target.trim(), code.trim())
      useDevices.getState().upsert(d)
      useDevices.getState().select(d.id)
      toast.success(`已和“${d.name}”配对`)
      onDone()
    } catch (err) {
      setError(humanError(err))
      if (errorCode(err) === 'expired') setCode('')
    } finally {
      setBusy(false)
    }
  }

  return (
    <form onSubmit={submit} className="flex flex-col gap-3">
      {lanPeers.length > 0 && (
        <div className="rounded-lg border border-line bg-hover/50 p-2.5 text-xs">
          <div className="mb-1.5 flex items-center gap-1.5 font-medium text-fg">
            <Icon name="wifi" size={14} className="text-accent" />
            同一局域网自动发现的电脑（点一下填入身份码，仍需输入配对码确认）
          </div>
          <div className="flex flex-wrap gap-1.5">
            {lanPeers.map((p) => (
              <button
                key={p.endpointId}
                type="button"
                onClick={() => setTarget(p.endpointId)}
                className={`flex items-center gap-1.5 rounded-md border px-2.5 py-1 text-left ${
                  target.trim() === p.endpointId
                    ? 'border-accent bg-accent-soft text-fg'
                    : 'border-line bg-panel hover:bg-hover'
                }`}
              >
                <Icon name="monitor" size={13} className="text-accent" />
                <span className="font-medium">{p.deviceName}</span>
                <span className="font-mono text-[11px] text-muted">{p.endpointId.slice(-6)}</span>
              </button>
            ))}
          </div>
        </div>
      )}

      <p className="text-sm leading-relaxed text-muted">
        先在要连接的电脑上打开“添加设备 → 让别的电脑连这台”，把它显示的身份码和配对码填到这里，也可以直接粘贴整串邀请码。
      </p>
      <label className="flex flex-col gap-1 text-sm">
        对方的身份码或邀请码
        <textarea
          required
          rows={2}
          autoFocus
          data-autofocus
          spellCheck={false}
          value={target}
          onChange={(e) => setTarget(e.target.value)}
          placeholder="64 位字母和数字，或 pcshare: 开头的邀请码"
          className="resize-none rounded-md border border-line bg-panel px-2.5 py-1.5 font-mono text-[13px] outline-none focus:border-accent"
        />
      </label>
      {!isInvite && (
        <label className="flex flex-col gap-1 text-sm">
          配对码
          <input
            required
            inputMode="numeric"
            pattern="\d{6}"
            maxLength={6}
            autoComplete="one-time-code"
            value={code}
            onChange={(e) => setCode(e.target.value.replace(/\D/g, ''))}
            placeholder="6 位数字"
            className="h-9 w-40 rounded-md border border-line bg-panel px-2.5 font-mono tracking-[0.3em] outline-none focus:border-accent"
          />
        </label>
      )}
      <p role="alert" className={`min-h-5 text-sm text-bad ${error ? '' : 'invisible'}`}>
        {error}
      </p>
      <div className="flex justify-end gap-2">
        <Button onClick={onDone}>取消</Button>
        <Button type="submit" variant="primary" disabled={busy}>
          {busy && <Icon name="loader" className="spin" />}
          {busy ? '正在连接…' : '连接并配对'}
        </Button>
      </div>
    </form>
  )
}

function HostPanel() {
  const self = useDevices((s) => s.self)
  const [offer, setOffer] = useState<PairOffer | null>(null)
  const [now, setNow] = useState(() => Date.now())
  const [error, setError] = useState<string | null>(null)

  const generate = async () => {
    try {
      const next = await backend.pairBegin()
      setOffer(next)
      setNow(Date.now())
      setError(null)
    } catch (e) {
      setError(humanError(e))
    }
  }

  useEffect(() => {
    let alive = true
    backend.pairBegin().then(
      (o) => alive && setOffer(o),
      (e) => alive && setError(humanError(e)),
    )
    const t = window.setInterval(() => setNow(Date.now()), 1000)
    return () => {
      alive = false
      window.clearInterval(t)
    }
  }, [])

  const left = offer ? Math.max(0, Math.round((offer.expiresAt - now) / 1000)) : 0
  const expired = !!offer && left === 0
  const copy = async (text: string, what: string) => {
    await navigator.clipboard.writeText(text)
    toast.info(`已复制${what}`)
  }

  return (
    <div className="flex flex-col gap-4 text-sm">
      <p className="leading-relaxed text-muted">
        在另一台电脑上选“连接到别的电脑”，填入下面的身份码和配对码。对方连上来时，这里会弹窗让你确认。
      </p>
      <div>
        <div className="mb-1 flex items-center justify-between text-xs text-muted">
          本机身份码
          {self && (
            <button type="button" className="text-accent hover:underline" onClick={() => void copy(self.id, '身份码')}>
              复制
            </button>
          )}
        </div>
        <code className="selectable block rounded-md bg-hover px-3 py-2 font-mono text-[12.5px] break-all">{self?.id ?? '…'}</code>
      </div>
      <div className="flex items-end gap-6">
        <div>
          <div className="mb-1 text-xs text-muted">配对码</div>
          <div className={`selectable font-mono text-3xl tracking-[0.25em] ${expired ? 'text-faint line-through' : ''}`}>{offer?.code ?? '······'}</div>
        </div>
        <div className="pb-1 text-xs text-muted">
          {expired ? '已过期' : offer ? `${Math.floor(left / 60)}:${String(left % 60).padStart(2, '0')} 后失效` : ''}
        </div>
        <div className="ml-auto flex gap-2 pb-0.5">
          {offer && !expired && <Button onClick={() => void copy(offer.invite, '邀请码，可以整串发给对方')}>复制邀请码</Button>}
          <Button onClick={() => void generate()}>
            <Icon name="refresh" /> 换一个
          </Button>
        </div>
      </div>
      {error && <p className="text-bad">{error}</p>}
      <p className="text-xs leading-relaxed text-faint">配对码 5 分钟内有效，输错 5 次立即作废。身份码可以公开，配对码只告诉你信任的人。{self && `（身份码末 8 位：${shortId(self.id).slice(-8)}）`}</p>
    </div>
  )
}
