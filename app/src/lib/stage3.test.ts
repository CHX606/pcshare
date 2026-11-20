import { describe, expect, it } from 'vitest'
import {
  base64FromUtf8,
  bytesFromBase64,
  mapClientPointToRemoteScreen,
  parseQuickInputHint,
} from './stage3'

describe('stage3 helpers', () => {
  it('maps rendered image clicks accurately to remote virtual-desktop coordinates', () => {
    const transform = {
      originX: -1920,
      originY: 120,
      sourceWidth: 1920,
      sourceHeight: 1080,
      imageWidth: 960,
      imageHeight: 540,
      dpiScalePercent: 150,
    }
    // Rendered box is 480x270 at (100, 50)
    const rect = { left: 100, top: 50, width: 480, height: 270 }

    // Center of rendered image -> center of second monitor (-960, 660)
    const center = mapClientPointToRemoteScreen(340, 185, rect, transform)
    expect(center).toEqual({ screenX: -960, screenY: 660 })

    // Top-left corner -> (-1920, 120)
    const topLeft = mapClientPointToRemoteScreen(100, 50, rect, transform)
    expect(topLeft).toEqual({ screenX: -1920, screenY: 120 })

    // Bottom-right exact edge & outside clamp -> strictly within [origin, origin + size - 1]
    const bottomRight = mapClientPointToRemoteScreen(580, 320, rect, transform)
    expect(bottomRight).toEqual({ screenX: -1, screenY: 1199 })
    const outOfBounds = mapClientPointToRemoteScreen(999, 999, rect, transform)
    expect(outOfBounds).toEqual({ screenX: -1, screenY: 1199 })
    const beforeOrigin = mapClientPointToRemoteScreen(0, 0, rect, transform)
    expect(beforeOrigin).toEqual({ screenX: -1920, screenY: 120 })
  })

  it('parses quick search device prefix and single-online-device fallback hint', () => {
    const devices = [
      { id: 'd1', name: '书房电脑', online: true },
      { id: 'd2', name: '公司主机', online: false },
    ]
    const withPrefix = parseQuickInputHint('书房 微信', devices)
    expect(withPrefix.scopedDeviceName).toBe('书房电脑')
    expect(withPrefix.keyword).toBe('微信')

    // Only 1 device is online -> omitting device name hints default target '书房电脑'
    const singleOnline = parseQuickInputHint('记事本', devices)
    expect(singleOnline.defaultSingleOnlineName).toBe('书房电脑')
    expect(singleOnline.keyword).toBe('记事本')
  })

  it('encodes UTF-8 terminal input and decodes raw binary output bytes losslessly', () => {
    const b64 = base64FromUtf8('echo 你好世界\r\n')
    const decoded = bytesFromBase64(b64)
    expect(new TextDecoder().decode(decoded)).toBe('echo 你好世界\r\n')
  })
})
