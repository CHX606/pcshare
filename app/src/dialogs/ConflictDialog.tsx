import { useState } from 'react'
import { Button, Modal } from '../components/Modal'
import { useUi, type ConflictPrompt } from '../store/ui'

export function ConflictDialog() {
  const cur = useUi((s) => s.conflictPrompt)
  return (
    <Modal open={!!cur} onClose={() => cur?.resolve(null)} title="遇到同名文件或文件夹" width={460}>
      {cur && <ConflictBody key={`${cur.fileName}-${cur.remainingCount}`} {...cur} />}
    </Modal>
  )
}

function ConflictBody({ fileName, destDeviceName, remainingCount, resolve }: ConflictPrompt) {
  const [applyToAll, setApplyToAll] = useState(remainingCount > 1)

  return (
    <div className="flex flex-col gap-4 text-sm">
      <p className="leading-relaxed text-muted">
        {destDeviceName}的目标位置已经有一个同名的 <span className="font-medium text-fg">“{fileName}”</span>，要怎么处理？
      </p>

      {remainingCount > 1 && (
        <label className="flex cursor-pointer items-center gap-2 rounded-md bg-hover px-3 py-2 text-xs">
          <input
            type="checkbox"
            checked={applyToAll}
            onChange={(e) => setApplyToAll(e.target.checked)}
            className="size-4 accent-[var(--color-accent)]"
          />
          应用到全部（本次共 {remainingCount} 项同名冲突）
        </label>
      )}

      <div className="grid grid-cols-1 gap-2">
        <button
          type="button"
          data-autofocus
          onClick={() => resolve({ policy: 'rename', applyToAll })}
          className="flex flex-col rounded-lg border border-line px-3.5 py-2.5 text-left hover:border-accent hover:bg-hover"
        >
          <span className="font-medium">都保留</span>
          <span className="text-xs text-muted">保留原来的不动，新传过去的自动加序号（如“名称 (2)”）</span>
        </button>
        <button
          type="button"
          onClick={() => resolve({ policy: 'skip', applyToAll })}
          className="flex flex-col rounded-lg border border-line px-3.5 py-2.5 text-left hover:border-accent hover:bg-hover"
        >
          <span className="font-medium">跳过</span>
          <span className="text-xs text-muted">不传这一项，保留目标位置原有的内容</span>
        </button>
        <button
          type="button"
          onClick={() => resolve({ policy: 'overwrite', applyToAll })}
          className="flex flex-col rounded-lg border border-line px-3.5 py-2.5 text-left hover:border-bad hover:bg-hover"
        >
          <span className="font-medium text-bad">覆盖</span>
          <span className="text-xs text-muted">用新传的内容替换目标位置原有的同名文件</span>
        </button>
      </div>

      <div className="flex justify-end">
        <Button onClick={() => resolve(null)}>取消传输</Button>
      </div>
    </div>
  )
}
