import { api } from "./api";
import { appState } from "./store";
let timer: ReturnType<typeof setTimeout> | undefined;
let generation = 0;
export async function refreshCatalog() {
  const response = await api.listSessions();
  if (response.code !== 200) throw new Error(response.message);
  // Preserve existing rows during background state changes; new rows enter first.
  const by = new Map(response.data.map(row => [JSON.stringify([row.project_key, row.id]), row]));
  const known = appState.sessions.map(row => JSON.stringify([row.project_key, row.id]));
  appState.sessions = [...response.data.filter(row => !known.includes(JSON.stringify([row.project_key, row.id]))), ...known.flatMap(key => by.has(key) ? [by.get(key)!] : [])];
}
export function startCatalog() {
  const owner = ++generation;
  const poll = async () => {
    if (owner !== generation || document.hidden) return;
    try { await refreshCatalog(); } catch { /* foreground commands report failures */ }
    if (owner === generation && !document.hidden) timer = setTimeout(poll, appState.sessions.some(row => row.status === "running") || appState.sessionState?.running ? 2000 : 15000);
  };
  const visible = () => { clearTimeout(timer); if (!document.hidden) void poll(); };
  document.addEventListener("visibilitychange", visible);
  void poll();
  return () => { ++generation; clearTimeout(timer); document.removeEventListener("visibilitychange", visible); };
}
