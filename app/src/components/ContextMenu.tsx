import { useEffect, useLayoutEffect, useRef } from 'react'
import { Icon, type IconName } from './Icon'

export type MenuItem =
  | { label: string; icon?: IconName; hint?: string; disabled?: boolean; danger?: boolean; onSelect: () => void }
  | 'sep'

export interface MenuState {
  x: number
  y: number
  items: MenuItem[]
  /** 由键盘（菜单键 / Shift+F10）打开时聚焦第一项 */
  fromKeyboard?: boolean
}

/** 右键菜单：popover=auto 提供点外面关闭和 Esc；按指针位置定位并限制在窗口内。 */
export function ContextMenu({ menu, onClose, label }: { menu: MenuState | null; onClose: () => void; label: string }) {
  const ref = useRef<HTMLDivElement>(null)
  const restore = useRef<HTMLElement | null>(null)

  useLayoutEffect(() => {
    const el = ref.current
    if (!el || !menu) return
    restore.current = document.activeElement as HTMLElement | null
    el.style.left = '0px'
    el.style.top = '0px'
    if (!el.matches(':popover-open')) el.showPopover()
    const r = el.getBoundingClientRect()
    const x = Math.min(menu.x, window.innerWidth - r.width - 4)
    const y = menu.y + r.height > window.innerHeight - 4 ? Math.max(4, menu.y - r.height) : menu.y
    el.style.left = `${Math.max(4, x)}px`
    el.style.top = `${y}px`
    const first = el.querySelector<HTMLButtonElement>('button:not(:disabled)')
    if (menu.fromKeyboard) first?.focus()
    else el.focus({ preventScroll: true })
  }, [menu])

  useEffect(() => {
    const el = ref.current
    if (!el) return
    const onToggle = (e: Event) => {
      // 关闭事件是异步派发的：如果期间又打开了新菜单，就不要把它清掉
      if ((e as ToggleEvent).newState === 'closed' && !el.matches(':popover-open')) onClose()
    }
    el.addEventListener('toggle', onToggle)
    return () => el.removeEventListener('toggle', onToggle)
  }, [onClose])

  const close = () => {
    ref.current?.hidePopover()
    restore.current?.focus({ preventScroll: true })
  }

  const onKeyDown = (e: React.KeyboardEvent) => {
    const items = [...(ref.current?.querySelectorAll<HTMLButtonElement>('button:not(:disabled)') ?? [])]
    const i = items.indexOf(document.activeElement as HTMLButtonElement)
    let next = -1
    if (e.key === 'ArrowDown') next = (i + 1) % items.length
    else if (e.key === 'ArrowUp') next = (i - 1 + items.length) % items.length
    else if (e.key === 'Home') next = 0
    else if (e.key === 'End') next = items.length - 1
    else if (e.key === 'Escape') {
      e.preventDefault()
      return close()
    } else if (e.key === 'Tab') return close()
    if (next >= 0) {
      e.preventDefault()
      items[next]?.focus()
    }
  }

  return (
    <div ref={ref} popover="auto" className="ctx-menu" tabIndex={-1} onKeyDown={onKeyDown} onContextMenu={(e) => e.preventDefault()}>
      <div role="menu" aria-label={label}>
        {menu?.items.map((item, i) =>
          item === 'sep' ? (
            <hr key={i} />
          ) : (
            <button
              key={i}
              type="button"
              role="menuitem"
              disabled={item.disabled}
              className={item.danger ? 'danger' : ''}
              onPointerMove={(e) => (e.currentTarget as HTMLButtonElement).focus({ preventScroll: true })}
              onClick={() => {
                close()
                item.onSelect()
              }}
            >
              <span className="flex w-4 justify-center">{item.icon && <Icon name={item.icon} size={15} />}</span>
              {item.label}
              {item.hint && (
                <span className="hint" aria-hidden="true">
                  {item.hint}
                </span>
              )}
            </button>
          ),
        )}
      </div>
    </div>
  )
}
