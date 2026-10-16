/**
 * Composer statistics pills: the readings mink actually records, drawn with the
 * copied stats-pill sheet (12/20 tertiary tier, hover-pill affordance, 14px
 * icons). Every figure comes from the session projection the diagnostics tab
 * reads, so the two surfaces can never disagree: turns and requests, total
 * tokens with the cache-hit share, and how full the context is.
 */

import { IconDatabaseOutlineRegular, IconGaugeOutlineRegular } from '../../ui/primitives/index.ts'
import { cachePercentage } from '../../data/diagnostics.ts'
import { fmtK } from '../../data/fmt.ts'
import type { SessionState } from '../../data/types.ts'
import { useEffect, useState } from 'react'
import css from './StatsPills.module.css'

/** Whether the frame is narrow enough for the compact readings. */
function useNarrowViewport(): boolean {
  const [narrow, setNarrow] = useState(() =>
    typeof window !== 'undefined' && window.matchMedia('(max-width: 767px)').matches)
  useEffect(() => {
    const query = window.matchMedia('(max-width: 767px)')
    const apply = (): void => { setNarrow(query.matches) }
    apply()
    query.addEventListener('change', apply)
    return () => { query.removeEventListener('change', apply) }
  }, [])
  return narrow
}

/** One reading: icon plus label, the sheet's resting form. */
function Pill({ stat, label }: { stat: string; label: string }) {
  return (
    <span className={css.anchor} data-composer-stat={stat}>
      <span className={css.pill}>
        {stat === 'activity' ? <IconGaugeOutlineRegular /> : <IconDatabaseOutlineRegular />}
        <span className={css.label}>{label}</span>
      </span>
    </span>
  )
}

/**
 * Render the session's readings under the composer.
 * @param props.state - the live projection; unknown counters read as "—".
 * @returns the pill row.
 */
export function StatsPills({ state }: { state: SessionState | undefined }) {
  const turns = state?.turnCount ?? null
  const requests = state?.requestCount ?? null
  const totalTokens = (state?.tokensIn ?? 0) + (state?.cacheReadTokens ?? 0) + (state?.tokensOut ?? 0)
  const hit = state === undefined
    ? null
    : cachePercentage(state.tokensIn, state.cacheReadTokens, state.cacheCreationTokens)
  const limit = state?.contextLimitKnown === true ? state.maxContextTokens : 0
  const contextPercent = limit > 0 ? Math.floor((state?.contextTokens ?? 0) * 100 / limit) : null
  // A session that has reported nothing reads as unknown rather than zero: a
  // freshly opened session has no usage yet, and "0 轮 · 0 tok" would be a claim
  // about work nobody measured.
  const reported = totalTokens > 0 || (turns ?? 0) > 0 || (requests ?? 0) > 0
  // A phone has one line for three readings, so the labels shorten instead of
  // wrapping: the same figures, in the abbreviations a terminal would print.
  const narrow = useNarrowViewport()
  const activity = reported
    ? narrow
      ? `${turns === null ? '—' : turns}轮·${requests === null ? '—' : `${requests}次`}`
      : `${turns === null ? '—' : turns}轮 ${requests === null ? '—' : requests}请求`
    : narrow ? '—轮·—次' : '—轮 —请求'
  const usage = reported
    ? narrow
      ? `${fmtK(totalTokens)}tok·命中${hit === null ? '—' : `${hit}%`}`
      : `${fmtK(totalTokens)} tok · 缓存命中 ${hit === null ? '—' : `${hit}%`}`
    : narrow ? '—tok·命中—' : '— tok · 缓存命中 —'
  const context = reported && contextPercent !== null
    ? narrow ? `ctx${contextPercent}%` : `上下文 ${contextPercent}%`
    : narrow ? 'ctx—' : '上下文 —'

  return (
    <div className={css.dock} data-stats-pills>
      {/* The dock's own sheet lays the pills in one row; on a narrow viewport
          they wrap instead of leaving the card. */}
      <Pill stat="activity" label={activity} />
      <span className={css.sep} aria-hidden>|</span>
      <Pill stat="usage" label={usage} />
      <span className={css.sep} aria-hidden>|</span>
      <Pill stat="context" label={context} />
    </div>
  )
}
