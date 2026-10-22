/**
 * Sidebar session browser: project groups over session rows. Markup, class
 * hookups and the title marquee come from the DSH `ui-workspace` rows
 * (WorkspaceBrowser + Rows); the tree is fed by mink's own session catalog, so
 * the slot renderers, drag-reorder wiring and row-entry animation are replaced
 * by direct props and left out for now.
 */

import clsx from 'clsx'
import { useEffect, useMemo, useRef, useState } from 'react'
import type { RefObject } from 'react'
import {
  IconCloseOutlineRegular,
  IconEllipsisOutlineRegular,
  IconFolderCloseRegular,
  IconFolderOpenRegular,
  IconNewChatOutlineRegular,
  IconSearchOutlineRegular,
  IconTrashOutlineRegular,
  IconTriangleRightFillRegular,
  Menu,
  MenuItemButton,
  SegmentedControl,
  StateDot,
  Tooltip,
  relativeTime,
} from '../../ui/primitives/index.ts'
import type { SessionSummary } from '../../data/api.ts'
import { matchesSession, projectName, type FilterScope } from '../../data/catalogFilter.ts'
import css from './SessionBrowser.module.css'
import rowCss from './Rows.module.css'

/** One project group: every discovered session whose working directory matches. */
export interface ProjectGroup {
  /** Working directory that identifies the project. */
  cwd: string
  /** Display name: the directory's last segment. */
  name: string
  sessions: SessionSummary[]
}

/* Overflow this small hides no meaningful tail; scrolling for it reads as an
   accidental jitter, so the title stays put. */
const MIN_TITLE_REVEAL_PX = 8

/* Marquee travel speed: slow enough to read the text as it passes. */
const TITLE_MARQUEE_PX_PER_MS = 0.03

/**
 * Place the title's scroll position and publish the stylesheet's fade-mask
 * hooks: `data-scrolled` while the title has left its start (left fade) and
 * `data-clipped` while text remains beyond the right edge (right fade).
 * @param title - the row's clipping title element.
 * @param left - scroll offset in CSS pixels.
 * @param range - the title's maximum scroll offset in CSS pixels.
 */
function placeTitle(title: HTMLSpanElement, left: number, range: number): void {
  if (typeof title.scrollTo === 'function') title.scrollTo({ left, behavior: 'instant' })
  else title.scrollLeft = left
  if (left > 0) title.dataset.scrolled = ''
  else delete title.dataset.scrolled
  if (left < range) title.dataset.clipped = ''
  else delete title.dataset.clipped
}

/**
 * Return the title to its resting state: scrolled to the start with both fade
 * masks off, so the resting ellipsis renders at full strength.
 * @param title - the row's clipping title element.
 */
function restTitle(title: HTMLSpanElement): void {
  if (typeof title.scrollTo === 'function') title.scrollTo({ left: 0, behavior: 'instant' })
  else title.scrollLeft = 0
  delete title.dataset.scrolled
  delete title.dataset.clipped
}

/**
 * Marquee a title wider than its one-line cell while its row is hovered.
 * @param title - ref to the row's clipping title element.
 * @returns stable pointer enter/leave handlers for the row.
 */
function useTitleMarquee(title: RefObject<HTMLSpanElement | null>): { enter: () => void; leave: () => void } {
  const frame = useRef(0)
  useEffect(() => () => { cancelAnimationFrame(frame.current) }, [])
  return useMemo(() => ({
    enter: (): void => {
      if (title.current === null) return
      const element = title.current
      const range = element.scrollWidth - element.clientWidth
      if (range <= MIN_TITLE_REVEAL_PX) return
      if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) {
        placeTitle(element, range, range)
        return
      }
      cancelAnimationFrame(frame.current)
      let previous: number | undefined
      let position = 0
      const step = (now: DOMHighResTimeStamp): void => {
        position += previous === undefined ? 0 : (now - previous) * TITLE_MARQUEE_PX_PER_MS
        previous = now
        placeTitle(element, Math.min(position, range), range)
        if (position < range) frame.current = requestAnimationFrame(step)
      }
      frame.current = requestAnimationFrame(step)
    },
    leave: (): void => {
      cancelAnimationFrame(frame.current)
      if (title.current === null) return
      restTitle(title.current)
    },
  }), [title])
}

