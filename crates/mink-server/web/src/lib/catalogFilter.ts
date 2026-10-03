import type { SessionSummary } from './api';

export type FilterScope = 'all' | 'project' | 'session' | 'path';
export function projectName(cwd: string): string { return cwd.split('/').filter(Boolean).pop() || cwd; }

/** Search only the discovered catalog; project names and full paths are separate scopes. */
export function matchesSession(row: SessionSummary, query: string, scope: FilterScope): boolean {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return true;
  const fields = {
    project: [projectName(row.cwd)],
    session: [row.title, row.alias, row.id],
    path: [row.cwd],
  };
  const candidates = scope === 'all' ? [...fields.project, ...fields.session, ...fields.path] : fields[scope];
  return candidates.some(value => value?.toLocaleLowerCase().includes(needle));
}
