import { describe, expect, it } from 'vitest'
import type { DesktopFrameMeta, DesktopInputEvent } from '../api/types'
import {
  DesktopInputQueue,
  MAX_INPUT_BATCH,
  TouchGesture,
  buildDesktopPacket,
  mapPointToRemotePixel,
  parseDesktopPacket,
  parseSourceChoice,
  pickSourceChoice,
  vkFromCode,
  wheelToWindows,
} from './desktop'

const transform = {
  originX: -1920,
  originY: 0,
  sourceWidth: 1920,
  sourceHeight: 1080,
  imageWidth: 1280,
  imageHeight: 720,
  dpiScalePercent: 125,
  targetHwnd: '123',
}

describe('parseDesktopPacket', () => {
  it('拆出帧头和视频数据，字段名与后端一致', () => {
    const meta: DesktopFrameMeta = {
      mode: 'live',
      format: 'h264-annexb',
      timestampUs: 1000,
      key: true,
      epoch: 2,
      codec: 'avc1.640028',
      encoder: 'h264_amf',
      reason: null,
      transform,
    }
    const data = new Uint8Array([0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28])
    const frame = parseDesktopPacket(buildDesktopPacket(meta, data))
    expect(frame.meta).toEqual(meta)
    expect(Array.from(frame.data)).toEqual(Array.from(data))
  })

  it('接受带偏移的视图和数字数组', () => {
    const buf = buildDesktopPacket({ mode: 'step', format: 'jpeg' } as DesktopFrameMeta, new Uint8Array([9, 8]))
    const padded = new Uint8Array(buf.byteLength + 3)
    padded.set(new Uint8Array(buf), 3)
    expect(Array.from(parseDesktopPacket(padded.subarray(3)).data)).toEqual([9, 8])
    expect(parseDesktopPacket(Array.from(new Uint8Array(buf))).meta.format).toBe('jpeg')
  })

  it('长度不对时报错', () => {
    expect(() => parseDesktopPacket(new Uint8Array([0, 0, 0, 9, 1]).buffer)).toThrow()
    expect(() => parseDesktopPacket(new ArrayBuffer(2))).toThrow()
  })
})

describe('mapPointToRemotePixel', () => {
  const rect = { left: 100, top: 50, width: 640, height: 360 }
  it('按比例换算到原图像素并加上负原点', () => {
    expect(mapPointToRemotePixel(100, 50, rect, transform)).toEqual({ x: -1920, y: 0 })
    expect(mapPointToRemotePixel(420, 230, rect, transform)).toEqual({ x: -960, y: 540 })
  })
  it('右下边缘和矩形外都限制在 0…尺寸−1', () => {
    expect(mapPointToRemotePixel(740, 410, rect, transform)).toEqual({ x: -1, y: 1079 })
    expect(mapPointToRemotePixel(0, 0, rect, transform)).toEqual({ x: -1920, y: 0 })
  })
})

describe('vkFromCode', () => {
  it('常用键位', () => {
    expect(vkFromCode('KeyA')).toBe(0x41)
    expect(vkFromCode('Digit7')).toBe(0x37)
    expect(vkFromCode('F12')).toBe(0x7b)
    expect(vkFromCode('Numpad3')).toBe(0x63)
    expect(vkFromCode('ControlLeft')).toBe(0xa2)
    expect(vkFromCode('Enter')).toBe(0x0d)
    expect(vkFromCode('F25')).toBeNull()
    expect(vkFromCode('Unidentified')).toBeNull()
  })
})

describe('wheelToWindows', () => {
  it('浏览器向下滚为 Windows 负值，每格 120', () => {
    expect(wheelToWindows(0, 100, 0)).toEqual({ deltaX: 0, deltaY: -120 })
    expect(wheelToWindows(0, -3, 1)).toEqual({ deltaX: 0, deltaY: 120 })
    expect(wheelToWindows(100, 0, 0).deltaX).toBe(120)
    expect(wheelToWindows(0, 1e9, 0).deltaY).toBe(-32767)
  })
})

describe('来源选择', () => {
  const targets = {
    monitors: [{ index: 1, name: '', originX: 0, originY: 0, width: 1, height: 1, dpiScalePercent: 100, isPrimary: true }],
    windows: [{ hwnd: '42', pid: 1, title: '', width: 1, height: 1 }],
    desktop: { place: 'normal' as const, elevated: false },
  }
  it('保留仍存在的来源，否则退回第一块显示器', () => {
    expect(pickSourceChoice(targets, 'win:42')).toBe('win:42')
    expect(pickSourceChoice(targets, 'win:7')).toBe('mon:1')
    expect(parseSourceChoice('win:42')).toEqual({ hwnd: '42' })
    expect(parseSourceChoice('mon:1')).toEqual({ monitorIndex: 1 })
  })
})

