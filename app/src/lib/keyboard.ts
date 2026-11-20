/**
 * 判断键盘事件是否处于输入法组字阶段（IME composing），
 * 避免中文输入法回车选词时误触发启动/提交。
 */
export function isImeComposing(e: KeyboardEvent | { nativeEvent: KeyboardEvent; keyCode?: number }): boolean {
  const native = 'nativeEvent' in e ? e.nativeEvent : e
  return native.isComposing || native.key === 'Process' || native.keyCode === 229
}

/**
 * 判断全局 window 键盘快捷键是否应让位给远控画布或终端区域，
 * 避免用户在远控或终端中按 Ctrl+1..5 / Ctrl+K 时误切页签或弹出快捷搜索。
 */
export function shouldSkipGlobalShortcut(e: KeyboardEvent): boolean {
  if (e.defaultPrevented) return true
  const isCaptured = (node: EventTarget | null) =>
    node instanceof Element && node.closest('[data-capture-keys="true"]') !== null
  return isCaptured(e.target) || isCaptured(document.activeElement)
}
