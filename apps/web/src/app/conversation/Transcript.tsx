/**
 * Transcript rendering: mink's own turn projection over the item stream, drawn
 * with the copied DSH stylesheets. User bubbles come from ui-chat's
 * MessageItem, assistant prose from AssistantMarkdown, reasoning from
 * ReasoningRow and the process header from TurnProcessNodeView.
 */

import { memo, useMemo, useState } from 'react'
import { DisclosureRow, IconChevronDownOutlineRegular, MarkdownText, TextShimmer } from '../../ui/primitives/index.ts'
import { processSummary, projectTurns } from '../../data/transcriptProjection.ts'
import type { ProcessDisplay } from '../settings/preferences.ts'
import type { ToolItem, TranscriptItem } from '../../data/types.ts'
import { MARKDOWN_LABELS } from './labels.ts'
import { ToolCard } from './tool/ToolCard.tsx'
import { GoalNotice } from './GoalNotice.tsx'
import { MessageActions } from './MessageActions.tsx'
import { ArtifactRefs } from './ArtifactRefs.tsx'
import { ChangedFiles } from './ChangedFiles.tsx'
import { changedFilesOf } from './tool/changedFiles.ts'
import { fmtK } from '../../data/fmt.ts'
import assistantCss from './AssistantMarkdown.module.css'
import chatCss from './ChatView.module.css'
import messageCss from './MessageItem.module.css'
import processCss from './TurnProcessNodeView.module.css'
import reasoningCss from './ReasoningRow.module.css'

/** First line of a reasoning block, its collapsed summary. */
function firstLine(text: string): string {
  const newline = text.indexOf('\n')
  return newline === -1 ? text : text.slice(0, newline)
}

/** A user turn: right-aligned bubble on the message column, with its chrome. */
const UserRow = memo(function UserRow({ text }: { text: string }) {
  return (
    <div className={messageCss.userRow}>
      <div className={messageCss.userStack}>
        <div className={messageCss.bubble}>{text}</div>
        <MessageActions text={text} label="复制用户消息" />
      </div>
    </div>
  )
})

/** Assistant narration: full-width Markdown body. */
const AssistantRow = memo(function AssistantRow({ text, streaming, usage, onOpenArtifact }: {
  text: string
  streaming: boolean
  /** The turn's recorded usage, when the server reported one. */
  usage?: string | undefined
  onOpenArtifact?: ((id: string) => void) | undefined
}) {
  return (
    <div className={assistantCss.root} data-streaming={streaming || undefined}>
      <div className={assistantCss.body}>
        <MarkdownText text={text} streaming={streaming} labels={MARKDOWN_LABELS} />
      </div>
      {onOpenArtifact !== undefined && <ArtifactRefs text={text} onOpen={onOpenArtifact} />}
      {/* A streaming answer has no settled chrome yet. */}
      {!streaming && <MessageActions text={text} label="复制回答" info={usage} />}
    </div>
  )
})

/** One collapsed reasoning block; opening it renders the full Markdown. */
const ThinkingRow = memo(function ThinkingRow({ text, running }: { text: string; running: boolean }) {
  const [expanded, setExpanded] = useState(false)
  const summary = useMemo(() => firstLine(text).replaceAll('**', ''), [text])
  return (
    <div
      className={reasoningCss.root}
      data-variant="think"
      data-state={running ? 'running' : 'ok'}
      data-expanded={expanded || undefined}
      data-preview={!expanded && summary !== '' ? '' : undefined}
    >
      <DisclosureRow
        rowClassName={reasoningCss.row}
        leadingClassName={reasoningCss.leading}
        titleClassName={reasoningCss.title}
        icon={<span className={reasoningCss.leading} aria-hidden="true">✳</span>}
        title="思考"
        running={running}
        open={expanded}
        expandable
        expandOnRowClick
        onToggle={() => {
          setExpanded(value => !value)
        }}
        collapsedContent={(
          <>
            <span className={reasoningCss.separator} aria-hidden />
            <span className={reasoningCss.summary} data-streaming={running || undefined}>
              <span className={reasoningCss.summaryText}>
                <TextShimmer>{summary}</TextShimmer>
              </span>
            </span>
          </>
        )}
      >
        {expanded && (
          <div className={reasoningCss.thinkBody}>
            <MarkdownText text={text} streaming={running} labels={MARKDOWN_LABELS} variant="compact" />
          </div>
        )}
      </DisclosureRow>
    </div>
  )
})

