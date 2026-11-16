// Windows 风格路径的纯函数工具。空字符串表示“此电脑”（盘符和常用文件夹）。

export const ROOT = ''

export function isDriveRoot(path: string): boolean {
  return /^[A-Za-z]:\\$/.test(path)
}

/** 规范化用户输入或粘贴的路径；无法识别时返回 null。 */
export function normalizePath(input: string): string | null {
  let p = input.trim().replace(/^"(.*)"$/, '$1').trim()
  if (p === '' || p === '此电脑') return ROOT
  p = p.replace(/\//g, '\\')
  if (/^[A-Za-z]:$/.test(p)) return p.toUpperCase() + '\\'
  if (!/^[A-Za-z]:\\/.test(p)) return null
  p = p[0].toUpperCase() + p.slice(1)
  p = p.replace(/\\{2,}/g, '\\')
  const parts = p.split('\\')
  const out: string[] = []
  for (const part of parts) {
    if (part === '' || part === '.') continue
    if (part === '..') {
      if (out.length > 1) out.pop()
      continue
    }
    out.push(part)
  }
  return out.length === 1 ? out[0] + '\\' : out.join('\\')
}

export function joinPath(dir: string, name: string): string {
  return dir.endsWith('\\') ? dir + name : dir + '\\' + name
}

export function parentPath(path: string): string {
  if (path === ROOT || isDriveRoot(path)) return ROOT
  const i = path.lastIndexOf('\\')
  if (i < 0) return ROOT
  const head = path.slice(0, i)
  return /^[A-Za-z]:$/.test(head) ? head + '\\' : head
}

export function baseName(path: string): string {
  if (isDriveRoot(path)) return path.slice(0, 2)
  const i = path.lastIndexOf('\\')
  return i < 0 ? path : path.slice(i + 1)
}

export interface Crumb {
  label: string
  path: string
}

export function crumbs(path: string): Crumb[] {
  const list: Crumb[] = [{ label: '此电脑', path: ROOT }]
  if (path === ROOT) return list
  const parts = path.split('\\').filter(Boolean)
  let acc = ''
  for (const [i, part] of parts.entries()) {
    acc = i === 0 ? part + '\\' : joinPath(acc, part)
    list.push({ label: part, path: acc })
  }
  return list
}

/** 判断 child 是否就是 ancestor 或在它下面（用于拦截“把文件夹复制进自己”）。 */
export function isInside(child: string, ancestor: string): boolean {
  const a = ancestor.toLowerCase()
  const c = child.toLowerCase()
  if (c === a) return true
  return c.startsWith(a.endsWith('\\') ? a : a + '\\')
}

/** 文件名里不允许出现的字符。返回错误说明，合法时返回 null。 */
export function validateName(name: string): string | null {
  const n = name.trim()
  if (n === '') return '名字不能为空'
  if (/[\\/:*?"<>|]/.test(n)) return '名字里不能有 \\ / : * ? " < > | 这些字符'
  if (/^(con|prn|aux|nul|com\d|lpt\d)(\..*)?$/i.test(n)) return '这是系统保留的名字，换一个'
  if (n.endsWith('.')) return '名字不能以点结尾'
  return null
}
