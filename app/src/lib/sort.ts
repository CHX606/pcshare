import type { FsEntry } from '../api/types'
import { typeName } from './format'

export type SortKey = 'name' | 'mtime' | 'type' | 'size'
export interface SortSpec {
  key: SortKey
  asc: boolean
}

const collator = new Intl.Collator('zh-CN', { numeric: true, sensitivity: 'base' })

/** 文件夹总在前面；同类按指定列排序，相同时按名称兜底。 */
export function sortEntries(entries: readonly FsEntry[], spec: SortSpec): FsEntry[] {
  const dir = spec.asc ? 1 : -1
  return [...entries].sort((a, b) => {
    if (a.dir !== b.dir) return a.dir ? -1 : 1
    let r = 0
    switch (spec.key) {
      case 'name':
        r = collator.compare(a.name, b.name)
        break
      case 'mtime':
        r = a.mtime - b.mtime
        break
      case 'type':
        r = collator.compare(typeName(a), typeName(b))
        break
      case 'size':
        r = a.size - b.size
        break
    }
    if (r === 0 && spec.key !== 'name') r = collator.compare(a.name, b.name)
    return r * dir
  })
}

/**
 * 打字跳转：从 start 的下一项开始找名称以 prefix 开头的项，找不到就回到开头继续找。
 * 返回下标，没有匹配时返回 -1。
 */
export function typeaheadIndex(names: readonly string[], prefix: string, start: number): number {
  if (!prefix || names.length === 0) return -1
  const p = prefix.toLocaleLowerCase()
  // 连续按同一个字母时在同字母开头的项之间循环；否则先看当前项是否仍然匹配
  const same = p.length > 1 && [...p].every((c) => c === p[0])
  const needle = same ? p[0] : p
  const from = same || p.length === 1 ? start + 1 : start
  for (let k = 0; k < names.length; k++) {
    const i = (((from + k) % names.length) + names.length) % names.length
    if (names[i].toLocaleLowerCase().startsWith(needle)) return i
  }
  return -1
}

/** 简单的模糊匹配：关键字的每个字按顺序出现在名称里即算匹配。返回分数，越小越好，不匹配为 -1。 */
export function fuzzyScore(name: string, query: string): number {
  const n = name.toLocaleLowerCase()
  const q = query.trim().toLocaleLowerCase()
  if (!q) return 0
  const direct = n.indexOf(q)
  if (direct === 0) return 0
  if (direct > 0) return 1 + direct
  let pos = -1
  let gaps = 0
  for (const ch of q) {
    if (ch === ' ') continue
    const next = n.indexOf(ch, pos + 1)
    if (next < 0) return -1
    gaps += next - pos - 1
    pos = next
  }
  return 100 + gaps
}