/** Compact relative time in the workbench's own words. */
function timeLabel(at: number, now: number): string {
  const { unit, n } = relativeTime(at, now)
  switch (unit) {
    case 'now': return '刚刚'
    case 'minutes': return `${n} 分钟`
    case 'hours': return `${n} 小时`
    case 'days': return `${n} 天`
    case 'months': return `${n} 个月`
    case 'years': return `${n} 年`
  }
}

/**
 * The relative stamp a row shows, from the session's own timestamps.
 * @param session - the row's session.
 * @param now - current epoch ms (injected so tests are deterministic).
 * @returns the workbench's compact relative label.
 */
export function rowTimeLabel(session: SessionSummary, now: number): string {
  return timeLabel(updatedAtMs(session, now), now)
}
export function groupSessions(sessions: readonly SessionSummary[]): ProjectGroup[] {
  const byCwd = new Map<string, ProjectGroup>()
  for (const session of sessions) {
    const cwd = session.cwd === '' ? '未记录工作目录' : session.cwd
    const existing = byCwd.get(cwd)
    if (existing === undefined) {
      byCwd.set(cwd, { cwd, name: projectName(cwd), sessions: [session] })
    } else {
      existing.sessions.push(session)
    }
  }
  return [...byCwd.values()]
}

/**
 * Row label: the session's own title, its alias, the first prompt it recorded
 * (the source labels a session by the request that opened it), or the id as the
 * last resort. A long prompt is cut so the row keeps its shape.
 */
export function sessionTitle(session: SessionSummary): string {
  const title = session.title?.trim()
  if (title !== undefined && title !== '') return title
  const prompt = session.first_prompt?.trim()
  if (prompt !== undefined && prompt !== '') {
    return prompt.length > 60 ? `${prompt.slice(0, 60)}…` : prompt
  }
  if (session.untitled === true) return '新对话'
  return session.alias ?? session.id
}

/**
 * Update stamp in epoch ms. `updated_at` wins when the session recorded one;
 * `modified_secs` is the store's absolute last-modified time in epoch seconds
 * (not an age), and a row the store could not stat falls back to "now".
 */
function updatedAtMs(session: SessionSummary, now: number): number {
  const parsed = Date.parse(session.updated_at)
  if (!Number.isNaN(parsed)) return parsed
  return session.modified_secs === null ? now : session.modified_secs * 1000
}

/** One session row: status dot, title, relative time, actions menu. */
function SessionRow({ session, selected, now, onOpen, onDelete }: {
  session: SessionSummary
  selected: boolean
  now: number
  onOpen: (session: SessionSummary) => void
  onDelete: (session: SessionSummary) => void
}) {
  const [menuOpen, setMenuOpen] = useState(false)
  const titleRef = useRef<HTMLSpanElement>(null)
  const marquee = useTitleMarquee(titleRef)
  const running = session.status === 'running'
  const title = sessionTitle(session)
  return (
    <div
      data-row-key={`session:${session.id}`}
      data-status={session.status}
      className={clsx(rowCss.sessionRow, selected && rowCss.selected, menuOpen && rowCss.menuOpen)}
      role="treeitem"
      aria-selected={selected}
      onClick={() => { onOpen(session) }}
      onPointerEnter={marquee.enter}
      onPointerLeave={marquee.leave}
    >
      <span className={rowCss.slot}>
        {running && <StateDot state="ongoing" />}
      </span>
      <span ref={titleRef} className={rowCss.title} title={title}>
        {title}
      </span>
      <span className={rowCss.time}>{rowTimeLabel(session, now)}</span>
      <span className={rowCss.rowActions} onClick={(e) => { e.stopPropagation() }}>
        <Menu
          open={menuOpen}
          onClose={() => { setMenuOpen(false) }}
          portal
          closeOnPointerLeave
          anchor={(
            <button
              type="button"
              className={rowCss.iconButton}
              aria-label={`会话操作：${title}`}
              onClick={() => { setMenuOpen(v => !v) }}
            >
              <IconEllipsisOutlineRegular />
            </button>
          )}
        >
          <MenuItemButton
            icon={<IconTrashOutlineRegular size={14} />}
            danger
            onSelect={() => { setMenuOpen(false); onDelete(session) }}
          >
            删除会话
          </MenuItemButton>
        </Menu>
      </span>
    </div>
  )
}

