import { useEffect, useId, useRef, type ReactNode } from 'react'
import { Icon } from './Icon'

interface ModalProps {
  open: boolean
  onClose: () => void
  title: string
  children: ReactNode
  /** any：点外面也关；closerequest：只有 Esc 和按钮能关 */
  closedBy?: 'any' | 'closerequest'
  /** false 时禁用关闭按钮与 Esc / 外部关闭（如提交请求进行中）。 */
  dismissible?: boolean
  width?: number
}

/** 原生 <dialog> 的模态框：浏览器负责焦点、Esc、背景不可操作。 */
export function Modal({
  open,
  onClose,
  title,
  children,
  closedBy = 'closerequest',
  dismissible = true,
  width = 460,
}: ModalProps) {
  const ref = useRef<HTMLDialogElement>(null)
  const titleId = useId()

  useEffect(() => {
    const d = ref.current
    if (!d) return
    if (open && !d.open) {
      d.showModal()
      // React 不输出 autofocus 属性，打开后手动聚焦标了 data-autofocus 的控件（默认会落在右上角关闭按钮上）
      d.querySelector<HTMLElement>('[data-autofocus]')?.focus()
    }
    if (!open && d.open) d.close()
  }, [open])

  // 兼容尚不支持原生 closedby="any" 的内核：按对话框外框坐标判断按下与抬起均在外部时关闭
  useEffect(() => {
    const d = ref.current
    if (!d || !open || closedBy !== 'any' || !dismissible) return
    if ('closedBy' in HTMLDialogElement.prototype) return
    const outside = (e: PointerEvent) => {
      const r = d.getBoundingClientRect()
      return e.clientX < r.left || e.clientX > r.right || e.clientY < r.top || e.clientY > r.bottom
    }
    let pressedOutside = false
    const onDown = (e: PointerEvent) => {
      pressedOutside = outside(e)
    }
    const onUp = (e: PointerEvent) => {
      if (pressedOutside && outside(e)) d.close()
    }
    d.addEventListener('pointerdown', onDown)
    d.addEventListener('pointerup', onUp)
    return () => {
      d.removeEventListener('pointerdown', onDown)
      d.removeEventListener('pointerup', onUp)
    }
  }, [open, closedBy, dismissible])

  const effectiveClosedBy = dismissible ? closedBy : 'none'

  return (
    <dialog
      ref={ref}
      aria-labelledby={titleId}
      onCancel={(e) => {
        if (!dismissible) e.preventDefault()
      }}
      onClose={() => {
        if (open && dismissible) onClose()
      }}
      {...({ closedby: effectiveClosedBy } as object)}
      style={{ width }}
    >
      {open && (
        <div className="flex flex-col">
          <header className="flex items-center justify-between px-5 pt-4 pb-2">
            <h2 id={titleId} className="text-base font-semibold">
              {title}
            </h2>
            <button
              type="button"
              aria-label="关闭"
              disabled={!dismissible}
              className="rounded p-1 text-muted hover:bg-hover hover:text-fg disabled:cursor-not-allowed disabled:opacity-40"
              onClick={() => dismissible && onClose()}
            >
              <Icon name="x" />
            </button>
          </header>
          <div className="px-5 pb-5">{children}</div>
        </div>
      )}
    </dialog>
  )
}

export function Button({
  variant = 'default',
  className = '',
  ...rest
}: React.ButtonHTMLAttributes<HTMLButtonElement> & { variant?: 'default' | 'primary' | 'danger' | 'ghost' }) {
  const styles = {
    default: 'border border-line bg-panel hover:bg-hover',
    primary: 'bg-accent text-accent-fg hover:brightness-110 border border-transparent',
    danger: 'bg-bad text-white hover:brightness-110 border border-transparent',
    ghost: 'hover:bg-hover border border-transparent',
  }[variant]
  return (
    <button
      type="button"
      className={`inline-flex h-8 items-center justify-center gap-1.5 rounded-md px-3 text-sm disabled:cursor-not-allowed disabled:opacity-50 ${styles} ${className}`}
      {...rest}
    />
  )
}
