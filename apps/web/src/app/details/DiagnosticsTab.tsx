import type { SessionSummary } from '../../data/api.ts'
import { diagnosticGroups } from '../../data/diagnostics.ts'
import type { SessionState } from '../../data/types.ts'
import css from './DetailPanel.module.css'

/** Diagnostics tab: the same counters the terminal's stats snapshot shows. */
export function DiagnosticsTab({ session, state }: { session: SessionSummary; state: SessionState | undefined }) {
  const groups = diagnosticGroups(state ?? null, session.cwd)
  const problems = (state?.items ?? []).filter(item => item.kind === 'error' || item.kind === 'signal')
  return (
    <div className={css.body}>
      {groups.map(group => (
        <section key={group.title} className={css.group}>
          <h4 className={css.groupTitle}>{group.title}</h4>
          <dl className={css.facts}>
            {group.rows.map(([label, value]) => (
              <div key={label} className={css.fact}>
                <dt>{label}</dt>
                <dd>{value}</dd>
              </div>
            ))}
          </dl>
          {group.title === 'Token'
            ? <p className={css.note}>命中率 = 缓存读取 ÷（未缓存输入 + 缓存读取 + 缓存创建）</p>
            : null}
        </section>
      ))}
      {problems.length > 0 && (
        <section className={css.group}>
          <h4 className={css.groupTitle}>本轮问题</h4>
          {problems.map((item, index) => (
            <p key={index} className={css.problem}>{'text' in item ? item.text : ''}</p>
          ))}
        </section>
      )}
      <p className={css.hint}>诊断只读：口径与终端 StatsSnapshot 一致，缺失的旧统计显示 —。</p>
    </div>
  )
}
