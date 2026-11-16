import type { Device } from '../api/types'

export function StatusDot({ device }: { device: Device }) {
  const s = device.status
  const cls =
    s.state === 'online'
      ? s.link === 'relay'
        ? 'bg-warn'
        : 'bg-ok'
      : s.state === 'connecting'
        ? 'bg-accent pulse'
        : 'bg-faint'
  return <span aria-hidden="true" className={`inline-block size-2 shrink-0 rounded-full ${cls}`} />
}
