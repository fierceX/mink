/**
 * Sidebar column shell: geometry, collapse crossfade and the browsing region.
 * Structure, class hookups and the freeze-during-collapse rule come from the
 * DSH `ui-sidebar` SidebarRoot; the slot seats collapse into props here, so the
 * browsing region holds the mink session browser instead of slot registrants.
 */

import clsx from 'clsx'
import { useEffect, useRef, useState } from 'react'
import {
  IconNewChatOutlineRegular,
  IconPanelLeftOutlineRegular,
  IconSettingsOutlineRegular,
  Tooltip,
} from '../../ui/primitives/index.ts'
import { MinkMark } from '../brand/MinkMark.tsx'
import { SessionBrowser, type ProjectGroup } from './SessionBrowser.tsx'
import type { SessionSummary } from '../../data/api.ts'
import css from './SidebarRoot.module.css'

/** Wide-content unmount delay; matches the 150ms wide-content fade-out. */
const COLLAPSE_SETTLE_MS = 150

/**
 * How long the column's scrollbars stay drawn after the pointer leaves it. The
 * bar is a pointer affordance here, and hiding it on the leave event itself
 * makes it blink out while the pointer is only crossing the column's edge.
 */
const SCROLLBAR_LINGER_MS = 2000

export interface SidebarRootProps {
  collapsed: boolean
  /** Resolved column width; wide content freezes at this width while fading. */
  width: number
  groups: readonly ProjectGroup[]
  selectedKey: string | undefined
  now: number
  /** Failure of a session verb, shown where the reader clicked it. */
  notice?: string | undefined
  onToggle: () => void
  onNewSession: () => void
  onOpenSettings: () => void
  onOpenSession: (session: SessionSummary) => void
  onCreateIn: (group: ProjectGroup) => void
  onDeleteSession: (session: SessionSummary) => void
}

/**
 * Render the sidebar column shell.
 * @param props - collapse state, resolved width, browser data and callbacks.
 * @returns the sidebar element tree.
 */