/** One project header row: folder, title, and the new-session affordance. */
function ProjectRow({ group, expanded, onToggle, onCreate }: {
  group: ProjectGroup
  expanded: boolean
  onToggle: () => void
  onCreate: (group: ProjectGroup) => void
}) {
  return (
    <div
      className={rowCss.projectRow}
      data-row-key={`workspace:${group.cwd}`}
      role="treeitem"
      aria-expanded={expanded}
      onClick={onToggle}
    >
      <span className={clsx(rowCss.slot, rowCss.folder, expanded && rowCss.folderActive)}>
        {expanded ? <IconFolderOpenRegular /> : <IconFolderCloseRegular />}
      </span>
      <span className={clsx(rowCss.slot, rowCss.chevron)}>
        <IconTriangleRightFillRegular className={clsx(rowCss.arrow, expanded && rowCss.arrowOpen)} />
      </span>
      <span className={rowCss.projectText} title={group.cwd}>
        <span className={rowCss.title}>{group.name}</span>
      </span>
      <span className={rowCss.rowActions}>
        <Tooltip label="在此项目新建会话" side="bottom" align="end" delayMs={500}>
          <button
            type="button"
            className={rowCss.iconButton}
            aria-label={`在此项目新建会话：${group.name}`}
            onClick={(e) => { e.stopPropagation(); onCreate(group) }}
          >
            <IconNewChatOutlineRegular />
          </button>
        </Tooltip>
      </span>
    </div>
  )
}

/** One search result row: status dot, title, and the scope's own context label. */
function SearchResultRow({ session, scope, selected, now, onOpen }: {
  session: SessionSummary
  scope: FilterScope
  selected: boolean
  now: number
  onOpen: (session: SessionSummary) => void
}) {
  const running = session.status === 'running'
  const title = sessionTitle(session)
  return (
    <div
      data-row-key={`session:${session.id}`}
      data-status={session.status}
      className={clsx(rowCss.searchResultRow, selected && rowCss.selected)}
      role="treeitem"
      aria-selected={selected}
      onClick={() => { onOpen(session) }}
    >
      <span className={rowCss.searchResultHeading}>
        {/* Like session rows, the leading slot owns every row marker. */}
        <span className={rowCss.slot}>{running && <StateDot state="ongoing" />}</span>
        <span className={rowCss.searchResultTitle}>{title}</span>
        <span className={rowCss.time}>{rowTimeLabel(session, now)}</span>
      </span>
      <span className={rowCss.searchResultMeta}>
        {/* The project scope names the project; the path scope names the directory. */}
        <span className={rowCss.searchResultWorkspace}>
          {scope === 'path' ? session.cwd : projectName(session.cwd)}
        </span>
      </span>
    </div>
  )
}

/**
 * Render the session browser.
 * @param props.groups - project groups to list.
 * @param props.selectedKey - `cwd:id` of the open session, when one is open.
 * @param props.now - epoch ms for relative-time formatting.
 * @param props.onOpen - open a session.
 * @param props.onCreate - start a session inside a project.
 * @param props.onDelete - delete a session.
 * @returns the browser element tree.
 */
