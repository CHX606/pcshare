import type { ShotTargets } from '../../api/types'
import { Button } from '../../components/Modal'
import { QUICK_KEYS } from '../../lib/desktop'

/** 画面来源选择器（供实时桌面 DesktopView 与截图模式 ScreenPanel 复用）。 */
export function SourceSelect({
  targets,
  value,
  onChange,
  fallbackOption = false,
}: {
  targets: ShotTargets | null
  value: string
  onChange: (choice: string) => void
  fallbackOption?: boolean
}) {
  return (
    <select
      aria-label="选择要查看的显示器或窗口"
      value={value}
      onChange={(e) => onChange(e.target.value)}
      className="h-8 max-w-64 rounded-md border border-line bg-panel px-2 text-xs outline-none focus:border-accent"
    >
      {fallbackOption && !targets && value && <option value={value}>显示器</option>}
      {targets?.monitors.map((m) => (
        <option key={`mon:${m.index}`} value={`mon:${m.index}`}>
          {m.name}（缩放 {m.dpiScalePercent}%）
        </option>
      ))}
      {targets?.windows.map((w) => (
        <option key={`win:${w.hwnd}`} value={`win:${w.hwnd}`}>
          窗口：{w.title}（{w.width}×{w.height}）
        </option>
      ))}
    </select>
  )
}

/** 远端文字与常用组合键发送条（供 DesktopView 与 ScreenPanel 复用）。 */
export function RemoteInputBar({
  text,
  onTextChange,
  disabled,
  placeholder,
  onSendText,
  onSendShortcut,
  className = '',
}: {
  text: string
  onTextChange: (text: string) => void
  disabled: boolean
  placeholder: string
  onSendText: (text: string) => void
  onSendShortcut: (combo: string) => void
  className?: string
}) {
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault()
        if (text) onSendText(text)
      }}
      className={`flex flex-wrap items-center gap-2 text-xs ${className}`}
    >
      <input
        value={text}
        disabled={disabled}
        onChange={(e) => onTextChange(e.target.value)}
        placeholder={placeholder}
        aria-label="向对方当前窗口输入文字"
        className="h-8 min-w-48 flex-1 rounded-md border border-line bg-panel px-2.5 text-xs outline-none focus:border-accent disabled:opacity-50"
      />
      <Button type="submit" variant="primary" disabled={disabled || !text}>
        发送文字
      </Button>
      <div className="flex flex-wrap items-center gap-1">
        {QUICK_KEYS.map((k) => (
          <button
            key={k.combo}
            type="button"
            disabled={disabled}
            onClick={() => onSendShortcut(k.combo)}
            className="h-7 rounded border border-line bg-panel px-2 font-mono text-[11px] hover:bg-hover disabled:opacity-40"
          >
            {k.label}
          </button>
        ))}
      </div>
    </form>
  )
}
