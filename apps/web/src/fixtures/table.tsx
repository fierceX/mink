// Dev-only fixture: a transcript column with a wide markdown table inside the
// real conversation geometry. The e2e lane uses it to prove that the breakout
// stays inside the transcript's clipped column and that the table itself is the
// scrollport.
import '../theme/styles/brand-font.css'
import '../theme/styles/base.css'
import '../theme/styles/corner-shape.css'
import '../theme/styles/design-platform.css'
import '../theme/styles/focus.css'
import '../theme/styles/scrollbar.css'
import '../theme/styles/gradient-shadow-text.css'
import '../theme/styles/shiki.css'
import '../theme/shell-base.css'
import '../app/app.css'
import { createRoot } from 'react-dom/client'
import { MarkdownText } from '../ui/primitives/markdown/MarkdownText.tsx'
import { MARKDOWN_LABELS } from '../app/conversation/labels.ts'
import rootCss from '../app/conversation/ConversationRoot.module.css'
import chatCss from '../app/conversation/ChatView.module.css'

const TABLE = `
| 模块 | 当前行为 | 改造方向 | 验收证据 |
| --- | --- | --- | --- |
| reconnect | the browser refreshes the whole page | reopen the runtime and rebuild the snapshot | a 5-second outage recovers without leaving the page |
| snapshot | the last twenty user turns are sent by default | four turns plus a 256 KiB ceiling | the wire body shrinks and older rows stay behind the button |
| goal | every lifecycle event carries the full review history | only the fields the bar renders | the detail tab asks for the full contract |
| table | a wide table reaches past the transcript and is clipped | a wrapper that scrolls inside the column | the last cell is reachable at maximum scroll |
| project | every new task asks for a directory and a name | one click in a project, or a path-only project entry | no session request before the first send |
`

function Fixture() {
  return (
    <div
      className={rootCss.root}
      data-fixture-root
      style={{
        width: 1000,
        height: 600,
        margin: '40px auto',
        ['--dsh-conversation-column-width' as string]: '1000px',
      }}
    >
      <div className={rootCss.body}>
        <div className={rootCss.scrollBody} data-conversation-scroll>
          <div className={chatCss.column}>
            <MarkdownText text={TABLE} streaming={false} labels={MARKDOWN_LABELS} />
          </div>
        </div>
      </div>
    </div>
  )
}

const el = document.getElementById('root')
if (el === null) throw new Error('wide-table fixture: missing #root')
createRoot(el).render(<Fixture />)