export function SessionBrowser({ groups, selectedKey, now, onOpen, onCreate, onDelete }: {
  groups: readonly ProjectGroup[]
  selectedKey: string | undefined
  now: number
  onOpen: (session: SessionSummary) => void
  onCreate: (group: ProjectGroup) => void
  onDelete: (session: SessionSummary) => void
}) {
  const [collapsed, setCollapsed] = useState<readonly string[]>([])
  const [query, setQuery] = useState('')
  const [scope, setScope] = useState<FilterScope>('all')
  const [searchExpanded, setSearchExpanded] = useState(false)
  const searchInput = useRef<HTMLInputElement>(null)
  const searching = query.trim() !== ''
  // Only the discovered catalog is searched: project names and full paths are
  // separate scopes, and the keyword survives a scope change.
  const results = useMemo(() => (searching
    ? groups.flatMap(group => group.sessions).filter(session => matchesSession(session, query, scope))
    : []), [groups, query, scope, searching])

  return (
    <div className={clsx(css.treeBody, css.wide)}>
      <div className={css.sectionHeader}>
        <span className={clsx(css.sectionLabel, css.wide, searchExpanded && css.sectionLabelHidden)}>
          会话
        </span>
        <div className={clsx(css.searchSlot, searchExpanded && css.searchSlotExpanded)}>
          <div
            className={clsx(css.search, searchExpanded && css.searchExpanded)}
            onClick={() => {
              setSearchExpanded(true)
              searchInput.current?.focus()
            }}
          >
            <Tooltip label="搜索会话" side="bottom" delayMs={500} disabled={searchExpanded}>
              <button
                type="button"
                className={css.searchButton}
                aria-label="搜索会话"
                aria-expanded={searchExpanded}
                onClick={() => {
                  setSearchExpanded(true)
                  searchInput.current?.focus()
                }}
              >
                <IconSearchOutlineRegular size={searchExpanded ? 11 : 14} />
              </button>
            </Tooltip>
            <input
              ref={searchInput}
              className={css.searchInput}
              type="text"
              placeholder="搜索项目或会话…"
              aria-label="搜索"
              value={query}
              tabIndex={searchExpanded ? 0 : -1}
              onChange={(event) => { setQuery(event.target.value) }}
              onKeyDown={(event) => {
                if (event.key !== 'Escape') return
                setQuery('')
                setSearchExpanded(false)
              }}
            />
            {searchExpanded && (
              <button
                type="button"
                className={css.clearButton}
                aria-label="清空筛选"
                onClick={() => {
                  setQuery('')
                  searchInput.current?.focus()
                }}
              >
                <IconCloseOutlineRegular size={12} />
              </button>
            )}
          </div>
        </div>
      </div>
      {searchExpanded && (
        <div className={css.searchScope}>
          <SegmentedControl
            id="mink-catalog-scope"
            label="筛选范围"
            value={scope}
            options={[
              { value: 'all', label: '全部' },
              { value: 'project', label: '项目' },
              { value: 'session', label: '会话' },
              { value: 'path', label: '路径' },
            ]}
            onChange={setScope}
          />
        </div>
      )}
      {searching
        ? (
          <div className={css.searchTree} role="tree" aria-label="搜索结果">
            {results.length === 0 && <p className={css.empty}>没有匹配的会话</p>}
            {results.map(session => (
              <SearchResultRow
                key={`${session.cwd}:${session.id}`}
                session={session}
                scope={scope}
                selected={selectedKey === `${session.cwd}:${session.id}`}
                now={now}
                onOpen={onOpen}
              />
            ))}
          </div>
        )
        : (
          <div className={css.list} role="tree" aria-label="会话">
            {groups.map((group) => {
              const expanded = !collapsed.includes(group.cwd)
              return (
                <div key={group.cwd} className={css.groupSection}>
                  <ProjectRow
                    group={group}
                    expanded={expanded}
                    onToggle={() => {
                      setCollapsed(current => (current.includes(group.cwd)
                        ? current.filter(entry => entry !== group.cwd)
                        : [...current, group.cwd]))
                    }}
                    onCreate={onCreate}
                  />
                  {expanded && group.sessions.map(session => (
                    <SessionRow
                      key={`${group.cwd}:${session.id}`}
                      session={session}
                      selected={selectedKey === `${group.cwd}:${session.id}`}
                      now={now}
                      onOpen={onOpen}
                      onDelete={onDelete}
                    />
                  ))}
                </div>
              )
            })}
          </div>
        )}
      <span className={css.fade} />
    </div>
  )
}
