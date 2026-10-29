/**
 * The context one command opening carries: what a callback needs to settle a
 * pick. Upstream passes its session context here; mink's surface is opened from
 * the composer, so the draft and the pick handlers ride along.
 */
export interface CommandContext {
  /** Write text into the composer's draft. */
  readonly write: (text: string) => void
  /** Run one picked command immediately. */
  readonly run: (command: string) => void
  /** Open the file picker (the panel's own entry). */
  readonly pickFiles: () => void
  /** Whether the session accepts image attachments. */
  readonly canAttach: boolean
}

/**
 * Frozen contract of the client command surface, derived from the reference
 * client's `ui-commands` contract. Types only, with two differences: the client
 * context and session context types the reference takes from its own runtime
 * packages are absent here (mink's command surface is one session-agnostic
 * popup), and the icon type points at the local primitives.
 */
import type { ComponentType } from 'react'
import type { IconProps } from '../../ui/primitives/icons/props.ts'

/** Copy for an option that must be acknowledged before onSelect can run. */
export interface SelectConfirmation {
  readonly title: string
  readonly description: string
  readonly acknowledgeLabel: string
  readonly cancelLabel: string
  readonly confirmLabel: string
}

/**
 * The draft span one command opening covers, snapshotted when the shell opens.
 * Upstream takes this from its input-trigger package; mink's composer owns its
 * own draft, so the span is a plain offset pair plus the revision it came from.
 */
export type TokenSpan = { readonly via?: undefined } & {
  /** Offset the command token starts at. */
  readonly start: number
  /** Offset the token ends at. */
  readonly end: number
  /** Draft revision the span was taken from. */
  readonly revision: number
}

/** Caller-owned group name and localized heading for popup options. */
export interface SelectOptionGroup {
  readonly name: string
  readonly label: string
}

/** Search behavior selected by the popup owner. */
export type PopupSearchMode = 'substring' | 'fuzzy-label'

/** One option row of a popupSelect shell. */
export interface SelectOption {
  readonly id: string
  readonly label: string
  /** Optional short marker rendered as a superscript beside the label. */
  readonly badge?: string
  readonly detail?: string
  /** Equal group names share a sticky heading; groups retain first-occurrence order. */
  readonly group?: SelectOptionGroup
  /**
   * The row the shell's highlight parks on when the panel opens, so an accept
   * gesture made without looking confirms the value in use. A business package
   * that marks a row `active` for presentation alone would make that row the
   * default pick.
   */
  readonly active?: boolean
  /** Optional in-page risk gate owned by the shared popup shell. */
  readonly confirmation?: SelectConfirmation
}

/** Command-owned search copy, resolved when its popup opens. */
export interface PopupSearchLabels {
  readonly placeholder: string
  /** Empty catalog, before filtering. */
  readonly empty: string
  /** Nonempty catalog with no matching rows. */
  readonly noResults: string
}

/**
 * Business registration for the popupSelect command kind. Data is
 * self-served: options/onSelect use the business package's own protocol.
 * The shell component is owned by ui-commands; business never sees it. Both
 * callbacks receive the CommandContext captured at popup open.
 */
export interface PopupSelectSpec {
  readonly kind: 'popupSelect'
  /** Defaults to substring search; fuzzy-label ranks names within each group. */
  readonly searchMode?: PopupSearchMode
  /**
   * Supply localized search copy for this opening; omitted uses the shell's generic copy.
   * @returns search placeholder and empty-state labels.
   */
  searchLabels?(): PopupSearchLabels
  options(session: CommandContext, signal: AbortSignal): Promise<readonly SelectOption[]>
  onSelect(option: SelectOption, session: CommandContext): void | Promise<void>
}

/**
 * Business registration for the action command kind: a bare invocation
 * consumes the trigger token and runs one client-side callback. It submits nothing, so an
 * attachment-carrying draft never refuses it.
 */
export interface ActionSpec {
  readonly kind: 'action'
  /**
   * Run the action for one session.
   * @param session - the CommandContext captured at invocation.
   */
  run(session: CommandContext): void
}

/** The UI behavior of a contribution or decoration. */
export type CommandUiSpec = PopupSelectSpec | ActionSpec

/**
 * One client-owned command contribution: a slash-menu entry whose behavior
 * lives entirely on the client (no host descriptor). Merged with the host
 * catalog by name — a collision with a host command fails loud at candidate
 * synthesis, never shadows. Row copy is read on every candidate pass, so a
 * locale change reaches the next menu open without re-registration.
 */
export interface CommandContribution {
  /** Command name without the leading slash (unique across contributions). */
  readonly name: string
  /** Localized menu row title; the name itself when absent. */
  label?(): string
  /** Localized menu row description; the row shows none when absent. */
  description?(): string
  /** Menu row glyph from the shared icon set. */
  readonly icon?: ComponentType<IconProps>
  /** Capability filter, called with a fresh projection per candidate pass. */
  available(session: CommandContext): boolean
  /** The command's UI behavior. */
  readonly ui: CommandUiSpec
}

/**
 * A UI decoration hung on one HOST command: what its BARE invocation does on
 * this client. Not a second command — the host command keeps its catalog
 * row, its argument claim (space / argued enter), and its lifecycle logging;
 * the decoration replaces only the bare menu-pick/enter with a popup whose
 * onSelect typically submits a completed line back through command.execute.
 * A decoration never manufactures a row: a name with no host catalog entry
 * in the session's directory simply never reaches the decoration.
 */
export interface CommandDecoration {
  /** The HOST command name this decorates (without the leading slash). */
  readonly name: string
  /** Capability filter, called with a fresh projection per bare invocation. */
  available(session: CommandContext): boolean
  /** The bare-invocation UI. */
  readonly ui: CommandUiSpec
}

/** The `ctx.commandUi` service face visible to business packages. */
export interface CommandUiContract {
  /**
   * Register one client command contribution; effect disposer. Duplicate
   * names throw at registration.
   */
  register(contribution: CommandContribution): () => void
  /**
   * Hang a bare-invocation decoration on one host command; effect disposer.
   * Duplicate names throw at registration.
   */
  decorate(decoration: CommandDecoration): () => void
  /** Close this command's open popups and confirmations without consuming composer drafts. */
  dismiss(name: string): void
  /** Resolve the popup controller this surface is opened from. */
  popupFor(context: CommandContext): unknown
}
