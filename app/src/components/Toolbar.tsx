import { useEffect, useRef, type HTMLAttributes, type ReactNode } from 'react'

interface ToolbarProps extends HTMLAttributes<HTMLDivElement> {
  label: string
  children: ReactNode
}

/**
 * 复合工具栏组件：单一 Tab 停靠点，方向键与 Home/End 在控件间循环移动。
 * 原生支持 focusgroup 时直接交由浏览器处理；不支持时按规范启用 roving tabindex 降级。
 */
export function Toolbar({ label, className = '', children, ...rest }: ToolbarProps) {
  const ref = useRef<HTMLDivElement>(null)

  useEffect(() => {
    const bar = ref.current
    if (!bar) return
    if ('focusGroup' in HTMLElement.prototype) return

    const getItems = () =>
      Array.from(bar.querySelectorAll<HTMLElement>('button, select, [tabindex]')).filter(
        (el) => !el.hasAttribute('disabled'),
      )

    const initRoving = () => {
      const items = getItems()
      if (!items.length) return
      const hasZero = items.some((el) => el.tabIndex === 0)
      if (!hasZero) {
        items.forEach((el, i) => {
          el.tabIndex = i === 0 ? 0 : -1
        })
      }
    }
    initRoving()

    const onFocusIn = (e: FocusEvent) => {
      const target = e.target as HTMLElement | null
      const items = getItems()
      if (target && items.includes(target)) {
        items.forEach((el) => {
          el.tabIndex = el === target ? 0 : -1
        })
      }
    }

    const onKeyDown = (e: KeyboardEvent) => {
      const items = getItems()
      if (!items.length) return
      const idx = items.indexOf(document.activeElement as HTMLElement)
      if (idx < 0) return

      const vertical = bar.getAttribute('aria-orientation') === 'vertical'
      const rtl = getComputedStyle(bar).direction === 'rtl'
      let nextIdx = -1

      if (e.key === 'Home') nextIdx = 0
      else if (e.key === 'End') nextIdx = items.length - 1
      else if (vertical && e.key === 'ArrowDown') nextIdx = (idx + 1) % items.length
      else if (vertical && e.key === 'ArrowUp') nextIdx = (idx - 1 + items.length) % items.length
      else if (!vertical && e.key === 'ArrowRight')
        nextIdx = (idx + (rtl ? -1 : 1) + items.length) % items.length
      else if (!vertical && e.key === 'ArrowLeft')
        nextIdx = (idx + (rtl ? 1 : -1) + items.length) % items.length

      if (nextIdx >= 0) {
        e.preventDefault()
        items.forEach((el, i) => {
          el.tabIndex = i === nextIdx ? 0 : -1
        })
        items[nextIdx].focus()
      }
    }

    bar.addEventListener('focusin', onFocusIn)
    bar.addEventListener('keydown', onKeyDown)
    return () => {
      bar.removeEventListener('focusin', onFocusIn)
      bar.removeEventListener('keydown', onKeyDown)
    }
  }, [children])

  return (
    <div
      ref={ref}
      role="toolbar"
      aria-label={label}
      {...({ focusgroup: 'toolbar wrap' } as object)}
      className={`app-toolbar flex flex-wrap items-center gap-1.5 ${className}`}
      {...rest}
    >
      {children}
    </div>
  )
}