/** One process group: the settled/running header plus its tool and think rows. */
function ProcessGroup({ items, active, defaultOpen, onOpenFile, onOpenArtifact }: {
  items: TranscriptItem[]
  active: boolean
  defaultOpen: boolean
  onOpenFile?: ((path: string) => void) | undefined
  onOpenArtifact?: ((id: string) => void) | undefined
}) {
  const [open, setOpen] = useState(defaultOpen)
  const tools = items.filter((item): item is ToolItem => item.kind === 'tool')
  const label = processSummary(items, active)
  return (
    <>
      <button
        type="button"
        className={processCss.root}
        data-open={open || undefined}
        disabled={tools.length === 0 && items.every(item => item.kind !== 'thinking')}
        aria-expanded={open}
        onClick={() => {
          setOpen(value => !value)
        }}
      >
        <span className={processCss.label}>{label}</span>
        <IconChevronDownOutlineRegular className={processCss.chevron} />
      </button>
      {open && items.map((item, index) => {
        if (item.kind === 'tool') {
          return (
            <ToolCard
              key={item.key ?? index}
              item={item}
              onOpenFile={onOpenFile}
              onOpenArtifact={onOpenArtifact}
            />
          )
        }
        if (item.kind === 'thinking') {
          return <ThinkingRow key={item.key ?? index} text={item.text} running={active} />
        }
        if (item.kind === 'system' || item.kind === 'signal' || item.kind === 'error') {
          return <p key={item.key ?? index} className={assistantCss.stopped}>{item.text}</p>
        }
        if (item.kind === 'sub_agent') {
          return (
            <p key={item.key ?? index} className={assistantCss.stopped}>
              子代理 {item.label ?? item.sessionId} · {item.status}{item.stopReason === undefined ? '' : ` · ${item.stopReason}`}
            </p>
          )
        }
        return null
      })}
    </>
  )
}

/** Total tokens a turn reported, in the workbench's own wording. */
function turnUsage(
  outcomes: Record<string, Record<string, unknown>> | undefined,
  user: TranscriptItem | undefined,
): string | undefined {
  const turnId = user?.kind === 'user' ? user.turnId : undefined
  const outcome = turnId === undefined ? undefined : outcomes?.[turnId]
  const usage = outcome?.['usage'] as { tokens?: { input_tokens?: number; output_tokens?: number; cache_read_tokens?: number } } | undefined
  const tokens = usage?.tokens
  if (tokens === undefined) return undefined
  const total = (tokens.input_tokens ?? 0) + (tokens.output_tokens ?? 0) + (tokens.cache_read_tokens ?? 0)
  return total === 0 ? undefined : `用量 ${fmtK(total)} tok`
}

/**
 * Render the transcript.
 * @param props.items - projected transcript items, oldest first.
 * @param props.running - the session is executing a turn right now.
 * @param props.processDisplay - how much of each turn's process to show.
 * @returns the transcript column.
 */
export function Transcript({ items, running, processDisplay = 'standard', outcomes, onOpenFile, onOpenArtifact }: {
  items: readonly TranscriptItem[]
  running: boolean
  processDisplay?: ProcessDisplay
  /** Per-turn outcomes, where the server reports a turn's usage. */
  outcomes?: Record<string, Record<string, unknown>> | undefined
  /** Open one changed file in the detail panel. */
  onOpenFile?: ((path: string) => void) | undefined
  /** Open one spilled output in the detail panel. */
  onOpenArtifact?: ((id: string) => void) | undefined
}) {
  const turns = useMemo(() => projectTurns([...items]), [items])
  return (
    <div className={chatCss.column}>
      {turns.map((turn, turnIndex) => {
        const last = turnIndex === turns.length - 1
        const changed = onOpenFile === undefined ? [] : changedFilesOf(turn.segments.flatMap(segment =>
          segment.kind === 'process' ? segment.items : [segment.item]))
        return (
          <div
            key={turn.key}
            className={chatCss.flowItem}
            data-turn-key={turn.key}
            {...(last ? {} : { 'data-actions-reveal': 'hover' })}
          >
            {turn.user !== undefined && <UserRow text={turn.user.kind === 'user' ? turn.user.text : ''} />}
            {turn.segments.map((segment) => {
              if (segment.kind === 'process') {
                // Concise keeps conclusions only: the process rows stay out.
                if (processDisplay === 'concise') return null
                return (
                  <ProcessGroup
                    key={segment.key}
                    items={segment.items}
                    active={running && last}
                    defaultOpen={processDisplay === 'detailed'}
                    onOpenFile={onOpenFile}
                    onOpenArtifact={onOpenArtifact}
                  />
                )
              }
              const item = segment.item
              if (item.kind === 'text') {
                return (
                  <AssistantRow
                    key={segment.key}
                    text={item.text}
                    streaming={running && last}
                    usage={turnUsage(outcomes, turn.user)}
                    onOpenArtifact={onOpenArtifact}
                  />
                )
              }
              if (item.kind === 'error') {
                return <p key={segment.key} className={assistantCss.stopped}>{item.text}</p>
              }
              if (item.kind === 'goal_message') {
                return <GoalNotice key={segment.key} item={item} />
              }
              return null
            })}
            {changed.length > 0 && <ChangedFiles files={changed} onOpen={onOpenFile!} />}
          </div>
        )
      })}
    </div>
  )
}
