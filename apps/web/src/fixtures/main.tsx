// Design-review entry (dev server only): the workbench's own controls rendered
// from synthetic state, above the transcript fixture. Everything here is stub
// data — no session, no server — so the page can be screenshotted and compared
// against a committed baseline. Not part of the production build.
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
import { Transcript } from "../app/conversation/Transcript.tsx";
import { Composer } from "../app/conversation/Composer.tsx";
import { GoalBar } from "../app/conversation/GoalBar.tsx";
import { StatsPills } from "../app/conversation/StatsPills.tsx";
import { MessageActions } from "../app/conversation/MessageActions.tsx";
import { ArtifactRefs } from "../app/conversation/ArtifactRefs.tsx";
import { PendingInputs } from "../app/conversation/PendingInputs.tsx";
import { CommandPanelPreview } from "./CommandPanelPreview.tsx";
import { emptySession } from "../data/types.ts";
import type { GoalSnapshot, SessionSummary } from "../data/api.ts";
import { FIXTURE_ITEMS } from "./items.ts";
import chatCss from "../app/conversation/ChatView.module.css";
import fixtureCss from "./fixtures.module.css";

/** Session the control gallery describes. */
const SESSION: SessionSummary = {
  project_key: 'demo',
  corrupt: false,
  id: 'fixture-workbench',
  alias: null,
  title: '控件画廊',
  cwd: '/work/mink',
  created_at: '',
  updated_at: '',
  modified_secs: null,
  status: 'free',
  path: '/work/mink/fixture-workbench',
}

/** Goal the bar renders: an active contract with turns left. */
const GOAL: GoalSnapshot = {
  id: 'goal-fixture',
  revision: 4,
  status: 'active',
  phase: 'executing',
  spec: { objective: '把工作台按来源设计重建并验证', acceptance_criteria: ['设计一致'], constraints: [] },
  budget: { max_turns: 20, turns_used: 16, remaining_turns: 4, deadline_unix_secs: 0, remaining_secs: 3600, blocked_after_consecutive_rounds: 3, blocked_rounds: 0, soft_token_limit: null },
}

/** Projection the stats pills and composer read. */
const STATE = {
  ...emptySession('fixture-workbench', '控件画廊'),
  model: 'deepseek-flash',
  turnCount: 42,
  requestCount: 137,
  tokensIn: 812_000,
  tokensOut: 96_400,
  cacheReadTokens: 12_400_000,
  cacheCreationTokens: 220_000,
  contextTokens: 61_000,
  maxContextTokens: 128_000,
  contextLimitKnown: true,
}

/** Neutral 1x1 swatch: the rail is about the frame, not about the picture. */
const NEUTRAL_SWATCH = 'data:image/svg+xml;utf8,<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="%23c9ccd1"/></svg>'

/** One draft attachment in each state the rail can show. */
const ATTACHMENTS = {
  entries: [
    { key: 'a1', name: 'screenshot.png', bytes: 2048, preview: NEUTRAL_SWATCH, status: 'ready' as const, attachment: { id: 'att-1', mime: 'image/png', width: 1, height: 1, bytes: 2048 } },
    { key: 'a2', name: 'uploading.png', bytes: 4096, preview: NEUTRAL_SWATCH, status: 'uploading' as const },
  ],
  add: () => {},
  remove: () => {},
  clear: () => {},
  ids: ['att-1'],
  blocked: false,
  notice: undefined as string | undefined,
}

const el = document.getElementById('root')
if (el === null) throw new Error('fixtures: missing #root')
createRoot(el).render(
  <div className={fixtureCss.page}>
    <h1 className={fixtureCss.heading}>工作台控件（合成数据，仅设计核对页）</h1>
    <section className={fixtureCss.stack} data-fixture-section="goal-bar">
      <h2 className={fixtureCss.caption}>目标条（活动目标的只读投影与 CAS 控制）</h2>
      <GoalBar goal={GOAL} onAction={async () => undefined} />
    </section>
    <section className={fixtureCss.stack} data-fixture-section="composer">
      <h2 className={fixtureCss.caption}>输入区（待处理引导、附件栏、composer、统计胶囊）</h2>
      <Composer
        hero={false}
        running
        targetTurnId="turn-fixture"
        disabled={false}
        attachments={ATTACHMENTS}
        canAttach
        model="deepseek-flash"
        pending={(
          <PendingInputs
            receipts={[{
              input_id: 'input-1',
              revision: 2,
              turn_id: 'turn-fixture',
              status: 'pending',
              guidance: true,
              input: { request_id: 'req-1', text: '顺便把统计胶囊也对齐', target_turn_id: 'turn-fixture', attachment_ids: [] },
            }]}
            session={SESSION}
            onChanged={() => {}}
          />
        )}
        onSend={async () => undefined}
        onGuide={async () => undefined}
        onCommand={async () => ({ message: '', ok: true })}
        onStop={() => {}}
      />
      <StatsPills state={STATE} />
    </section>
    <section className={fixtureCss.stack} data-fixture-section="chrome">
      <h2 className={fixtureCss.caption}>消息 chrome（复制 + 本轮用量）与产物入口</h2>
      <MessageActions text="控件画廊的复制与用量" label="复制回答" info="用量 18.5k tok" />
      <ArtifactRefs text="完整输出见 artifact://run-1。" onOpen={() => {}} />
    </section>
    <section className={fixtureCss.stack} data-fixture-section="command-menu">
      <h2 className={fixtureCss.caption}>命令弹层（拷贝的 popupSelect，`+` 与 `/` 共用）</h2>
      <div className={fixtureCss.menuStage}>
        <CommandPanelPreview />
      </div>
    </section>
    <h1 className={fixtureCss.heading}>工具卡与转录（合成数据）</h1>
    <h2 className={fixtureCss.caption}>转录（过程行、工具卡家族、改动文件卡）</h2>
    <div className={chatCss.root}>
      <div className={chatCss.scroll}>
        <Transcript items={FIXTURE_ITEMS} running={false} onOpenFile={() => {}} onOpenArtifact={() => {}} />
      </div>
    </div>
  </div>,
)
