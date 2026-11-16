import { useEffect, useRef, useState } from 'react'
import type { FsEntry } from '../../api/types'
import { extOf } from '../../lib/format'
import { validateName } from '../../lib/path'
import { toast } from '../../store/toasts'
import { Icon } from '../../components/Icon'

/** 列表里的重命名输入框：默认只选中文件名主干，不选扩展名。 */
export function RenameInput({ entry, onDone }: { entry: FsEntry; onDone: (name: string | null) => Promise<boolean> | void }) {
  const ref = useRef<HTMLInputElement>(null)
  const [value, setValue] = useState(entry.name)
  const [error, setError] = useState<string | null>(null)
  const busy = useRef(false)
  const cancelled = useRef(false)

  useEffect(() => {
    const el = ref.current
    if (!el) return
    el.focus()
    const ext = entry.dir ? '' : extOf(entry.name)
    el.setSelectionRange(0, ext ? entry.name.length - ext.length - 1 : entry.name.length)
  }, [entry])

  const commit = async () => {
    if (busy.current || cancelled.current) return
    const name = value.trim()
    const err = validateName(name)
    if (err) {
      setError(err)
      toast.warn(err)
      ref.current?.focus()
      return
    }
    busy.current = true
    const ok = await onDone(name)
    busy.current = false
    if (ok === false) ref.current?.focus()
  }

  return (
    <input
      ref={ref}
      value={value}
      aria-label="新名字"
      aria-invalid={!!error}
      spellCheck={false}
      onChange={(e) => {
        setValue(e.target.value)
        setError(null)
      }}
      onKeyDown={(e) => {
        e.stopPropagation()
        if (e.key === 'Enter') void commit()
        if (e.key === 'Escape') {
          cancelled.current = true
          void onDone(null)
        }
      }}
      onBlur={() => void commit()}
      onClick={(e) => e.stopPropagation()}
      onDoubleClick={(e) => e.stopPropagation()}
      className={`h-6 w-full min-w-0 rounded border bg-panel px-1.5 outline-none ${error ? 'border-bad' : 'border-accent'}`}
    />
  )
}

export function EntryIcon({ entry }: { entry: FsEntry }) {
  if (entry.dir) return <Icon name="folder" className="shrink-0 text-warn" />
  const ext = extOf(entry.name)
  const image = ['jpg', 'jpeg', 'png', 'gif', 'webp', 'bmp'].includes(ext)
  return <Icon name={image ? 'image' : 'file'} className="shrink-0 text-muted" />
}
