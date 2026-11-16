import { useUi } from '../store/ui'
import { Button, Modal } from './Modal'

/** 全局确认框，由 confirmDialog() 调起。危险操作默认聚焦在“取消”上。 */
export function ConfirmHost() {
  const c = useUi((s) => s.confirm)
  return (
    <Modal open={!!c} onClose={() => c?.resolve(false)} title={c?.title ?? ''} width={420}>
      {c && (
        <form
          method="dialog"
          onSubmit={(e) => {
            e.preventDefault()
            const submitter = (e.nativeEvent as SubmitEvent).submitter as HTMLButtonElement | null
            c.resolve(submitter?.value === 'ok')
          }}
        >
          {c.body && <p className="mb-5 text-sm leading-relaxed text-muted">{c.body}</p>}
          <div className="flex justify-end gap-2">
            <Button type="submit" value="cancel" data-autofocus={c.danger || undefined}>
              取消
            </Button>
            <Button type="submit" value="ok" variant={c.danger ? 'danger' : 'primary'} data-autofocus={!c.danger || undefined}>
              {c.okText ?? '确定'}
            </Button>
          </div>
        </form>
      )}
    </Modal>
  )
}
