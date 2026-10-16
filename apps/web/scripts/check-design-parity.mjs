// Design parity check: every file the workbench copies from deepseek-harness must
// still match its source byte for byte, except where mink deliberately rewrites
// an import target (recorded below). Drift means the copy was hand-edited, which
// the repository forbids — edits belong in a mink-owned file, or upstream.
//
// Usage: node scripts/check-design-parity.mjs [--source <deepseek-harness root>]

import { createHash } from 'node:crypto'
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const WEB_DIR = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const REPO_DIR = resolve(WEB_DIR, '../..')

/** Roots whose files are copies of upstream sources. */
const COPIED_ROOTS = ['src/theme', 'src/ui', 'src/util', 'src/store']

/**
 * Copies whose upstream file has a different name or package path. Everything
 * else is located by matching the file's path inside its source package.
 */
const SOURCES = new Map([
  ['src/util/code-language.ts', 'packages/util/code-language/src/index.ts'],
  ['src/util/workspace-path/file-address.ts', 'packages/util/workspace-path/src/file-address.ts'],
  ['src/util/workspace-path/index.ts', 'packages/util/workspace-path/src/index.ts'],
  ['src/theme/shell-base.css', 'packages/client/web/src/base.css'],
  // Barrel and contract files share a name across packages: pin them by hand so
  // the comparison never guesses.
  ['src/ui/primitives/index.ts', 'packages/client/ui-primitives/src/index.ts'],
  ['src/store/index.ts', 'packages/client/store/src/index.ts'],
  ['src/store/contract.ts', 'packages/client/store/src/contract.ts'],
])

/**
 * Files a deliberate mink edit is allowed in, with the exact rewrites and their
 * reason. Everything else under the copied roots must match upstream byte for
 * byte; an edit belongs in a mink-owned file instead. A file may need several
 * import targets pointed at the local copies, so the rewrites are a list.
 */
const REWRITES = new Map([
  ['src/ui/primitives/PathLabel.tsx', {
    reason: 'workspace-path import points at the local copy',
    rewrites: [{ from: "'@deepseek-ai/dsh-util-workspace-path'", to: "'../../util/workspace-path/index.ts'" }],
  }],
  ['src/ui/primitives/code-highlighting.ts', {
    reason: 'code-language re-export points at the local copy',
    rewrites: [{ from: "'@deepseek-ai/dsh-util-code-language'", to: "'../../util/code-language.ts'" }],
  }],
  ['src/ui/primitives/settings-form/form-model.ts', {
    reason: 'store import points at the local copy',
    rewrites: [{ from: "'@deepseek-ai/dsh-client-store'", to: "'../../../store/index.ts'" }],
  }],
  ['src/ui/commands/PopupSelectView.tsx', {
    reason: 'primitive imports point at the local copies; the locale seat is typed locally',
    rewrites: [
      { from: "'@deepseek-ai/dsh-client-ui-primitives'", to: "'../primitives/index.ts'" },
      { from: "from '@deepseek-ai/dsh-client-ui-slots'\n", to: "from '../../app/commands/locale.ts'\n" },
      { from: "from './contract.ts'", to: "from '../../app/commands/contract.ts'" },
    ],
  }],
  ['src/ui/commands/popup.ts', {
    reason: 'store and primitive imports point at the local copies; the token span is the local contract type',
    rewrites: [
      { from: "'@deepseek-ai/dsh-client-store'", to: "'../../store/index.ts'" },
      { from: "import type { TokenSpan } from '@deepseek-ai/dsh-client-ui-input-trigger/client'", to: "import type { TokenSpan } from '../../app/commands/contract.ts'" },
      { from: "from '@deepseek-ai/dsh-client-ui-primitives'", to: "from '../primitives/index.ts'" },
      { from: "from './contract.ts'", to: "from '../../app/commands/contract.ts'" },
    ],
  }],
  ['src/ui/commands/option-groups.ts', {
    reason: 'contract types point at the local copy',
    rewrites: [{ from: "from './contract.ts'", to: "from '../../app/commands/contract.ts'" }],
  }],
])

/** Upstream packages the copied files come from. */
const SOURCE_ROOTS = ['packages/client', 'packages/util', 'packages/storage']

function walk(dir, out = []) {
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry)
    if (statSync(path).isDirectory()) walk(path, out)
    else out.push(path)
  }
  return out
}

