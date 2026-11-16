import type { Device } from '../api/types'
import { formatAgo } from './format'

/** 设备状态的一句话描述：直连 · 3 毫秒 / 中转 · 46 毫秒 / 重连中… / 离线 · 3 天前在线 */
export function statusText(d: Device): string {
  const s = d.status
  if (s.state === 'online') {
    if (s.link === 'local') return '本机'
    return `${s.link === 'direct' ? '直连' : '中转'} · ${s.rttMs} 毫秒`
  }
  if (s.state === 'connecting') return '重连中…'
  return s.lastSeen ? `离线 · ${formatAgo(s.lastSeen)}在线` : '离线'
}
