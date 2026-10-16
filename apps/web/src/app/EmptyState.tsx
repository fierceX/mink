/**
 * Empty state: no session is open yet. It carries the same brand mark the hero
 * uses, one line saying what to do, and the start action the sidebar offers —
 * a session is created in a working directory the reader types (the discovered
 * projects are candidates), so this state can start even when nothing has been
 * discovered yet.
 */

import { Button, IconPlusOutlineMedium } from '../ui/primitives/index.ts'
import { MinkMark } from './brand/MinkMark.tsx'
import type { ProjectGroup } from './sidebar/SessionBrowser.tsx'
import css from './EmptyState.module.css'

/**
 * Render the empty state.
 * @param props.project - the project a new session would go into, when one is known.
 * @param props.notice - load failure or command notice to report.
 * @param props.onCreate - open the new-task dialog, seeded from that project.
 * @returns the state element tree.
 */
export function EmptyState({ project, notice, onCreate }: {
  project: ProjectGroup | undefined
  notice: string | undefined
  onCreate: (project: ProjectGroup | undefined) => void
}) {
  return (
    <div className={css.root} data-empty-state>
      <div className={css.stack}>
        <span className={css.mark}><MinkMark size={38} /></span>
        <h1 className={css.title}>今天想让 Mink 做什么？</h1>
        <p className={css.note}>
          {project === undefined
            ? '还没有发现任何项目：新建任务时输入 server 上的工作目录即可。'
            : `从左侧选择 ${project.name} 里的会话，或在这里新建一个。`}
        </p>
        <Button
          variant="primary"
          className={css.start}
          onClick={() => { onCreate(project) }}
        >
          <IconPlusOutlineMedium size={14} />
          {project === undefined ? '新建任务' : `在 ${project.name} 新建任务`}
        </Button>
        {notice !== undefined && <p className={css.notice} role="status">{notice}</p>}
      </div>
    </div>
  )
}
