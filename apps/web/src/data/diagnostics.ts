import type { SessionState } from './types';
import { fmtK } from './fmt';

// Same partition and integer truncation as StatsSnapshot::cache_pct / ctx_pct.
export function cachePercentage(input: number, read: number, creation: number | null | undefined): number | null {
  if (creation == null) return null;
  const total = input + read + creation;
  return total > 0 ? Math.floor(read * 100 / total) : null;
}
export const workLabels: Record<string,string> = { idle:'空闲',waiting:'等待模型',thinking:'思考中',generating:'生成回复',tool:'执行工具','sub-agent':'等待子代理',compacting:'压缩上下文',error:'出错' };
const phaseLabels: Record<string,string> = { idle:'空闲',running:'运行中',cancelling:'正在停止',closing:'正在释放',closed:'已关闭' };
export function diagnosticGroups(state: SessionState | null, cwd: string) {
  const percent = (value: number | null) => value == null ? '—' : `${value}%`;
  const plan = state?.resources?.plan;
  const todo = state?.resources?.todo.items;
  return [
    { title:'运行', rows:[
      ['模型',state?.model || '未记录'],['工作目录',cwd || '未记录'],
      ['工作状态',workLabels[state?.workState ?? ''] ?? '未记录'],['运行阶段',phaseLabels[state?.phase ?? ''] ?? '未记录'],
      ['轮次',state?.turnCount?.toLocaleString() ?? '—'],['模型请求',state?.requestCount?.toLocaleString() ?? '—'],
      ...(state?.waitElapsedSecs != null ? [['模型等待',`${state.waitElapsedSecs}s`]] : []),
    ] },
    { title:'Token', rows:[
      ['输入',fmtK((state?.tokensIn ?? 0) + (state?.cacheReadTokens ?? 0))],['输出',fmtK(state?.tokensOut ?? 0)],
      ['缓存命中率',percent(state ? cachePercentage(state.tokensIn,state.cacheReadTokens,state.cacheCreationTokens) : null)],
      ['缓存读取',fmtK(state?.cacheReadTokens ?? 0)],['缓存创建',state?.cacheCreationTokens == null ? '—' : fmtK(state.cacheCreationTokens)],['未缓存输入',fmtK(state?.tokensIn ?? 0)],
    ] },
    { title:'上下文', rows:[
      ['当前上下文',fmtK(state?.contextTokens ?? 0)],['上下文上限',state?.contextLimitKnown ? state.maxContextTokens > 0 ? fmtK(state.maxContextTokens) : '未限制' : '—'],
      ['上下文占比',percent(state?.maxContextTokens ? Math.floor(state.contextTokens * 100 / state.maxContextTokens) : null)],
    ] },
    { title:'任务', rows:[
      ['信念度',state?.belief ? state.belief.toFixed(2) : '未跟踪'],['计划',plan ? plan.plan ? '已确认' : plan.draft ? '草稿' : '无' : '未记录'],
      ['Todo',todo ? `进行中 ${todo.filter(item=>item.status==='in_progress').length} / 待办 ${todo.filter(item=>item.status==='pending').length}` : '未记录'],
    ] },
  ];
}
