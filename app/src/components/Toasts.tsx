import { useEffect, useRef } from 'react'
import { useToasts, type ToastKind } from '../store/toasts'
import { Icon, type IconName } from './Icon'

const STYLE: Record<ToastKind, { icon: IconName; color: string }> = {
  info: { icon: 'info', color: 'text-accent' },
  success: { icon: 'check', color: 'text-ok' },
  warn: { icon: 'alert', color: 'text-warn' },
  error: { icon: 'alert', color: 'text-bad' },
}

/** 右下角提示。popover=manual：不会因为点了别处而消失，多条可以叠放；模态框打开期间暂缓显示，焦点在内部时暂停自动消失。 */
export function Toasts() {
  const toasts = useToasts((s) => s.toasts)
  const dismiss = useToasts((s) => s.dismiss)
  const setFocusInside = useToasts((s) => s.setFocusInside)
  const host = useRef<HTMLDivElement>(null)
  const live = useRef<HTMLDivElement>(null)
  const last = toasts[toasts.length - 1]

  useEffect(() => {
    const el = host.current
    if (!el) return
    const syncVisibility = () => {
      const hasModal = document.querySelector('dialog:modal') !== null
      try {
        if (hasModal || toasts.length === 0) {
          if (el.matches(':popover-open')) el.hidePopover()
        } else if (!el.matches(':popover-open')) {
          el.showPopover()
        }
      } catch {
        // Ignore in headless test DOM without Popover API
      }
    }
    syncVisibility()
    const obs = new MutationObserver(syncVisibility)
    obs.observe(document.body, { subtree: true, attributes: true, attributeFilter: ['open'] })
    return () => obs.disconnect()
  }, [toasts.length])

  useEffect(() => {
    if (!last) return
    if (document.querySelector('dialog:modal') !== null) return
    const doc = document as Document & { ariaNotify?: (text: string) => void }
    if (typeof doc.ariaNotify === 'function') doc.ariaNotify(last.text)
    else if (live.current) live.current.textContent = last.text
  }, [last])

  return (
    <>
      <div
        ref={host}
        popover="manual"
        className="toast-host"
        role="region"
        aria-label="通知"
        onFocus={() => setFocusInside(true)}
        onBlur={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget as Node | null)) {
            setFocusInside(false)
          }
        }}
      >
        {toasts.map((t) => (
          <div
            key={t.id}
            className="toast-item flex max-w-[420px] items-start gap-2 rounded-lg border border-line bg-panel px-3 py-2.5 text-sm shadow-lg"
          >
            <Icon name={STYLE[t.kind].icon} className={`mt-0.5 shrink-0 ${STYLE[t.kind].color}`} />
            <p className="selectable flex-1 leading-relaxed">{t.text}</p>
            <button
              type="button"
              aria-label="关闭提示"
              className="rounded p-0.5 text-faint hover:text-fg"
              onClick={(e) => {
                const container = host.current
                const wasFocused = container?.contains(document.activeElement)
                dismiss(t.id)
                if (wasFocused) {
                  requestAnimationFrame(() => {
                    const nextBtn = container?.querySelector<HTMLButtonElement>('button')
                    nextBtn?.focus()
                  })
                }
                e.stopPropagation()
              }}
            >
              <Icon name="x" size={14} />
            </button>
          </div>
        ))}
      </div>
      <div ref={live} aria-live="polite" className="sr-only" />
    </>
  )
}
