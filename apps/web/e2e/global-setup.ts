// Isolated browser lane: a temporary mink home with one fixture session, a
// deterministic local provider, and the server serving the freshly built
// workbench from disk. Nothing here touches the developer's own sessions, and
// the assistant answers are the fixture provider's, so the live paths (streaming
// text, guidance while a turn runs) replay the same way every run.

import { spawn, execSync, type ChildProcess } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdirSync, mkdtempSync, openSync, realpathSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const WEB_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const REPO_DIR = resolve(WEB_DIR, '../..')
const TARGET_DIR = resolve(REPO_DIR, process.env['CARGO_TARGET_DIR'] ?? 'target')

/** Port the isolated server listens on (the baseURL every live spec uses). */
export const E2E_PORT = 18821
/** Port of the deterministic provider. */
const PROVIDER_PORT = 18822
/** Fixture session the live specs open. */
export const E2E_SESSION_ID = 'workbench-fixture'
/** Fixture session long enough to scroll and to cap the turn list on a phone. */
export const E2E_LONG_ID = 'workbench-long'
/**
 * Working directory every fixture session records. The real path matters: core
 * derives a project key from the resolved working directory, so a symlinked
 * `/tmp` would put the session in a different project directory than the key
 * the server computes when it opens the session.
 */
export const E2E_CWD = realpathSync(mkdtempSync(join(tmpdir(), 'mink-web-e2e-cwd-')))
/** Temporary home; deleted on teardown. */
export const E2E_HOME = join(tmpdir(), 'mink-web-e2e-home')

/** The project directory key core derives from a working directory. */
function projectKey(cwd: string): string {
  const normalized = cwd.replaceAll('\\', '/')
  const hash = createHash('sha256').update(normalized).digest('hex').slice(0, 16)
  const readable = normalized
    .replace(/^\/+/, '')
    .replace(/[^A-Za-z0-9._-]/g, '-')
    .replace(/--+/g, '-')
    .replace(/^-|-$/g, '')
    .slice(0, 48) || 'root'
  return `${readable}--${hash}`
}

/** Two turns of history: one plain exchange and one with a tool call. */
function conversation(): string {
  const rows: Record<string, unknown>[] = [
    { role: 'user', content: 'fixture question one' },
    {
      role: 'assistant',
      content: [
        { type: 'thinking', thinking: 'inspecting the fixture project' },
        { type: 'text', text: 'fixture answer one' },
        { type: 'tool_use', id: 'read-1', name: 'Read', input: { path: 'README.md' } },
      ],
    },
    {
      role: 'user',
      content: [{ type: 'tool_result', tool_use_id: 'read-1', content: '1:# e2e project' }],
    },
    { role: 'assistant', content: [{ type: 'text', text: 'fixture answer two' }] },
    { role: 'user', content: 'fixture question two' },
    { role: 'assistant', content: [{ type: 'text', text: '完整输出见 [artifact://fixture-output](artifact://fixture-output)。' }] },
    { role: 'assistant', content: [{ type: 'text', text: 'fixture answer three' }] },
  ]
  return `${rows.map(row => JSON.stringify(row)).join('\n')}\n`
}

/** Thirty short turns: enough to scroll a phone viewport and to overflow the
 * turn list, without making the fixture hard to read. */
function longConversation(): string {
  const rows: Record<string, unknown>[] = []
  for (let turn = 1; turn <= 30; turn += 1) {
    rows.push({ role: 'user', content: `long fixture question ${turn}` })
    rows.push({ role: 'assistant', content: [{ type: 'text', text: `long fixture answer ${turn}` }] })
  }
  return `${rows.map(row => JSON.stringify(row)).join('\n')}\n`
}

