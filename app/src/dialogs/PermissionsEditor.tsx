import type { Permissions } from '../api/types'

const ROWS: Array<{ key: keyof Permissions; label: string; desc: string; risky?: boolean }> = [
  { key: 'readFiles', label: '读文件', desc: '浏览文件夹、下载文件' },
  { key: 'writeFiles', label: '写文件', desc: '新建、重命名、移动、删除、上传' },
  { key: 'apps', label: '开软件、管进程', desc: '打开软件、结束卡死的程序' },
  { key: 'screenshot', label: '截图', desc: '看这台电脑的屏幕', risky: true },
  { key: 'input', label: '点按', desc: '替你在这台电脑上点击和打字', risky: true },
  { key: 'terminal', label: '命令行', desc: '在这台电脑上执行任意命令', risky: true },
  { key: 'power', label: '电源', desc: '关机、重启、睡眠、锁屏', risky: true },
  { key: 'clipboard', label: '剪贴板', desc: '读写这台电脑的剪贴板', risky: true },
]

/** 权限档（第一册 7.3 节）。高风险项默认关闭并标注。 */
export function PermissionsEditor({ value, onChange }: { value: Permissions; onChange: (p: Permissions) => void }) {
  return (
    <fieldset className="rounded-lg border border-line">
      <legend className="sr-only">权限</legend>
      {ROWS.map((r, i) => (
        <label key={r.key} className={`flex cursor-pointer items-center gap-3 px-3 py-2 hover:bg-hover ${i ? 'border-t border-line' : ''}`}>
          <input
            type="checkbox"
            checked={value[r.key]}
            onChange={(e) => onChange({ ...value, [r.key]: e.target.checked })}
            className="size-4 accent-[var(--color-accent)]"
          />
          <span className="flex-1">
            <span className="text-sm">{r.label}</span>
            {r.risky && <span className="ml-2 rounded bg-warn/15 px-1.5 py-0.5 text-[11px] text-warn">高风险</span>}
            <span className="block text-xs text-muted">{r.desc}</span>
          </span>
        </label>
      ))}
    </fieldset>
  )
}
