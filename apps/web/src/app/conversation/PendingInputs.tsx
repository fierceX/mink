import { requestId } from '../../data/request-id.ts'
/**
 * Pending inputs (guidance): the queue that binds to the running turn. Receipts
 * come from the session projection (`inputs_updated` / snapshot), every change
 * is a revision compare-and-set, and withdrawing or editing is refused once the
 * server has started applying the input — the strip hides those verbs instead of
 * offering a call that must fail.
 */

import { useCallback, useState } from 'react'
import type { HumanInput, InputReceipt } from '../../data/types.ts'
import { api, type SessionSummary } from '../../data/api.ts'
import { Button } from '../../ui/primitives/index.ts'
import css from './PendingInputs.module.css'

/** Guidance requests carry the turn they bind to; a null target is a fresh turn. */
export function guidanceRequest(text: string, targetTurnId: string | null): HumanInput {
  return {
    request_id: requestId(),
    text,
    target_turn_id: targetTurnId,
    attachment_ids: [],
  }
}

/** Status wording for one receipt. */
function statusLabel(status: InputReceipt['status']): string {
  switch (status) {
    case 'pending': return '待应用'
    case 'applying': return '应用中'
    case 'applied': return '已应用'
    case 'unapplied': return '未应用'
    case 'withdrawn': return '已撤回'
  }
}

/**
 * Render the pending-input strip.
 * @param props.receipts - the session's input receipts, in queue order.
 * @param props.session - the session they belong to.
 * @param props.onChanged - reload the projection after a server-side change.
 * @returns the strip, or null when the queue is empty.
 */
export function PendingInputs({ receipts, session, onChanged }: {
  receipts: readonly InputReceipt[]
  session: SessionSummary
  onChanged: (receipt: InputReceipt) => void
}) {
  const [editing, setEditing] = useState<string | undefined>(undefined)
  const [draft, setDraft] = useState('')
  const [busy, setBusy] = useState<string | undefined>(undefined)
  const [error, setError] = useState<string | undefined>(undefined)

  const change = useCallback((receipt: InputReceipt, action: 'edit' | 'withdraw' | 'resume') => {
    if (busy !== undefined) return
    setBusy(receipt.input_id)
    setError(undefined)
    // One async body owns both failure kinds: a rejected call and a business
    // refusal (a non-200 code in a 2xx envelope) both say why in place.
    void (async () => {
      try {
        const response = action === 'edit'
          ? await api.editInput(session.id, receipt, draft, session.project_key)
          : action === 'withdraw'
            ? await api.withdrawInput(session.id, receipt, session.project_key)
            : await api.resumeInput(session.id, receipt, session.project_key)
        if (response.code !== 200) {
          setError(response.message || `code ${response.code}`)
          return
        }
        setEditing(undefined)
        setDraft('')
        onChanged(response.data)
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause))
      }
    })().finally(() => {
      setBusy(undefined)
    })
  }, [busy, draft, onChanged, session])

  if (receipts.length === 0) return null

  return (
    <div className={css.pending} data-pending-inputs={receipts.length}>
      {receipts.map((receipt) => {
        const changeable = receipt.status === 'pending' || receipt.status === 'unapplied'
        return (
          <div key={receipt.input_id} className={css.entry} data-input-status={receipt.status}>
            <div className={css.entryHead}>
              <span className={css.status}>{statusLabel(receipt.status)}</span>
              <span className={css.text}>{receipt.input.text}</span>
            </div>
            {editing === receipt.input_id
              ? (
                <div className={css.editRow}>
                  <input
                    className={css.editInput}
                    value={draft}
                    aria-label="编辑引导"
                    onChange={(event) => { setDraft(event.target.value) }}
                  />
                  <Button
                    size="sm"
                    variant="primary"
                    disabled={busy !== undefined || draft.trim() === ''}
                    onClick={() => { change(receipt, 'edit') }}
                  >
                    保存
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    onClick={() => {
                      setEditing(undefined)
                      setDraft('')
                    }}
                  >
                    取消
                  </Button>
                </div>
              )
              : changeable && (
                <div className={css.actions}>
                  <Button
                    size="sm"
                    variant="ghost"
                    disabled={busy !== undefined}
                    onClick={() => {
                      setEditing(receipt.input_id)
                      setDraft(receipt.input.text)
                    }}
                  >
                    编辑
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    disabled={busy !== undefined}
                    onClick={() => { change(receipt, 'withdraw') }}
                  >
                    撤回
                  </Button>
                </div>
              )}
            {receipt.status === 'unapplied' && (
              <div className={css.actions}>
                <Button
                  size="sm"
                  variant="outline"
                  disabled={busy !== undefined}
                  onClick={() => { change(receipt, 'resume') }}
                >
                  用于下一轮
                </Button>
              </div>
            )}
          </div>
        )
      })}
      {error !== undefined && <p className={css.error} role="alert">{error}</p>}
    </div>
  )
}
