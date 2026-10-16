/**
 * The composer's command surface: one copied popupSelect shell serves both
 * entries — the add control opens it on the whole contract, and typing `/` opens
 * it with the draft's query. The shell owns its search box, highlight,
 * single-flight pick and dismissal; mink supplies the rows and what a pick does.
 */

import { useCallback, useMemo, useSyncExternalStore } from 'react'
import { allCommandHints } from '../../data/commands.ts'
import { PopupSelectController } from '../../ui/commands/popup.ts'
import type { CommandContext, SelectOption, TokenSpan } from '../commands/contract.ts'

/** Row id of the file picker entry, which leads the list. */
export const ATTACH_OPTION_ID = 'mink.attach'

/** What the surface needs from the composer. */
export interface CommandPopupOptions {
  /** Write a picked command into the draft. */
  write: (text: string) => void
  /** Open the image picker. */
  pickFiles: () => void
  /** Whether this session takes image attachments. */
  canAttach: boolean
  /** Focus the draft again after the surface closes. */
  focusComposer: () => void
}

/** The surface's rows: the picker first, then the whole command contract. */
export function commandPopupOptions(canAttach: boolean, withAttach: boolean): SelectOption[] {
  const commands = allCommandHints().map(hint => ({
    id: hint.command,
    label: hint.command,
    detail: hint.available ? hint.summary : '本部署不可用',
  }))
  if (!withAttach) return commands
  return [
    {
      id: ATTACH_OPTION_ID,
      label: '添加图片',
      detail: canAttach ? '图片只进入会话附件目录' : '此会话不接受图片',
    },
    ...commands,
  ]
}

/** What the composer drives the surface with. */
export interface CommandPopupHandle {
  controller: PopupSelectController<CommandContext>
  /** Open on the whole contract (the add control). */
  openAll: () => void
  /** Open with a query the draft already carries (the `/` prefix). */
  openWith: (query: string) => void
  /** Update the query while the shell is open (the draft is the search box). */
  setQuery: (query: string) => void
  /** Move the highlight (the draft's own keys). */
  move: (delta: 1 | -1) => void
  /** Index the highlight sits on. */
  active: () => number
  /** Settle the highlighted row. */
  selectActive: () => void
  /** Close the shell. */
  close: () => void
}

/**
 * Build the composer's command surface.
 * @param options - the composer's own verbs.
 * @returns the controller plus its entry points.
 */
export function useCommandPopup(options: CommandPopupOptions): CommandPopupHandle {
  const { write, pickFiles, canAttach, focusComposer } = options
  const controller = useMemo(
    () => new PopupSelectController<CommandContext>({
      // The shell is opened from the composer's own draft, which a pick writes
      // back into, so there is no token span to consume.
      consume: () => true,
      focusComposer,
    }),
    [focusComposer],
  )

  const start = useCallback((withAttach: boolean, query: string) => {
    const context: CommandContext = { write, run: write, pickFiles, canAttach }
    const span: TokenSpan = { start: 0, end: 0, revision: 0 }
    controller.open('mink', {
      searchMode: 'substring',
      searchLabels: () => ({
        placeholder: '搜索命令…',
        empty: '无可用命令',
        noResults: '没有匹配的命令',
      }),
      options: async () => commandPopupOptions(canAttach, withAttach),
      onSelect: (option: SelectOption, ctx: CommandContext) => {
        if (option.id === ATTACH_OPTION_ID) {
          if (ctx.canAttach) ctx.pickFiles()
          return
        }
        // A pick places the command in the draft for the reader to run: the
        // surface never fires an action on its own.
        ctx.write(`${option.label} `)
      },
    }, context, { via: 'menu', span })
    if (query !== '') controller.setSearch(query)
  }, [canAttach, controller, pickFiles, write])

  const openAll = useCallback(() => { start(true, '') }, [start])
  const openWith = useCallback((query: string) => { start(false, query) }, [start])
  const setQuery = useCallback((query: string) => { controller.setSearch(query) }, [controller])
  const close = useCallback(() => { controller.dismiss({ focusComposer: true }) }, [controller])
  // The draft owns the query: the surface follows it instead of holding a search
  // box of its own, so deleting in the draft narrows the list and nothing
  // re-seeds it.
  const move = useCallback((delta: 1 | -1) => { controller.move(delta) }, [controller])
  const active = useCallback(() => controller.state.getSnapshot().active, [controller])
  const selectActive = useCallback(() => {
    void controller.select(controller.state.getSnapshot().active)
  }, [controller])

  return { controller, openAll, openWith, setQuery, close, move, active, selectActive }
}

/**
 * Track whether the surface is open (the composer reads it for its own chrome).
 * @param controller - the surface's controller.
 * @returns whether the shell is open.
 */
export function useCommandPopupOpen(controller: PopupSelectController<CommandContext>): boolean {
  return useSyncExternalStore(
    useCallback((notify: () => void) => controller.state.subscribe(notify), [controller]),
    useCallback(() => controller.state.getSnapshot().open, [controller]),
  )
}
