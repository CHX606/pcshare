import { describe, expect, it } from 'vitest'
import { formatDuration, formatSize, shortId } from './format'

describe('formatDuration（剩余时间边界格式化）', () => {
  it('处理无效值与负数返回空字符串', () => {
    expect(formatDuration(-1)).toBe('')
    expect(formatDuration(Number.NaN)).toBe('')
    expect(formatDuration(Number.POSITIVE_INFINITY)).toBe('')
  })

  it('在 59.x / 119.x / 3599.x 秒向上取整时不出现 60 秒或 60 分', () => {
    expect(formatDuration(0)).toBe('0 秒')
    expect(formatDuration(0.2)).toBe('1 秒')
    expect(formatDuration(59)).toBe('59 秒')
    expect(formatDuration(59.4)).toBe('1 分')
    expect(formatDuration(60)).toBe('1 分')
    expect(formatDuration(60.1)).toBe('1 分 1 秒')
    expect(formatDuration(119.4)).toBe('2 分')
    expect(formatDuration(119.9)).toBe('2 分')
    expect(formatDuration(121)).toBe('2 分 1 秒')
    expect(formatDuration(3599.4)).toBe('1 小时')
    expect(formatDuration(3600)).toBe('1 小时')
    expect(formatDuration(3660)).toBe('1 小时 1 分')
    expect(formatDuration(7200)).toBe('2 小时')
  })
})

describe('formatSize 与 shortId', () => {
  it('字节单位换算与身份码截断', () => {
    expect(formatSize(512)).toBe('512 B')
    expect(formatSize(1024)).toBe('1.0 KB')
    expect(formatSize(10 * 1024 * 1024)).toBe('10 MB')
    expect(shortId('abcdef01234567899876543210fedcba')).toBe('abcdef01…10fedcba')
    expect(shortId('short')).toBe('short')
  })
})
