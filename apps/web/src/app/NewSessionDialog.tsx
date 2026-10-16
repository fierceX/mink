/**
 * New-task dialog: a session is created inside a working directory on the
 * machine running the server, so the directory is an input rather than a pick
 * from the sessions already discovered. The first session in a brand-new
 * directory is created from here, and existing projects act as candidates the
 * field offers while still accepting any path. A refusal keeps what the reader
 * typed so they can fix the path instead of starting over.
 */

import { useState } from 'react'
import { Button, Modal } from '../ui/primitives/index.ts'
import css from './NewSessionDialog.module.css'

/** One discovered project, offered as a candidate working directory. */
export interface ProjectCandidate {
  cwd: string
  name: string
}

/**
 * Render the new-task dialog.
 * @param props.projects - discovered projects to offer as candidates.
 * @param props.seedCwd - working directory the dialog starts from.
 * @param props.busy - the create request is in flight.
 * @param props.error - why the last create was refused, when it was.
 * @param props.onClose - dismiss the dialog.
 * @param props.onCreate - create a session in the typed directory.
 * @returns the dialog element tree.
 */
export function NewSessionDialog({ projects, seedCwd, busy, error, onClose, onCreate }: {
  projects: readonly ProjectCandidate[]
  seedCwd: string
  busy: boolean
  error: string | undefined
  onClose: () => void
  onCreate: (cwd: string, name: string) => void
}) {
  const [cwd, setCwd] = useState(seedCwd)
  const [name, setName] = useState('')
  const directory = cwd.trim()
  const submit = (): void => {
    if (directory === '' || busy) return
    onCreate(directory, name.trim())
  }

  return (
    <Modal
      open
      title="新建任务"
      closeLabel="关闭新建任务"
      className={css.dialogSize}
      description="工作目录位于 server 所在机器。"
      onClose={onClose}
      footer={(
        <>
          <Button size="sm" variant="ghost" disabled={busy} onClick={onClose}>取消</Button>
          <Button
            size="sm"
            variant="primary"
            type="submit"
            form="mink-new-session"
            disabled={busy || directory === ''}
          >
            {busy ? '正在创建…' : '创建'}
          </Button>
        </>
      )}
    >
      <form
        id="mink-new-session"
        className={css.form}
        onSubmit={(event) => {
          event.preventDefault()
          submit()
        }}
      >
        <label className={css.field}>
          <span className={css.label}>工作目录</span>
          <input
            className={css.input}
            value={cwd}
            list="mink-new-session-projects"
            placeholder="/path/to/project"
            aria-label="工作目录"
            autoComplete="off"
            spellCheck={false}
            data-modal-autofocus
            onChange={(event) => { setCwd(event.target.value) }}
          />
        </label>
        {/* Existing projects are candidates, not a boundary: any path the
            server can reach is accepted, which is what makes the first session
            in a new directory possible from the browser. */}
        <datalist id="mink-new-session-projects">
          {projects.map(project => <option key={project.cwd} value={project.cwd} />)}
        </datalist>
        <label className={css.field}>
          <span className={css.label}>任务名称（可选）</span>
          <input
            className={css.input}
            value={name}
            placeholder="例如：调整恢复策略"
            aria-label="任务名称"
            autoComplete="off"
            onChange={(event) => { setName(event.target.value) }}
          />
        </label>
        {error !== undefined && <p className={css.error} role="alert">{error}</p>}
      </form>
    </Modal>
  )
}
