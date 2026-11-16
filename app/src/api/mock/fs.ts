// 模拟的文件系统：只在浏览器里开发界面时使用，不代表真实行为。
import { ApiError } from '../errors'
import type { FsEntry, FsListing, FsRoot } from '../types'
import { baseName, isInside, joinPath, parentPath } from '../../lib/path'

function hash(s: string): number {
  let h = 2166136261
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i)
    h = Math.imul(h, 16777619)
  }
  return h >>> 0
}

export function rng(seed: number): () => number {
  let a = seed
  return () => {
    a |= 0
    a = (a + 0x6d2b79f5) | 0
    let t = Math.imul(a ^ (a >>> 15), 1 | a)
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

const FILE_WORDS = ['报告', '会议纪要', '预算', '照片', '截图', '合同', '笔记', '方案', '清单', '草稿', '数据', '备份', 'README', 'invoice', 'setup', 'notes']
const EXTS = ['docx', 'xlsx', 'pdf', 'txt', 'jpg', 'png', 'mp4', 'zip', 'pptx', 'md', 'exe', 'mp3']
const DIR_WORDS = ['资料', '归档', '2024', '2025', '临时', '素材', '导出', '旧版本', 'src', 'assets']
const NOW = Date.now()

function randomEntries(path: string, count: number, dirRatio: number): FsEntry[] {
  const r = rng(hash(path.toLowerCase()))
  const used = new Set<string>()
  const out: FsEntry[] = []
  for (let i = 0; out.length < count && i < count * 3; i++) {
    const isDir = r() < dirRatio
    const word = isDir ? DIR_WORDS[Math.floor(r() * DIR_WORDS.length)] : FILE_WORDS[Math.floor(r() * FILE_WORDS.length)]
    const suffix = r() < 0.6 ? ` ${Math.floor(r() * 300)}` : ''
    const name = isDir ? word + suffix : `${word}${suffix}.${EXTS[Math.floor(r() * EXTS.length)]}`
    if (used.has(name.toLowerCase())) continue
    used.add(name.toLowerCase())
    out.push({
      name,
      dir: isDir,
      size: isDir ? 0 : Math.floor(r() ** 3 * 800_000_000) + 120,
      mtime: NOW - Math.floor(r() * 400 * 86400_000),
    })
  }
  return out
}

function dirs(...names: string[]): FsEntry[] {
  return names.map((name, i) => ({ name, dir: true, size: 0, mtime: NOW - (i + 3) * 86400_000 * 7 }))
}

export class MockFs {
  private nodes = new Map<string, Map<string, FsEntry>>()
  readonly user: string
  readonly roots: FsRoot[]

  constructor(user: string, drives: string[]) {
    this.user = user
    const home = `C:\\Users\\${user}`
    this.roots = [
      ...drives.map((d, i): FsRoot => ({
        name: i === 0 ? `本地磁盘 (${d})` : `数据 (${d})`,
        path: `${d}\\`,
        kind: 'drive',
        total: (i === 0 ? 512 : 2000) * 1e9,
        free: (i === 0 ? 96 : 1240) * 1e9,
      })),
      { name: '桌面', path: `${home}\\Desktop`, kind: 'desktop' },
      { name: '下载', path: `${home}\\Downloads`, kind: 'downloads' },
      { name: '文档', path: `${home}\\Documents`, kind: 'documents' },
    ]
    this.seed('C:\\', [...dirs('Program Files', 'Program Files (x86)', 'Users', 'Windows', 'System Volume Information'), { name: 'pagefile.sys', dir: false, size: 17e9, mtime: NOW - 3600_000 }])
    this.seed('C:\\Users', dirs(user, 'Public'))
    this.seed(home, dirs('Desktop', 'Downloads', 'Documents', 'Pictures', 'Music', 'Videos'))
    this.seed(`${home}\\Downloads`, randomEntries(`${home}\\Downloads`, 140, 0.08))
    if (drives.includes('D:')) {
      this.seed('D:\\', dirs('照片', '项目', '软件', '五千个文件'))
      this.seed('D:\\五千个文件', randomBig(5000))
    }
  }

  private key(path: string): string {
    return path.toLowerCase()
  }

  private seed(path: string, entries: FsEntry[]) {
    this.nodes.set(this.key(path), new Map(entries.map((e) => [e.name.toLowerCase(), e])))
  }

  /** 取目录节点；没生成过的目录按路径随机生成内容。 */
  private node(path: string): Map<string, FsEntry> {
    const k = this.key(path)
    let n = this.nodes.get(k)
    if (n) return n
    if (/^[a-z]:\\$/i.test(path)) {
      if (!this.roots.some((r) => r.path.toLowerCase() === k))
        throw new ApiError('not_found', `这台电脑上没有 ${path.slice(0, 2)} 盘`)
    } else {
      // 递归保证父目录存在，再确认自己确实是父目录里的一个文件夹
      const self = this.node(parentPath(path)).get(baseName(path).toLowerCase())
      if (!self?.dir) throw new ApiError('not_found', `找不到文件夹“${path}”`)
    }
    const depth = path.split('\\').filter(Boolean).length
    const count = depth > 6 ? 4 : Math.floor(rng(hash(k))() * 40) + 3
    n = new Map(randomEntries(path, count, depth > 5 ? 0 : 0.25).map((e) => [e.name.toLowerCase(), e]))
    this.nodes.set(k, n)
    return n
  }

  list(path: string): FsListing {
    if (/system volume information/i.test(path)) throw new ApiError('forbidden', '对方系统不允许读取这个文件夹')
    return { path, entries: [...this.node(path).values()] }
  }

  stat(path: string): FsEntry | undefined {
    return this.node(parentPath(path)).get(baseName(path).toLowerCase())
  }

  mkdir(path: string) {
    const parent = this.node(parentPath(path))
    const name = baseName(path)
    if (parent.has(name.toLowerCase())) throw new ApiError('already_exists', `已经有一个叫“${name}”的文件或文件夹`)
    parent.set(name.toLowerCase(), { name, dir: true, size: 0, mtime: Date.now() })
    this.nodes.set(this.key(path), new Map())
  }

  rename(path: string, newName: string) {
    const parentP = parentPath(path)
    const parent = this.node(parentP)
    const old = parent.get(baseName(path).toLowerCase())
    if (!old) throw new ApiError('not_found', `找不到“${baseName(path)}”，可能已被移走`)
    if (newName.toLowerCase() !== old.name.toLowerCase() && parent.has(newName.toLowerCase()))
      throw new ApiError('already_exists', `已经有一个叫“${newName}”的文件或文件夹`)
    parent.delete(old.name.toLowerCase())
    parent.set(newName.toLowerCase(), { ...old, name: newName, mtime: Date.now() })
    if (old.dir) this.moveSubtree(path, joinPath(parentP, newName))
  }

  remove(path: string) {
    const parent = this.node(parentPath(path))
    const name = baseName(path)
    const e = parent.get(name.toLowerCase())
    if (!e) throw new ApiError('not_found', `找不到“${name}”，可能已被删除`)
    if (/pagefile|\.lock$/i.test(name)) throw new ApiError('busy', `“${name}”正被系统或其他程序占用，删不掉`)
    parent.delete(name.toLowerCase())
    for (const k of [...this.nodes.keys()]) if (isInside(k, path.toLowerCase())) this.nodes.delete(k)
  }

  /** 把一项放进目标目录（复制或传输完成时调用）。重名时按“改名保留”处理。 */
  put(destDir: string, entry: FsEntry, conflict: 'overwrite' | 'skip' | 'rename' = 'rename'): string | null {
    const dest = this.node(destDir)
    let name = entry.name
    if (dest.has(name.toLowerCase())) {
      if (conflict === 'skip') return null
      if (conflict === 'rename') {
        const dot = entry.dir ? -1 : name.lastIndexOf('.')
        const stem = dot > 0 ? name.slice(0, dot) : name
        const ext = dot > 0 ? name.slice(dot) : ''
        for (let i = 2; dest.has(name.toLowerCase()); i++) name = `${stem} (${i})${ext}`
      }
    }
    dest.set(name.toLowerCase(), { ...entry, name, mtime: entry.mtime })
    return joinPath(destDir, name)
  }

  /** 估算一项的大小（文件夹按 50 MB 估算），用来模拟进度。 */
  sizeOf(path: string): number {
    const e = this.stat(path)
    if (!e) throw new ApiError('not_found', `找不到“${baseName(path)}”`)
    return e.dir ? 50_000_000 : e.size
  }

  private moveSubtree(from: string, to: string) {
    const f = from.toLowerCase()
    for (const [k, v] of [...this.nodes.entries()]) {
      if (isInside(k, f)) {
        this.nodes.delete(k)
        this.nodes.set(to.toLowerCase() + k.slice(f.length), v)
      }
    }
  }
}

function randomBig(count: number): FsEntry[] {
  const r = rng(5000)
  const out: FsEntry[] = []
  for (let i = 1; i <= count; i++) {
    out.push({
      name: `IMG_${String(i).padStart(5, '0')}.jpg`,
      dir: false,
      size: Math.floor(r() * 6_000_000) + 200_000,
      mtime: NOW - Math.floor(r() * 900 * 86400_000),
    })
  }
  return out
}
