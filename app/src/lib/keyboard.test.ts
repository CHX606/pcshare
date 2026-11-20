import { describe, expect, it } from 'vitest'
import { isImeComposing } from './keyboard'

describe('isImeComposing', () => {
  it('识别组合输入态（isComposing / Process / keyCode 229）', () => {
    const fakeEvent = (partial: Partial<KeyboardEvent>) => partial as KeyboardEvent
    expect(isImeComposing(fakeEvent({ isComposing: true, key: 'Enter', keyCode: 13 }))).toBe(true)
    expect(isImeComposing(fakeEvent({ isComposing: false, key: 'Process', keyCode: 0 }))).toBe(true)
    expect(isImeComposing(fakeEvent({ isComposing: false, key: 'Enter', keyCode: 229 }))).toBe(true)
    expect(isImeComposing(fakeEvent({ isComposing: false, key: 'Enter', keyCode: 13 }))).toBe(false)
    expect(
      isImeComposing({ nativeEvent: fakeEvent({ isComposing: true, key: 'Enter', keyCode: 13 }) }),
    ).toBe(true)
  })
})
