// Isolated lane teardown: stop the fixture provider and server by pid, then drop
// the temporary home.
import { readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

export default async function globalTeardown(): Promise<void> {
  try {
    const pids = readFileSync(join(tmpdir(), 'mink-web-e2e.pids'), 'utf-8').trim().split('\n')
    for (const pid of pids) {
      if (pid === '') continue
      try {
        process.kill(Number(pid), 'SIGKILL')
      } catch {
        // Already gone.
      }
    }
  } catch {
    // No pid file: nothing was started.
  }
  rmSync(join(tmpdir(), 'mink-web-e2e.pids'), { force: true })
  rmSync(join(tmpdir(), 'mink-web-e2e-home'), { recursive: true, force: true })
}