/** Index upstream sources as `<package>/src/<path>` suffixes, one list per name. */
function indexSources(sourceRoot) {
  const index = new Map()
  for (const root of SOURCE_ROOTS) {
    let files
    try {
      files = walk(join(sourceRoot, root))
    } catch {
      continue
    }
    for (const file of files) {
      const marker = '/src/'
      const at = file.indexOf(marker)
      if (at < 0) continue
      const suffix = file.slice(at + marker.length)
      const name = suffix.slice(suffix.lastIndexOf('/') + 1)
      const list = index.get(name)
      // The package a source belongs to, compared loosely against the copied
      // path so `ui-primitives/src/focus.ts` matches `src/ui/primitives/focus.ts`.
      const pack = file.slice(0, at).slice(file.slice(0, at).lastIndexOf('/') + 1)
      const entry = { suffix, file, pack: pack.replace(/[^a-z0-9]/gi, '').toLowerCase() }
      if (list === undefined) index.set(name, [entry])
      else list.push(entry)
    }
  }
  return index
}

/** Trailing path segments two paths share, the score for choosing a source. */
function sharedTail(a, b) {
  const left = a.split('/')
  const right = b.split('/')
  let shared = 0
  while (shared < left.length && shared < right.length
    && left[left.length - 1 - shared] === right[right.length - 1 - shared]) shared += 1
  return shared
}

/**
 * Undo the recorded rewrites so the copy must reduce to its upstream bytes.
 * @returns the local file's hash with the rewritten specifiers restored, or the
 *   string 'unreverted' when the rewrites are not the only differences.
 */
function reverted(file, source, rewrite) {
  const local = readFileSync(file, 'utf-8')
  let restored = local
  for (const step of rewrite.rewrites) {
    if (!restored.includes(step.to)) return 'unreverted'
    restored = restored.replaceAll(step.to, step.from)
  }
  return createHash('sha256').update(restored).digest('hex')
}

function sha256(file) {
  return createHash('sha256').update(readFileSync(file)).digest('hex')
}

const sourceRoot = process.argv.includes('--source')
  ? resolve(process.argv[process.argv.indexOf('--source') + 1])
  : resolve(REPO_DIR, '../deepseek-harness')

if (!existsSync(sourceRoot)) {
  console.error(
    `design source not found: ${sourceRoot}\n` +
      'pass `--source <deepseek-harness root>`; the revision to compare against is pinned in ' +
      'apps/web/THIRD_PARTY_NOTICES.md'
  )
  process.exit(2)
}

const index = indexSources(sourceRoot)
const identical = []
const rewritten = []
const drifted = []
const unmatched = []

for (const root of COPIED_ROOTS) {
  for (const file of walk(join(WEB_DIR, root))) {
    const dest = relative(WEB_DIR, file)
    const pinned = SOURCES.get(dest)
    let match
    if (pinned !== undefined) {
      match = join(sourceRoot, pinned)
    } else {
      const name = file.slice(file.lastIndexOf('/') + 1)
      const suffix = dest.replace(/^src\//, '')
      const wanted = suffix.split('/').slice(0, -1).join('').replace(/[^a-z0-9]/gi, '').toLowerCase()
      const score = candidate =>
        (candidate.pack === wanted ? 100 : 0) + sharedTail(candidate.suffix, suffix)
      const candidates = [...(index.get(name) ?? [])].sort((a, b) => score(b) - score(a))
      // An ambiguous name (two packages, same file) must be pinned by hand
      // rather than resolved by whichever sorted first.
      if (candidates.length > 1 && score(candidates[0]) === score(candidates[1])) {
        throw new Error(`ambiguous upstream source for ${dest}; add it to SOURCES`)
      }
      match = candidates[0]?.file
    }
    if (match === undefined) {
      unmatched.push(dest)
      continue
    }
    if (sha256(file) === sha256(match)) {
      identical.push(dest)
      continue
    }
    const allowed = REWRITES.get(dest)
    if (allowed !== undefined && reverted(file, match, allowed) === sha256(match)) {
      rewritten.push(`${dest} (${allowed.reason})`)
      continue
    }
    drifted.push(`${dest} ≠ ${relative(sourceRoot, match)}`)
  }
}

const total = identical.length + rewritten.length + drifted.length + unmatched.length
console.log(`design parity against ${sourceRoot}`)
console.log(`  identical : ${identical.length}/${total}`)
console.log(`  rewritten : ${rewritten.length}/${total}`)
for (const row of rewritten) console.log(`    - ${row}`)
console.log(`  drifted   : ${drifted.length}/${total}`)
for (const row of drifted) console.log(`    ! ${row}`)
if (unmatched.length > 0) {
  console.log(`  no upstream file matched (mink-owned): ${unmatched.length}`)
  for (const row of unmatched.slice(0, 20)) console.log(`    ? ${row}`)
  if (unmatched.length > 20) console.log(`    … ${unmatched.length - 20} more`)
}

process.exit(drifted.length === 0 ? 0 : 1)
