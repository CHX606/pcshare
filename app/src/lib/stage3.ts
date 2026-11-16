import type { ShotTransform } from '../api/types'

export interface RenderedBox {
  left: number
  top: number
  width: number
  height: number
}

/**
 * 将网页中渲染后的截图 `<img />` 点击位置 `(clientX, clientY)` 还原为远端真实虚拟桌面坐标（任务 3.3）。
 * 兼容多显示器负坐标、DPI 缩放以及 `max_width` 等比压缩后的图像尺寸。
 */
export function mapClientPointToRemoteScreen(
  clientX: number,
  clientY: number,
  rect: RenderedBox,
  transform: Pick<
    ShotTransform,
    'originX' | 'originY' | 'sourceWidth' | 'sourceHeight' | 'imageWidth' | 'imageHeight'
  >,
): { screenX: number; screenY: number } {
  const safeW = Math.max(1, rect.width)
  const safeH = Math.max(1, rect.height)
  const ratioX = Math.max(0, Math.min(1, (clientX - rect.left) / safeW))
  const ratioY = Math.max(0, Math.min(1, (clientY - rect.top) / safeH))

  const imgX = ratioX * transform.imageWidth
  const imgY = ratioY * transform.imageHeight
  const iw = Math.max(1, transform.imageWidth)
  const ih = Math.max(1, transform.imageHeight)

  const screenX = Math.round(transform.originX + (imgX * transform.sourceWidth) / iw)
  const screenY = Math.round(transform.originY + (imgY * transform.sourceHeight) / ih)
  return { screenX, screenY }
}

export interface QuickDeviceLite {
  id: string
  name: string
  online: boolean
}

export function parseQuickInputHint(
  raw: string,
  devices: QuickDeviceLite[],
): {
  scopedDeviceId?: string
  scopedDeviceName?: string
  defaultSingleOnlineName?: string
  keyword: string
} {
  const trimmed = raw.trim()
  const spaceIdx = trimmed.search(/\s/)
  if (spaceIdx > 0) {
    const prefix = trimmed.slice(0, spaceIdx).toLowerCase()
    const rest = trimmed.slice(spaceIdx).trim()
    const hit = devices.find((d) => d.name.toLowerCase().includes(prefix))
    if (hit) {
      return {
        scopedDeviceId: hit.id,
        scopedDeviceName: hit.name,
        keyword: rest,
      }
    }
  }
  const onlineList = devices.filter((d) => d.online && d.id !== 'self')
  return {
    defaultSingleOnlineName: onlineList.length === 1 ? onlineList[0].name : undefined,
    keyword: trimmed,
  }
}

export function base64FromBytes(bytes: Uint8Array): string {
  let binary = ''
  const chunkSize = 0x8000
  for (let i = 0; i < bytes.length; i += chunkSize) {
    const sub = bytes.subarray(i, i + chunkSize)
    binary += String.fromCharCode(...sub)
  }
  return btoa(binary)
}

export function base64FromUtf8(text: string): string {
  return base64FromBytes(new TextEncoder().encode(text))
}

export function bytesFromBase64(b64: string): Uint8Array {
  const bin = atob(b64)
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) {
    out[i] = bin.charCodeAt(i)
  }
  return out
}
