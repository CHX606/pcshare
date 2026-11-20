import type { FsEntry } from '../api/types'

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let v = bytes / 1024
  let i = 0
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i++
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`
}

export function formatSpeed(bytesPerSec: number): string {
  return `${formatSize(Math.round(bytesPerSec))}/秒`
}

const dateFmt = new Intl.DateTimeFormat('zh-CN', {
  year: 'numeric',
  month: '2-digit',
  day: '2-digit',
  hour: '2-digit',
  minute: '2-digit',
  hour12: false,
})

export function formatTime(ms: number): string {
  if (!ms) return ''
  return dateFmt.format(ms)
}

export function formatAgo(ms: number, now = Date.now()): string {
  const s = Math.max(0, Math.round((now - ms) / 1000))
  if (s < 60) return '刚刚'
  if (s < 3600) return `${Math.floor(s / 60)} 分钟前`
  if (s < 86400) return `${Math.floor(s / 3600)} 小时前`
  return `${Math.floor(s / 86400)} 天前`
}

export function formatDuration(sec: number): string {
  if (!Number.isFinite(sec) || sec < 0) return ''
  const total = Math.ceil(sec)
  if (total < 60) return `${total} 秒`
  if (total < 3600) {
    const m = Math.floor(total / 60)
    const s = total % 60
    return s > 0 ? `${m} 分 ${s} 秒` : `${m} 分`
  }
  const h = Math.floor(total / 3600)
  const m = Math.floor((total % 3600) / 60)
  return m > 0 ? `${h} 小时 ${m} 分` : `${h} 小时`
}

const TYPE_NAMES: Record<string, string> = {
  exe: '应用程序',
  lnk: '快捷方式',
  txt: '文本文档',
  md: 'Markdown 文档',
  pdf: 'PDF 文档',
  doc: 'Word 文档',
  docx: 'Word 文档',
  xls: 'Excel 表格',
  xlsx: 'Excel 表格',
  ppt: 'PPT 演示文稿',
  pptx: 'PPT 演示文稿',
  jpg: 'JPG 图片',
  jpeg: 'JPG 图片',
  png: 'PNG 图片',
  gif: 'GIF 图片',
  mp4: 'MP4 视频',
  mkv: 'MKV 视频',
  mp3: 'MP3 音频',
  zip: '压缩文件',
  rar: '压缩文件',
  '7z': '压缩文件',
}

export function extOf(name: string): string {
  const i = name.lastIndexOf('.')
  return i > 0 ? name.slice(i + 1).toLowerCase() : ''
}

export function typeName(e: Pick<FsEntry, 'name' | 'dir'>): string {
  if (e.dir) return '文件夹'
  const ext = extOf(e.name)
  if (!ext) return '文件'
  return TYPE_NAMES[ext] ?? `${ext.toUpperCase()} 文件`
}

/** 设备身份码只显示前后几位，方便核对。 */
export function shortId(id: string): string {
  return id.length > 16 ? `${id.slice(0, 8)}…${id.slice(-8)}` : id
}
