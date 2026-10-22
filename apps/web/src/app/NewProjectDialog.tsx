/**
 * New-project entry: only the directory is asked for. A project is derived from
 * the sessions that live in a working directory, so this validates the path and
 * opens the composing view; it creates no session and no project record.
 */

import { useEffect, useRef, useState } from 'react'
import { api } from '../data/api.ts'
import { Button, Modal } from '../ui/primitives/index.ts'
import css from './NewProjectDialog.module.css'

export interface NewProjectDialogProps {
  /** Path the dialog starts from (the last project, when one is known). */
  seedPath: string
  onClose: () => void
  /** Open the composing view for a validated directory. */
  onReady: (project: { cwd: string; name: string }) => void
}

/**
 * Render the new-project dialog.
 * @param props - seed path, close and ready callbacks.
 * @returns the dialog element tree.
 */
export function NewProjectDialog({ seedPath, onClose, onReady }: NewProjectDialogProps) {
  const [path, setPath] = useState(seedPath)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | undefined>(undefined)
  const alive = useRef(true)
  useEffect(() => () => { alive.current = false }, [])
  const directory = path.trim()

  const submit = async (): Promise<void> => {
    if (directory === '' || busy) return
    setBusy(true)
    setError(undefined)
    try {
      const response = await api.workspace(directory)
      if (response.code !== 200) {
        setError(response.message || `code ${response.code}`)
        return
      }
      if (!alive.current) return
      onReady({ cwd: response.data.cwd, name: response.data.name })
    } catch (cause) {
      if (!alive.current) return
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      if (alive.current) setBusy(false)
    }
  }

  return (
    <Modal
      open
      title="新建项目"
      closeLabel="关闭新建项目"
      className={css.dialogSize}
      description="项目由工作目录派生；首次发送后才会创建会话。"
      onClose={onClose}
      footer={(
        <>
          <Button size="sm" variant="ghost" disabled={busy} onClick={onClose}>取消</Button>
          <Button
            size="sm"
            variant="primary"
            type="submit"
            form="mink-new-project"
            disabled={busy || directory === ''}
          >
            {busy ? '正在检查…' : '打开目录'}
          </Button>
        </>
      )}
    >
      <form
        id="mink-new-project"
        className={css.form}
        onSubmit={(event) => {
          event.preventDefault()
          void submit()
        }}
      >
        <label className={css.field}>
          <span className={css.label}>工作目录</span>
          <input
            className={css.input}
            value={path}
            placeholder="/path/to/project"
            aria-label="工作目录"
            autoComplete="off"
            spellCheck={false}
            data-modal-autofocus
            onChange={(event) => { setPath(event.target.value) }}
          />
        </label>
        {error !== undefined && <p className={css.error} role="alert">{error}</p>}
      </form>
    </Modal>
  )
}
