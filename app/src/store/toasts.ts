import { create } from 'zustand'

export type ToastKind = 'info' | 'success' | 'warn' | 'error'
export interface Toast {
  id: number
  kind: ToastKind
  text: string
  at: number
}

interface ToastState {
  toasts: Toast[]
  history: Toast[]
  focusInside: boolean
  push(kind: ToastKind, text: string): void
  dismiss(id: number): void
  setFocusInside(inside: boolean): void
}

let seq = 0
const LIFETIME: Record<ToastKind, number> = { info: 4000, success: 4000, warn: 7000, error: 9000 }
const pendingDismissIds = new Set<number>()

export const useToasts = create<ToastState>()((set, get) => ({
  toasts: [],
  history: [],
  focusInside: false,
  push(kind, text) {
    const t: Toast = { id: ++seq, kind, text, at: Date.now() }
    set((s) => ({ toasts: [...s.toasts.slice(-3), t], history: [t, ...s.history].slice(0, 50) }))
    window.setTimeout(() => {
      if (get().focusInside) {
        pendingDismissIds.add(t.id)
      } else {
        get().dismiss(t.id)
      }
    }, LIFETIME[kind])
  },
  dismiss(id) {
    pendingDismissIds.delete(id)
    set((s) => {
      const toasts = s.toasts.filter((t) => t.id !== id)
      if (toasts.length === 0) {
        pendingDismissIds.clear()
        return { toasts, focusInside: false }
      }
      return { toasts }
    })
  },
  setFocusInside(inside) {
    set({ focusInside: inside })
    if (!inside && pendingDismissIds.size > 0) {
      const ids = [...pendingDismissIds]
      pendingDismissIds.clear()
      for (const id of ids) get().dismiss(id)
    }
  },
}))

export const toast = {
  info: (t: string) => useToasts.getState().push('info', t),
  success: (t: string) => useToasts.getState().push('success', t),
  warn: (t: string) => useToasts.getState().push('warn', t),
  error: (t: string) => useToasts.getState().push('error', t),
}
