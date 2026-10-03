import { describe, expect, it } from 'vitest';
import type { SessionSummary } from './api';
import { matchesSession, projectName, type FilterScope } from './catalogFilter';

const row: SessionSummary = { id:'session-42', project_key:'key', cwd:'/work/parent/Mink', title:'修复 Cache', alias:'release.v2', status:'free', path:'/sessions/session-42', created_at:'', updated_at:'', modified_secs:null, corrupt:false };
describe('catalog search scopes', () => {
  it.each<[FilterScope, string, boolean]>([
    ['project', ' mink ', true], ['project', 'parent', false], ['project', 'Cache', false],
    ['session', 'cache', true], ['session', 'release.v2', true], ['session', 'session-42', true], ['session', 'Mink', false],
    ['path', '/work/parent', true], ['path', 'release.v2', false],
    ['all', 'parent', true], ['all', 'Cache', true], ['all', 'release.*', false], ['all', '   ', true],
  ])('%s / %s => %s', (scope, query, expected) => expect(matchesSession(row,query,scope)).toBe(expected));
  it('handles missing titles and trailing path separators', () => {
    expect(projectName('/work/project/')).toBe('project');
    expect(matchesSession({ ...row, title:null, alias:null }, 'session-42', 'session')).toBe(true);
    expect(matchesSession({ ...row, title:null, alias:null }, 'undefined', 'all')).toBe(false);
  });
});
