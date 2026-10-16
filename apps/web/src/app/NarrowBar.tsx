/**
 * Narrow-frame top bar: on a phone the sidebar is a drawer, so the way into it
 * has to live outside the sidebar and outside the conversation — the empty state
 * has no header of its own. The bar carries the drawer button, the brand and the
 * settings entry, mirroring the legacy workbench's phone top bar.
 */

import { IconPanelLeftOutlineRegular, IconSettingsOutlineRegular } from '../ui/primitives/index.ts'
import { MinkMark } from './brand/MinkMark.tsx'
import css from './NarrowBar.module.css'

/**
 * Render the phone top bar.
 * @param props.onOpenSidebar - draw the sidebar out.
 * @param props.onOpenSettings - open the settings dialog.
 * @returns the bar element tree.
 */
export function NarrowBar({ onOpenSidebar, onOpenSettings }: {
  onOpenSidebar: () => void
  onOpenSettings: () => void
}) {
  return (
    <div className={css.bar} data-narrow-bar>
      <button type="button" className={css.action} aria-label="展开侧栏" onClick={onOpenSidebar}>
        <IconPanelLeftOutlineRegular size={16} />
      </button>
      <span className={css.brand}>
        <MinkMark size={20} />
      </span>
      <button type="button" className={css.action} aria-label="设置" onClick={onOpenSettings}>
        <IconSettingsOutlineRegular size={16} />
      </button>
    </div>
  )
}
