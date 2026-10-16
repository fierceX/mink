// DSH comparison capture: drives a locally running `dsh web` with a browser and
// saves the upstream surfaces the workbench is compared against. Design review
// only — the captures show whatever sessions that DSH instance holds, so they
// stay out of the repository (the default output is a temp directory).
//
// Usage:
//   node scripts/dsh-snapshot.mjs --url "http://127.0.0.1:3080/?token=…" [--out /tmp/dsh-snapshots] [--session "标题"]
//
// The token URL is the one `dsh web` prints; it also installs the auth cookie,
// which is why every capture starts from it.

import { mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { chromium } from '@playwright/test'

/** Parse `--flag value` pairs. */
function parseArgs(argv) {
  const args = new Map()
  for (let index = 0; index < argv.length; index += 1) {
    const token = argv[index]
    if (!token.startsWith('--')) continue
    args.set(token.slice(2), argv[index + 1])
    index += 1
  }
  return args
}

const args = parseArgs(process.argv.slice(2))
const url = args.get('url')
if (url === undefined) {
  console.error('usage: node scripts/dsh-snapshot.mjs --url "<dsh web url with token>" [--out dir] [--session title]')
  process.exit(1)
}
const out = args.get('out') ?? '/tmp/dsh-snapshots'
const sessionTitle = args.get('session')
mkdirSync(out, { recursive: true })

const browser = await chromium.launch()
const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2 })
const page = await context.newPage()
const captured = []

/** Save one capture and record it for the manifest. */
async function shot(name, options = {}) {
  const path = join(out, `${name}.png`)
  await page.screenshot({ path, ...options })
  captured.push(path)
}

try {
  await page.goto(url, { waitUntil: 'domcontentloaded' })
  await page.waitForTimeout(2500)
  // The onboarding notice stands between the shell and its hero.
  const dismiss = page.getByRole('button', { name: /^(Continue|知道了|继续)$/ })
  if (await dismiss.count() > 0) await dismiss.first().click().catch(() => {})
  await page.waitForTimeout(700)
  await shot('hero')

  const row = sessionTitle === undefined
    ? page.locator('[data-slot="sidebar.workspaces.session.row.action"]').first()
    : page.getByText(sessionTitle).first()
  if (await row.count() > 0) {
    await row.click({ timeout: 8000 }).catch(() => {})
    await page.waitForTimeout(3000)
    await shot('chat')

    const composer = page.locator('[data-slot="conversation.composer"]').first()
    if (await composer.count() > 0) {
      const box = await composer.boundingBox()
      if (box !== null) {
        await shot('composer', {
          clip: { x: 0, y: Math.max(0, box.y - 12), width: 1440, height: Math.min(300, 900 - box.y) },
        })
      }
    }
    const tool = page.locator('[data-tool]').first()
    if (await tool.count() > 0) {
      await tool.click().catch(() => {})
      await page.waitForTimeout(600)
      await shot('tool-card')
    }
  }

  const settings = page.locator('[data-slot="settings.trigger"], [data-slot="sidebar.settings"]').first()
  if (await settings.count() > 0) {
    await settings.click({ timeout: 8000 }).catch(() => {})
    await page.waitForTimeout(1200)
    await shot('settings')
    await page.keyboard.press('Escape')
  }
} finally {
  await browser.close()
}

writeFileSync(join(out, 'manifest.json'), `${JSON.stringify({ url: url.replace(/token=[^&]+/, 'token=…'), captured }, null, 1)}\n`)
console.log(`captured ${captured.length} surface(s) into ${out}`)
for (const path of captured) console.log(` - ${path}`)
console.log('these captures may contain private sessions: review before sharing, never commit them')
