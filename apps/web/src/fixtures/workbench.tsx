// Chat-surface review entry (dev server only): the whole workbench composition
// — sidebar, header, transcript, goal bar, composer, stat pills — rendered from
// synthetic state, so the main surface can be screenshotted and compared against
// a committed baseline without a server or any session data. Not part of the
// production build.
import "../theme/styles/brand-font.css";
import "../theme/styles/base.css";
import "../theme/styles/corner-shape.css";
import "../theme/styles/design-platform.css";
import "../theme/styles/focus.css";
import "../theme/styles/scrollbar.css";
import "../theme/styles/gradient-shadow-text.css";
import "../theme/styles/shiki.css";
import "../theme/shell-base.css";
import "../app/app.css";
import { createRoot } from "react-dom/client";
import { AppFrame } from "../app/frame/AppFrame.tsx";
import { Conversation } from "../app/conversation/Conversation.tsx";
import { EmptyState } from "../app/EmptyState.tsx";
import { SidebarRoot } from "../app/sidebar/SidebarRoot.tsx";
import { groupSessions } from "../app/sidebar/SessionBrowser.tsx";
import { SIDEBAR_DEFAULT } from "../app/frame/columns.ts";
import { emptySession } from "../data/types.ts";
import type { GoalSnapshot, SessionSummary } from "../data/api.ts";
import { FIXTURE_ITEMS } from "./items.ts";

/** The session the surface shows. */
const SESSION: SessionSummary = {
  project_key: 'demo',
  corrupt: false,
  id: 'fixture-chat',
  alias: null,
  title: '工作台主界面核对',
  cwd: '/work/mink',
  created_at: '',
  updated_at: new Date().toISOString(),
  modified_secs: null,
  status: 'free',
  path: '/work/mink/fixture-chat',
}

/** Two more rows so the sidebar has a project to list. */
const SIBLINGS: SessionSummary[] = [
  SESSION,
  { ...SESSION, id: 'fixture-chat-2', title: '工具卡与转录', first_prompt: '把工具卡按来源重做一遍' },
  { ...SESSION, id: 'fixture-chat-3', title: null, alias: null, first_prompt: '对齐统计胶囊与状态栏口径' },
]

/** Goal the bar renders above the composer. */
const GOAL: GoalSnapshot = {
  id: 'goal-fixture',
  revision: 7,
  status: 'active',
  phase: 'executing',
  spec: { objective: '按来源设计完成工作台主界面', acceptance_criteria: ['结构与交互一致'], constraints: [] },
  budget: { max_turns: 20, turns_used: 12, remaining_turns: 8, deadline_unix_secs: 0, remaining_secs: 5400, blocked_after_consecutive_rounds: 3, blocked_rounds: 0, soft_token_limit: null },
}

/** Projection the transcript, goal bar and pills read. */
const STATE = {
  ...emptySession('fixture-chat', '工作台主界面核对'),
  model: 'deepseek-flash',
  turnCount: 12,
  requestCount: 48,
  tokensIn: 1_240_000,
  tokensOut: 168_000,
  cacheReadTokens: 9_600_000,
  cacheCreationTokens: 140_000,
  contextTokens: 72_000,
  maxContextTokens: 128_000,
  contextLimitKnown: true,
  items: FIXTURE_ITEMS,
  goal: GOAL,
}

/** The alias vocabulary the model seat offers in the gallery. */
const FIXTURE_MODEL_CHOICES = [
  { alias: 'flash', model: 'deepseek-flash' },
  { alias: 'pro', model: 'deepseek-v4-pro' },
] as const

const noop = () => {}
const noopAsync = async () => undefined

/** `?state=empty` shows the no-session-opened state instead of the chat. */
const EMPTY_STATE = new URLSearchParams(window.location.search).get('state') === 'empty'

const el = document.getElementById('root')
if (el === null) throw new Error('workbench fixtures: missing #root')
createRoot(el).render(
  <AppFrame
    sidebarWidth={SIDEBAR_DEFAULT}
    onSidebarWidthChange={noop}
    rightbarWidth={0}
    onRightbarWidthChange={noop}
    narrowExpanded={false}
    onNarrowClose={noop}
    onNarrowChange={noop}
    sidebar={(width, collapsed) => (
      <SidebarRoot
        collapsed={collapsed}
        width={width}
        groups={groupSessions(SIBLINGS)}
        selectedKey={`${SESSION.cwd}:${SESSION.id}`}
        now={Date.now()}
        onToggle={noop}
        onNewSession={noop}
        onAddProject={noop}
        onOpenSettings={noop}
        onOpenSession={noop}
        onCreateIn={noop}
        onDeleteSession={noop}
      />
    )}
    center={EMPTY_STATE
      ? (
        <EmptyState
          project={groupSessions(SIBLINGS)[0]}
          notice={undefined}
          onCreate={noop}
        />
      )
      : (
      <Conversation
        state={STATE}
        session={SESSION}
        title={SESSION.title ?? SESSION.id}
        open
        error={undefined}
        detailsOpen={false}
        processDisplay="standard"
        notice={undefined}
        hasOlder={false}
        onLoadOlder={async () => false}
        onGoalAction={noopAsync}
        onOpenFile={noop}
        onOpenArtifact={noop}
        onRetryOpen={noop}
        draft=""
        onDraftChange={noop}
        modelChoices={FIXTURE_MODEL_CHOICES}
        onModelChange={noop}
        onSend={noopAsync}
        onGuide={noopAsync}
        onReceiptChanged={noop}
        onCommand={async () => ({ message: '', ok: true })}
        followTick={0}
        onToggleDetails={noop}
        onStop={noop}
      />
      )}
  />,
)
