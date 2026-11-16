import { describe, expect, it } from 'vitest'
import type { FsEntry } from '../api/types'
import { fuzzyScore, sortEntries, typeaheadIndex } from './sort'

const e = (name: string, dir: boolean, size = 0, mtime = 0): FsEntry => ({ name, dir, size, mtime })

describe('sortEntries', () => {
  const list = [e('b.txt', false, 30, 3), e('文件夹2', true), e('a10.txt', false, 10, 1), e('a2.txt', false, 20, 2), e('文件夹1', true)]

  it('文件夹总在前面，名称按自然顺序', () => {
    expect(sortEntries(list, { key: 'name', asc: true }).map((x) => x.name)).toEqual(['文件夹1', '文件夹2', 'a2.txt', 'a10.txt', 'b.txt'])
  })
  it('倒序时文件夹仍在前面', () => {
    const r = sortEntries(list, { key: 'size', asc: false }).map((x) => x.name)
    expect(r.slice(0, 2).every((n) => n.startsWith('文件夹'))).toBe(true)
    expect(r.slice(2)).toEqual(['b.txt', 'a2.txt', 'a10.txt'])
  })
  it('不修改原数组', () => {
    const copy = list.map((x) => x.name)
    sortEntries(list, { key: 'mtime', asc: true })
    expect(list.map((x) => x.name)).toEqual(copy)
  })
})

describe('typeaheadIndex', () => {
  const names = ['apple', 'apricot', 'banana', 'blueberry', 'cherry']
  it('单个字母从当前项的下一项开始找，并循环', () => {
    expect(typeaheadIndex(names, 'b', 0)).toBe(2)
    expect(typeaheadIndex(names, 'b', 2)).toBe(3)
    expect(typeaheadIndex(names, 'b', 3)).toBe(2)
    expect(typeaheadIndex(names, 'a', -1)).toBe(0)
  })
  it('连续输入多个字时，当前项仍匹配就停在当前项', () => {
    expect(typeaheadIndex(names, 'ap', 0)).toBe(0)
    expect(typeaheadIndex(names, 'apr', 0)).toBe(1)
    expect(typeaheadIndex(names, 'bl', 2)).toBe(3)
  })
  it('重复按同一个字母时在同字母开头的项之间循环', () => {
    expect(typeaheadIndex(names, 'bb', 2)).toBe(3)
  })
  it('没有匹配返回 -1', () => {
    expect(typeaheadIndex(names, 'z', 0)).toBe(-1)
    expect(typeaheadIndex([], 'a', 0)).toBe(-1)
  })
})

describe('fuzzyScore', () => {
  it('开头匹配优于中间匹配，优于逐字匹配，不匹配为 -1', () => {
    const start = fuzzyScore('微信', '微')
    const mid = fuzzyScore('企业微信', '微信')
    const loose = fuzzyScore('Visual Studio Code', 'vsc')
    expect(start).toBe(0)
    expect(mid).toBeGreaterThan(start)
    expect(loose).toBeGreaterThan(mid)
    expect(fuzzyScore('Chrome', 'xyz')).toBe(-1)
  })
})
