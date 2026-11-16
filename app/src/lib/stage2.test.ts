import { describe, expect, it } from 'vitest'
import type { FsEntry } from '../api/types'
import {
  activateTabInPane,
  applyListingDiff,
  closeTabInPane,
  createPaneTabsState,
  openTabInPane,
  resolvePaneTabsState,
  tabLabelOf,
} from './stage2'

const entry = (name: string, size: number, mtime = 100, dir = false): FsEntry => ({
  name,
  dir,
  size,
  mtime,
})

describe('applyListingDiff（任务 2.1 目录缓存增量更新）', () => {
  it('内容没有变化时复用原数组引用，避免整页重排重绘', () => {
    const a = entry('报告.docx', 1024, 10)
    const b = entry('照片', 0, 20, true)
    const prev = [a, b]
    const next = [entry('报告.docx', 1024, 10), entry('照片', 0, 20, true)]
    const merged = applyListingDiff(prev, next)
    expect(merged).toBe(prev)
    expect(merged[0]).toBe(a)
  })

  it('只更新变化的项、保留未变项的对象引用并移除已删除项', () => {
    const unchanged = entry('不变.txt', 50, 1)
    const oldChanged = entry('改动.txt', 50, 1)
    const removed = entry('已删.txt', 10, 1)
    const prev = [unchanged, oldChanged, removed]

    const next = [
      entry('不变.txt', 50, 1),
      entry('改动.txt', 99, 2),
      entry('新增.txt', 12, 3),
    ]

    const merged = applyListingDiff(prev, next)
    expect(merged).not.toBe(prev)
    expect(merged.map((x) => x.name)).toEqual(['不变.txt', '改动.txt', '新增.txt'])
    expect(merged[0]).toBe(unchanged)
    expect(merged[1].size).toBe(99)
  })
})

describe('多标签栏状态机（任务 2.6）', () => {
  it('支持新建、切换、关闭标签，且最后一个标签关闭时重置到此电脑而不清空栏', () => {
    let pane = createPaneTabsState('self', 'C:\\Users\\Me')
    expect(pane.tabs).toHaveLength(1)
    expect(tabLabelOf(pane.tabs[0].path)).toBe('Me')

    pane = openTabInPane(pane, 'D:\\工作')
    expect(pane.tabs).toHaveLength(2)
    expect(pane.activeId).toBe(pane.tabs[1].id)

    const firstId = pane.tabs[0].id
    pane = activateTabInPane(pane, firstId)
    expect(pane.activeId).toBe(firstId)

    pane = closeTabInPane(pane, firstId)
    expect(pane.tabs).toHaveLength(1)
    expect(pane.tabs[0].path).toBe('D:\\工作')

    pane = closeTabInPane(pane, pane.tabs[0].id)
    expect(pane.tabs).toHaveLength(1)
    expect(pane.tabs[0].path).toBe('')
    expect(tabLabelOf('')).toBe('此电脑')
  })

  it('resolvePaneTabsState 对未初始化设备返回稳定引用，避免 React useSyncExternalStore 循环重渲染', () => {
    const empty = {}
    expect(resolvePaneTabsState(empty, 'self')).toBe(resolvePaneTabsState(empty, 'self'))
  })
})
