import { describe, expect, it } from 'vitest'
import { ROOT, baseName, crumbs, isInside, joinPath, normalizePath, parentPath, validateName } from './path'

describe('normalizePath', () => {
  it('接受各种写法的盘符和路径', () => {
    expect(normalizePath('c:')).toBe('C:\\')
    expect(normalizePath('C:\\')).toBe('C:\\')
    expect(normalizePath('d:/照片/2024/')).toBe('D:\\照片\\2024')
    expect(normalizePath('"C:\\Program Files"')).toBe('C:\\Program Files')
    expect(normalizePath('C:\\\\a\\\\b')).toBe('C:\\a\\b')
  })
  it('处理 . 和 ..，且不越过盘符根', () => {
    expect(normalizePath('C:\\a\\.\\b\\..\\c')).toBe('C:\\a\\c')
    expect(normalizePath('C:\\..\\..')).toBe('C:\\')
  })
  it('空串和“此电脑”表示根，无法识别的返回 null', () => {
    expect(normalizePath('  ')).toBe(ROOT)
    expect(normalizePath('此电脑')).toBe(ROOT)
    expect(normalizePath('relative\\path')).toBeNull()
    expect(normalizePath('\\\\server\\share')).toBeNull()
  })
})

describe('路径拼接与拆分', () => {
  it('joinPath / parentPath / baseName 互相一致', () => {
    expect(joinPath('C:\\', 'a')).toBe('C:\\a')
    expect(joinPath('C:\\a', 'b')).toBe('C:\\a\\b')
    expect(parentPath('C:\\a\\b')).toBe('C:\\a')
    expect(parentPath('C:\\a')).toBe('C:\\')
    expect(parentPath('C:\\')).toBe(ROOT)
    expect(parentPath(ROOT)).toBe(ROOT)
    expect(baseName('C:\\a\\b.txt')).toBe('b.txt')
    expect(baseName('C:\\')).toBe('C:')
    for (const p of ['C:\\a', 'D:\\x\\y\\z']) expect(joinPath(parentPath(p), baseName(p))).toBe(p)
  })
  it('crumbs 生成每一级', () => {
    expect(crumbs(ROOT)).toEqual([{ label: '此电脑', path: ROOT }])
    expect(crumbs('C:\\Users\\me')).toEqual([
      { label: '此电脑', path: ROOT },
      { label: 'C:', path: 'C:\\' },
      { label: 'Users', path: 'C:\\Users' },
      { label: 'me', path: 'C:\\Users\\me' },
    ])
  })
  it('isInside 不区分大小写，且不把同前缀的兄弟目录当成子目录', () => {
    expect(isInside('c:\\A\\b', 'C:\\a')).toBe(true)
    expect(isInside('C:\\a', 'C:\\a')).toBe(true)
    expect(isInside('C:\\ab', 'C:\\a')).toBe(false)
    expect(isInside('C:\\a\\b', 'C:\\')).toBe(true)
  })
})

describe('validateName', () => {
  it('拒绝非法字符、保留名和以点结尾', () => {
    expect(validateName('报告.docx')).toBeNull()
    expect(validateName('')).not.toBeNull()
    expect(validateName('a/b')).not.toBeNull()
    expect(validateName('a?')).not.toBeNull()
    expect(validateName('CON')).not.toBeNull()
    expect(validateName('com1.txt')).not.toBeNull()
    expect(validateName('abc.')).not.toBeNull()
  })
})