export function SidebarRoot({
  collapsed,
  width,
  groups,
  selectedKey,
  now,
  notice,
  onToggle,
  onNewSession,
  onOpenSettings,
  onOpenSession,
  onCreateIn,
  onDeleteSession,
}: SidebarRootProps) {
  // Wide content stays mounted while the collapse animates (fading via
  // .collapsed .wide), unmounts at settle, and remounts right away on expand.
  const [settled, setSettled] = useState(collapsed)
  useEffect(() => {
    if (!collapsed) {
      setSettled(false)
      return
    }
    const timer = window.setTimeout(() => {
      setSettled(true)
    }, COLLAPSE_SETTLE_MS)
    return () => {
      window.clearTimeout(timer)
    }
  }, [collapsed])
  const wide = !collapsed || !settled
  const lastWideWidth = useRef(width)
  if (!collapsed) lastWideWidth.current = width

  // Scrollbars in the column follow the pointer (.quietBars rebinds them
  // away): drawn while it is inside, and for SCROLLBAR_LINGER_MS after it
  // leaves. A pointer that returns within that window cancels the pending hide.
  const column = useRef<HTMLDivElement>(null)
  const [pointerInside, setPointerInside] = useState(false)
  const lingerTimer = useRef<number | undefined>(undefined)
  const armLinger = (): void => {
    if (lingerTimer.current !== undefined) return
    lingerTimer.current = window.setTimeout(() => {
      lingerTimer.current = undefined
      setPointerInside(false)
    }, SCROLLBAR_LINGER_MS)
  }
  const cancelLinger = (): void => {
    window.clearTimeout(lingerTimer.current)
    lingerTimer.current = undefined
  }
  // Leaving is decided by the column's BOX, not by DOM containment, and only
  // while the bars are drawn: a pointer moved onto a fixed-position panel that
  // hangs over the column would otherwise fire no leave event here.
  useEffect(() => {
    if (!pointerInside) return
    const onMove = (event: PointerEvent): void => {
      const rect = column.current?.getBoundingClientRect()
      if (rect === undefined) return
      const inside = event.clientX >= rect.left && event.clientX < rect.right
        && event.clientY >= rect.top && event.clientY < rect.bottom
      if (inside) cancelLinger()
      else armLinger()
    }
    document.addEventListener('pointermove', onMove)
    return () => {
      document.removeEventListener('pointermove', onMove)
      cancelLinger()
    }
  }, [pointerInside])

  const toggleLabel = collapsed ? '展开侧栏' : '收起侧栏'
  const toggle = (
    <Tooltip label={toggleLabel} delayMs={500} side="right">
      <button
        type="button"
        className={clsx(css.iconButton, css.toggle)}
        aria-label={toggleLabel}
        aria-expanded={!collapsed}
        onClick={onToggle}
      >
        {!wide && (
          <span className={css.railMark} aria-hidden="true">
            <MinkMark size={24} />
          </span>
        )}
        <IconPanelLeftOutlineRegular className={css.panelIcon} size={wide ? 16 : 18} />
      </button>
    </Tooltip>
  )

  return (
    <div
      ref={column}
      className={clsx(css.root, !wide && css.collapsed, collapsed && wide && css.fading, !pointerInside && css.quietBars)}
      style={wide ? { width: collapsed ? lastWideWidth.current : width } : undefined}
      onPointerEnter={() => {
        cancelLinger()
        setPointerInside(true)
      }}
      onPointerLeave={() => {
        armLinger()
      }}
    >
      <div className={css.logoRow}>
        {wide && (
          <span className={clsx(css.brand, css.wide)}>
            <span className={css.brandIdentity}>
              <span className={css.brandMark}>
                <MinkMark size={24} />
              </span>
              <span className={css.brandName}>
                <span className={css.fallbackBrandName}>Mink</span>
              </span>
            </span>
          </span>
        )}
        {toggle}
      </div>

      {/* The label fades before the hover/focus affordance, including on
          translucent backgrounds. */}
      <Tooltip label="新建任务" delayMs={500} side="right" disabled={wide}>
        <button
          type="button"
          className={css.newSession}
          aria-label="新建任务"
          onClick={onNewSession}
        >
          <span className={css.newSessionLabelMask}>
            <span className={css.newSessionContent}>
              {wide ? <IconNewChatOutlineRegular size={14} /> : <IconNewChatOutlineRegular size={18} />}
              {wide && <span className={clsx(css.newSessionLabel, css.wide)}>新建任务</span>}
            </span>
          </span>
        </button>
      </Tooltip>

      {/* The browsing region fills the column between the controls and the foot
          in both states; the rail keeps its single expand affordance above. */}
      <div className={css.regionArea}>
        {notice !== undefined && (
          <p className={css.notice} role="status">{notice}</p>
        )}
        {wide && (
          <SessionBrowser
            groups={groups}
            selectedKey={selectedKey}
            now={now}
            onOpen={onOpenSession}
            onCreate={onCreateIn}
            onDelete={onDeleteSession}
          />
        )}
      </div>

      {/* Footer: settings stays pinned to the column's foot, one panel row. */}
      <div className={css.footArea}>
        <div className={css.footerActions} />
        <div className={css.settingsArea}>
          <Tooltip label="设置" delayMs={500} side="right" disabled={wide}>
            <button
              type="button"
              className={clsx(css.panelRow, !wide && css.panelActive)}
              aria-label="设置"
              onClick={onOpenSettings}
            >
              <span className={css.panelGlyph} aria-hidden="true">
                <IconSettingsOutlineRegular size={wide ? 16 : 18} />
              </span>
              {wide && <span className={clsx(css.panelTitle, css.wide)}>设置</span>}
            </button>
          </Tooltip>
        </div>
      </div>
    </div>
  )
}