describe('DesktopInputQueue', () => {
  const tick = () => new Promise((r) => setTimeout(r, 0))

  it('一次只有一批在途，排队时合并移动和滚轮，按键顺序不变', async () => {
    const sent: DesktopInputEvent[][] = []
    let ack: () => void = () => undefined
    const q = new DesktopInputQueue(
      (events) => {
        sent.push(events)
        return new Promise<void>((r) => (ack = r))
      },
      () => undefined,
    )
    q.push({ kind: 'move', x: 1, y: 1 })
    q.push({ kind: 'move', x: 2, y: 2 })
    q.push({ kind: 'move', x: 3, y: 3 })
    q.push({ kind: 'button', x: 3, y: 3, button: 'left', down: true })
    q.push({ kind: 'move', x: 4, y: 4 })
    q.push({ kind: 'wheel', x: 4, y: 4, deltaX: 0, deltaY: 120 })
    q.push({ kind: 'wheel', x: 4, y: 4, deltaX: 0, deltaY: 120 })
    expect(sent).toEqual([[{ kind: 'move', x: 1, y: 1 }]])
    ack()
    await tick()
    expect(sent[1]).toEqual([
      { kind: 'move', x: 3, y: 3 },
      { kind: 'button', x: 3, y: 3, button: 'left', down: true },
      { kind: 'move', x: 4, y: 4 },
      { kind: 'wheel', x: 4, y: 4, deltaX: 0, deltaY: 240 },
    ])
    ack()
    await tick()
    expect(q.busy).toBe(false)
  })

  it('每批最多 64 个，失败时报告并继续，关闭后丢弃', async () => {
    const sizes: number[] = []
    const errors: number[] = []
    const q = new DesktopInputQueue(
      async (events) => {
        sizes.push(events.length)
        if (sizes.length === 2) throw new Error('x')
      },
      (_e, events) => errors.push(events.length),
    )
    for (let i = 0; i < 100; i++) q.push({ kind: 'key', vk: 0x41, down: i % 2 === 0 })
    await tick()
    await tick()
    expect(sizes[0]).toBe(1)
    expect(sizes.slice(1)).toEqual([MAX_INPUT_BATCH, 99 - MAX_INPUT_BATCH])
    expect(errors).toEqual([MAX_INPUT_BATCH])
    q.close()
    q.push({ kind: 'release' })
    await tick()
    expect(sizes.length).toBe(3)
  })
})

describe('TouchGesture', () => {
  const p = (x: number, y: number) => ({ x, y })

  it('轻点是左键单击，两次轻点落在同一点', () => {
    const g = new TouchGesture()
    g.down(p(100, 100), p(10, 10), 0)
    expect(g.move(p(101, 100), p(13, 10))).toEqual([])
    const tap = g.up(p(101, 100), 50)
    expect(tap.map((e) => e.kind)).toEqual(['move', 'button', 'button'])
    expect(tap[1]).toMatchObject({ x: 100, y: 100, button: 'left', down: true })
    g.down(p(103, 102), p(12, 11), 200)
    expect(g.up(p(103, 102), 250)[2]).toMatchObject({ x: 100, y: 100, down: false })
    g.down(p(103, 102), p(12, 11), 2000)
    expect(g.up(p(103, 102), 2050)[2]).toMatchObject({ x: 103, y: 102 })
  })

  it('移动超过阈值后按下左键拖动，松手时松开', () => {
    const g = new TouchGesture()
    g.down(p(100, 100), p(10, 10), 0)
    const start = g.move(p(130, 100), p(30, 10))
    expect(start[1]).toMatchObject({ x: 100, y: 100, button: 'left', down: true })
    expect(start[2]).toEqual({ kind: 'move', x: 130, y: 100 })
    expect(g.longPress()).toEqual([])
    expect(g.up(p(150, 100), 900)).toEqual([{ kind: 'button', x: 150, y: 100, button: 'left', down: false }])
  })

  it('长按是右键单击，之后的移动和松手不再发送', () => {
    const g = new TouchGesture()
    g.down(p(100, 100), p(10, 10), 0)
    expect(g.longPress()[1]).toMatchObject({ button: 'right', down: true })
    expect(g.move(p(200, 200), p(90, 90))).toEqual([])
    expect(g.up(p(200, 200), 900)).toEqual([])
    expect(g.active).toBe(false)
  })

  it('拖动中被打断时报告需要松开', () => {
    const g = new TouchGesture()
    g.down(p(0, 0), p(0, 0), 0)
    g.move(p(50, 0), p(50, 0))
    expect(g.cancel()).toBe(true)
    expect(g.cancel()).toBe(false)
  })
})
