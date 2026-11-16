import { useState } from 'react'
import { crumbs, normalizePath } from '../../lib/path'
import { toast } from '../../store/toasts'
import { Icon } from '../../components/Icon'

interface Props {
  path: string
  onNavigate: (path: string) => void
  editRequest: number
}

/** 路径栏：每一级都能点；点空白处、Ctrl+L 或 Alt+D 切换成输入框，可直接输入或粘贴路径。 */
export function PathBar({ path, onNavigate, editRequest }: Props) {
  const [editing, setEditing] = useState(false)
  const [text, setText] = useState(path)
  const [seenRequest, setSeenRequest] = useState(editRequest)

  const startEdit = () => {
    setText(path)
    setEditing(true)
  }
  // 父组件收到 Ctrl+L 等快捷键时递增 editRequest，这里在渲染期间同步切换成输入框
  if (editRequest !== seenRequest) {
    setSeenRequest(editRequest)
    startEdit()
  }

  const submit = () => {
    const p = normalizePath(text)
    if (p == null) {
      toast.warn('路径格式不对，应该像 C:\\Users\\文档 这样，以盘符开头')
      return
    }
    setEditing(false)
    onNavigate(p)
    // 交还焦点给文件列表，便于继续用键盘
    requestAnimationFrame(() => document.querySelector<HTMLElement>('[data-file-list]')?.focus())
  }

  if (editing) {
    return (
      <form
        className="flex h-8 min-w-0 flex-1"
        onSubmit={(e) => {
          e.preventDefault()
          submit()
        }}
      >
        <input
          autoFocus
          onFocus={(e) => e.currentTarget.select()}
          value={text}
          aria-label="路径"
          spellCheck={false}
          onChange={(e) => setText(e.target.value)}
          onBlur={() => setEditing(false)}
          onKeyDown={(e) => {
            if (e.key === 'Escape') {
              e.preventDefault()
              setEditing(false)
            }
          }}
          placeholder="输入或粘贴路径，例如 D:\照片"
          className="h-8 w-full rounded-md border border-accent bg-panel px-2.5 font-mono text-[13px] outline-none"
        />
      </form>
    )
  }

  const list = crumbs(path)
  return (
    <nav
      aria-label="当前位置"
      className="flex h-8 min-w-0 flex-1 cursor-text items-center rounded-md border border-line bg-panel px-1"
      onClick={(e) => {
        if (e.target === e.currentTarget) startEdit()
      }}
    >
      <ol className="flex min-w-0 items-center overflow-hidden">
        {list.map((c, i) => {
          const last = i === list.length - 1
          return (
            <li key={c.path} className={`flex min-w-0 items-center ${last ? '' : 'shrink'}`}>
              {i > 0 && <Icon name="right" size={12} className="mx-0.5 shrink-0 text-faint" />}
              <button
                type="button"
                aria-current={last ? 'location' : undefined}
                onClick={() => onNavigate(c.path)}
                className={`truncate rounded px-1.5 py-0.5 hover:bg-hover ${last ? 'font-medium' : 'text-muted'}`}
              >
                {c.label}
              </button>
            </li>
          )
        })}
      </ol>
      <button
        type="button"
        aria-label="编辑路径"
        title="编辑路径（Ctrl+L）"
        onClick={startEdit}
        className="ml-auto h-full min-w-8 flex-1 rounded"
      />
    </nav>
  )
}