function writeSession(root: string, id: string, title: string, history: string): void {
  const dir = join(root, id)
  mkdirSync(dir, { recursive: true })
  mkdirSync(join(dir, 'artifacts'), { recursive: true })
  // One spilled output the fixture history references by `artifact://` id.
  writeFileSync(join(dir, 'artifacts', 'fixture-output.txt'), 'fixture artifact body\n')
  writeFileSync(join(dir, 'conversation.jsonl'), history)
  writeFileSync(join(dir, 'session.json'), JSON.stringify({
    id,
    alias: id === E2E_SESSION_ID ? 'fixture' : null,
    title,
    cwd: E2E_CWD,
    created_at: '',
    updated_at: '',
    parent: null,
    first_prompt: null,
    summary: null,
  }))
}

function writeFixtureHome(): void {
  rmSync(E2E_HOME, { recursive: true, force: true })
  const projectDir = join(E2E_HOME, '.mink', 'projects', projectKey(E2E_CWD))
  writeSession(projectDir, E2E_SESSION_ID, 'fixture session', conversation())
  writeSession(projectDir, E2E_LONG_ID, 'long fixture session', longConversation())
  const sessionDir = join(E2E_HOME, '.mink', 'projects', projectKey(E2E_CWD), E2E_SESSION_ID)
  mkdirSync(sessionDir, { recursive: true })
  mkdirSync(E2E_CWD, { recursive: true })
  writeFileSync(join(E2E_CWD, 'README.md'), '# e2e project\n\nfixture project for the web lane.\n')
}

/** Wait until a URL answers, then hand back control. */
async function waitFor(url: string, timeoutMs = 30_000): Promise<void> {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    try {
      const response = await fetch(url)
      if (response.ok) return
    } catch {
      // Not listening yet.
    }
    await new Promise(resolve => setTimeout(resolve, 200))
  }
  throw new Error(`timed out waiting for ${url}`)
}

export default async function globalSetup(): Promise<void> {
  // A server already on the fixture ports would answer the readiness probe and
  // silently serve its own home and build, so refuse to start instead.
  for (const port of [E2E_PORT, PROVIDER_PORT]) {
    const taken = await fetch(`http://127.0.0.1:${port}/`, { method: 'GET' }).then(() => true, () => false)
    if (taken) throw new Error(`port ${port} is already serving; stop that process before running the live lane`)
  }
  // The served bundle is the workbench built from apps/web; the server reads it
  // from disk so the lane never depends on a stale embedded copy.
  execSync('npm run build', { cwd: WEB_DIR, stdio: 'inherit' })
  writeFixtureHome()

  const logFd = openSync(join(tmpdir(), 'mink-web-e2e-server.log'), 'w')
  const children: ChildProcess[] = []
  const provider = spawn(process.execPath, [join(WEB_DIR, 'e2e/mock-provider.mjs')], { stdio: 'ignore' })
  children.push(provider)
  await waitFor(`http://127.0.0.1:${PROVIDER_PORT}/health`)

  const server = spawn(join(TARGET_DIR, 'debug', 'mink-server'), [], {
    env: {
      ...process.env,
      MINK_HOME: E2E_HOME,
      MINK_SERVER_PORT: String(E2E_PORT),
      MINK_SERVER_DEV_WEB: join(WEB_DIR, 'dist'),
      MINK_IMAGE_INPUT: 'on',
      DEEPSEEK_API_KEY: 'sk-local-fixture',
      DEEPSEEK_BASE_URL: `http://127.0.0.1:${PROVIDER_PORT}/v1`,
      MINK_SERVER_TURN_TIMEOUT: '30',
      NO_PROXY: '127.0.0.1,localhost',
    },
    // The server's own diagnostics are worth keeping: a lane that talks to a
    // server with the wrong configuration should be diagnosable from its log.
    stdio: ['ignore', logFd, logFd],
  })
  children.push(server)
  await waitFor(`http://127.0.0.1:${E2E_PORT}/api/sessions`)

  writeFileSync(join(tmpdir(), 'mink-web-e2e.pids'), children.map(child => String(child.pid ?? '')).join('\n'))
}
