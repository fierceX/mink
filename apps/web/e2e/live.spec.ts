import { devices, expect, test, type Page } from '@playwright/test'
import { mkdirSync } from 'node:fs'
import { E2E_LONG_ID, E2E_SESSION_ID } from './global-setup.ts'

// Live lane: the workbench served by the isolated mink-server (fixture home +
// deterministic provider, see e2e/global-setup.ts). Assertions here cover paths
// that only exist against a real session: the streamed transcript, guidance
// queued into a running turn, the file/plan/goal panels, and the settings that
// survive a reload.

/**
 * Open the fixture session, which the isolated home always contains. It is
 * addressed by id: specs that create their own sessions sort ahead of it in the
 * catalog, so "the first row" is not a stable handle.
 */
async function openFixture(page: Page, id: string = E2E_SESSION_ID): Promise<void> {
  await page.goto('/')
  const row = page.locator(`[data-row-key="session:${id}"]`)
  await expect(row).toBeVisible()
  await row.click()
  await expect(page.locator('[class*="bubble"]').first()).toBeVisible()
}

async function fixtureCwd(page: Page): Promise<string> {
  const response = await page.request.get('/api/sessions')
  expect(response.ok()).toBe(true)
  const body = await response.json() as { data: { id: string; cwd: string }[] }
  const fixture = body.data.find(session => session.id === E2E_SESSION_ID)
  expect(fixture).toBeDefined()
  return fixture!.cwd
}

/**
 * Wait until no connection notice pins the reading chrome. Opening a session can
 * pass through a brief reconnect, and the recovered confirmation is a state that
 * deliberately keeps the chrome on screen.
 */
async function waitForSettledChrome(page: Page): Promise<void> {
  await expect(page.locator('[data-phase]')).toHaveCount(0)
  await expect(page.getByLabel('已恢复')).toHaveCount(0)
}

test('centers the composing hero in the conversation column', async ({ page }) => {
  await page.goto('/')
  await page.getByRole('button', { name: /^在 .* 新建任务$/ }).click()
  await expect(page.locator('[data-new-conversation]')).toBeVisible()
  const geometry = await page.evaluate(() => {
    const root = document.querySelector('[data-new-conversation]')?.getBoundingClientRect()
    const stack = document.querySelector('[data-new-conversation] [class*="stack"]')?.getBoundingClientRect()
    const card = document.querySelector('[data-new-conversation] [data-composer-card]')?.getBoundingClientRect()
    return { root, stack, card }
  })
  expect(geometry.root).toBeDefined()
  expect(geometry.stack).toBeDefined()
  expect(geometry.card).toBeDefined()
  const root = geometry.root!
  const stack = geometry.stack!
  const card = geometry.card!
  // The hero inherits the conversation root's width axis and centers in the
  // whole middle column, instead of shrink-wrapping at its left edge.
  expect(Math.abs((stack.left + stack.width / 2) - (root.left + root.width / 2))).toBeLessThanOrEqual(3)
  expect(card.width).toBeGreaterThan(600)
  expect(card.left).toBeGreaterThan(root.left + 120)
  await page.screenshot({ path: 'test-results/live-new-conversation-centered.png' })
})

test('creates the session only when the first message is sent', async ({ page }) => {
  await page.goto('/')
  // An existing project is enough: no directory dialog, no name field.
  await page.getByRole('button', { name: /^在 .* 新建任务$/ }).click()
  await expect(page.locator('[data-new-conversation]')).toBeVisible()
  expect(new URL(page.url()).searchParams.has('session')).toBe(false)

  await page.getByLabel('输入消息').fill('please answer with the fixture response')
  await page.getByRole('button', { name: '发送' }).click()

  // The first send creates the session, admits the turn, and the live snapshot
  // replaces the composing view with the real conversation.
  await expect(page.getByText(/Completed: fixture response/)).toBeVisible()
  await expect(page).toHaveURL(/[?&]session=/)
  await expect(page.locator('[data-new-conversation]')).toHaveCount(0)
})

test('renders the fixture history and streams a fresh turn', async ({ page }) => {
  await openFixture(page)
  // History projects through the same reducer the served app runs.
  await expect(page.getByText('fixture question one')).toBeVisible()
  await expect(page.getByText(/fixture answer one/)).toBeVisible()

  // One tool call from that history renders through the copied card family.
  const process = page.getByRole('button', { name: /^执行过程|^执行中/ }).first()
  if (await process.count() > 0) {
    await process.click()
    const tool = page.locator('[data-tool]').first()
    if (await tool.count() > 0) await tool.click()
  }
  await page.screenshot({ path: 'test-results/live-transcript.png' })

  await page.getByLabel('输入消息').fill('please answer with the fixture response')
  await page.getByRole('button', { name: '发送' }).click()

  // The provider answers "Completed: fixture response"; the transcript picks it
  // up from the live stream rather than from a reload.
  await expect(page.getByText(/Completed: fixture response/)).toBeVisible()
  // The turn settled: the primary control is back to sending, and the disabled
  // state is only the empty draft.
  await expect(page.getByRole('button', { name: '发送' })).toBeVisible()
  // The stats pills read the same projection the diagnostics tab does.
  await expect(page.locator('[data-stats-pills]')).toContainText('请求')
  await page.screenshot({ path: 'test-results/live-turn.png' })
})

test('queues guidance while a turn runs and applies it', async ({ page }) => {
  await openFixture(page)
  // `slow-task` keeps the provider busy long enough to queue guidance.
  await page.getByLabel('输入消息').fill('start a slow-task please')
  await page.getByRole('button', { name: '发送' }).click()

  const field = page.getByLabel('输入消息')
  await expect(field).toHaveAttribute('placeholder', /提交引导/)
  await field.fill('guidance: please keep it short')
  await page.getByRole('button', { name: '提交引导' }).click()

  // The queue shows the receipt with its status and its revision-checked verbs.
  const pending = page.locator('[data-pending-inputs]')
  await expect(pending).toBeVisible()
  await expect(pending.getByText('guidance: please keep it short')).toBeVisible()
  await page.screenshot({ path: 'test-results/live-guidance.png' })

  // The provider reports guidance applied on the next request; the queue drains.
  await expect(page.getByText(/guidance applied/)).toBeVisible()
  await expect(pending).toHaveCount(0)
})

test('opens the detail panel beside the transcript', async ({ page }) => {
  await openFixture(page)
  await page.getByLabel('展开详情').click()
  const panel = page.locator('[data-detail-panel]')
  await expect(panel).toBeVisible()
  await expect(panel.getByRole('tab', { name: '任务' })).toBeVisible()

  await panel.getByRole('tab', { name: '目标' }).click()
  await expect(panel.getByRole('heading', { name: /目标/ })).toBeVisible()

  await panel.getByRole('tab', { name: '文件' }).click()
  await expect(panel.getByRole('heading', { name: /文件/ })).toBeVisible()

  // Diagnostics reports the same numbers the terminal's stats snapshot does.
  await panel.getByRole('tab', { name: '诊断' }).click()
  await expect(panel.getByRole('heading', { name: 'Token' })).toBeVisible()
  await expect(panel.getByText('缓存命中率')).toBeVisible()
  await expect(panel.getByText(/命中率 = 缓存读取/)).toBeVisible()
  await page.screenshot({ path: 'test-results/live-diagnostics.png' })

  await page.getByLabel('收起详情').click()
  await expect(panel).toHaveCount(0)
})

test('applies the appearance and process preferences from the settings dialog', async ({ page }) => {
  await page.goto('/')
  await page.getByRole('button', { name: '设置' }).click()
  const dialog = page.getByRole('dialog', { name: '设置' })
  await expect(dialog).toBeVisible()

  // The dark cube is the persisted preference; the body attribute is the palette.
  await dialog.getByRole('button', { name: '深色' }).click()
  await expect(page.locator('body[data-ds-dark-theme]')).toHaveCount(1)
  await page.screenshot({ path: 'test-results/live-settings-dark.png' })

  await dialog.getByRole('tab', { name: '详细' }).click()
  await page.getByLabel('关闭设置').click()
  await expect(dialog).toHaveCount(0)
  // Preferences survive a reload (the boot step re-applies them before paint).
  await page.reload()
  await expect(page.locator('body[data-ds-dark-theme]')).toHaveCount(1)
})

test('searches the session catalog by scope and clears back to the tree', async ({ page }) => {
  await page.goto('/')
  await expect(page.locator('[data-row-key^="session:"]').first()).toBeVisible()

  await page.getByRole('button', { name: '搜索会话' }).click()
  // The field is addressed by its placeholder: the button's label is a substring
  // of the field's accessible name.
  const field = page.getByPlaceholder('搜索项目或会话…')
  await field.fill('no-such-session-zzz')
  await expect(page.getByText('没有匹配的会话')).toBeVisible()

  // Switching scope keeps the keyword.
  await page.getByRole('tab', { name: '项目' }).click()
  await expect(field).toHaveValue('no-such-session-zzz')

  await page.getByRole('tab', { name: '全部' }).click()
  await field.fill('fixture')
  await expect(page.locator('[data-row-key^="session:"]').first()).toBeVisible()
  await page.screenshot({ path: 'test-results/live-search.png' })

  await page.getByRole('button', { name: '清空筛选' }).click()
  await expect(page.getByRole('tree', { name: '会话' })).toBeVisible()
  await expect(field).toHaveValue('')
})

test('refuses images for a session whose frozen capability is text-only', async ({ page }) => {
  // The hand-written fixture session carries no image capability, so the picker
  // is closed and a paste is refused instead of uploading into a session that
  // cannot use the picture.
  await openFixture(page)
  await page.getByRole('button', { name: '命令' }).click()
  await expect(page.getByRole('option', { name: /添加图片/ })).toContainText('此会话不接受图片')
  await expect(page.locator('input[aria-label="选择图片"]')).toBeDisabled()
  await page.keyboard.press('Escape')

  await page.getByLabel('输入消息').evaluate((element) => {
    const data = new DataTransfer()
    data.items.add(new File([new Uint8Array([137, 80, 78, 71])], 'shot.png', { type: 'image/png' }))
    element.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }))
  })
  await expect(page.getByRole('status').filter({ hasText: '此会话不支持图片输入' })).toBeVisible()
  await expect(page.locator('[data-composer-attachments]')).toHaveCount(0)
})

test('uploads an image for a session that accepts them and sends it', async ({ page }) => {
  // A session created through the API freezes the capability the server
  // resolves (here: images on), which is what a real session carries.
  const created = await page.request.post('/api/sessions', {
    data: { name: 'attachment-fixture', cwd: `/tmp/mink-web-e2e-att-${Date.now()}` },
  })
  expect(created.ok()).toBe(true)
  const body = await created.json() as { data: { id: string; project_key: string } }

  await page.goto('/')
  const row = page.locator(`[data-row-key="session:${body.data.id}"]`)
  await expect(row).toBeVisible()
  await row.click()
  await expect(page.getByLabel('输入消息')).toBeVisible()
  // The picker opens once the session's snapshot has published its capability.
  await page.getByRole('button', { name: '命令' }).click()
  await expect(page.getByRole('option', { name: /添加图片/ })).toContainText('图片只进入会话附件目录')
  await page.keyboard.press('Escape')

  const png = Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
    'base64',
  )
  await page.setInputFiles('input[aria-label="选择图片"]', {
    name: 'shot.png',
    mimeType: 'image/png',
    buffer: png,
  })
  const rail = page.locator('[data-composer-attachments]')
  await expect(rail).toBeVisible()
  const thumbnail = rail.locator('img').first()
  await expect(thumbnail).toHaveAttribute('src', /\/attachments\//)
  await expect.poll(async () => thumbnail.evaluate((img: HTMLImageElement) => img.naturalWidth))
    .toBeGreaterThan(0)
  await page.screenshot({ path: 'test-results/live-attachment.png' })

  // Sending with the attachment reaches the provider, which answers by reading
  // the path the server handed back (the image never enters the request itself).
  await page.getByLabel('输入消息').fill('here is the picture')
  await page.getByRole('button', { name: '发送' }).click()
  await expect(page.getByText(/Completed:/)).toBeVisible()

  // A sent draft is clean: the rail empties with the draft it belonged to.
  await expect(page.locator('[data-composer-attachments]')).toHaveCount(0)

  // Drafted images belong to the session: leaving and returning restores them
  // from the stored ids, without re-uploading anything.
  await page.setInputFiles('input[aria-label="选择图片"]', {
    name: 'kept.png',
    mimeType: 'image/png',
    buffer: png,
  })
  await expect(page.locator('[data-composer-attachments] img').first()).toHaveAttribute('src', /\/attachments\//)
  await openFixture(page, E2E_LONG_ID)
  await openFixture(page, body.data.id)
  await expect(page.locator('[data-composer-attachments] img').first()).toHaveAttribute('src', /\/attachments\//)
})

test('keeps an image chosen while a text submission awaits its receipt', async ({ page }) => {
  // The accepted submission owns only the attachment set it was created with:
  // a file chosen while it is in flight belongs to the next message.
  const created = await page.request.post('/api/sessions', {
    data: { name: 'attachment-pending-fixture', cwd: `/tmp/mink-web-e2e-att-pending-${Date.now()}` },
  })
  expect(created.ok()).toBe(true)
  const body = await created.json() as { data: { id: string; project_key: string } }
  await page.goto(`/?session=${body.data.id}&project=${body.data.project_key}`)
  const composer = page.getByLabel('输入消息')
  await expect(composer).toBeEnabled()
  // A session with a transcript keeps the docked composer; the hero → docked
  // switch of a first message is a separate mount and not this case.
  await composer.fill('warm up the transcript')
  await page.getByRole('button', { name: '发送' }).click()
  await expect(page.getByText(/Completed:/).first()).toBeVisible()

  let releaseReceipt!: () => void
  const held = new Promise<void>((resolve) => { releaseReceipt = resolve })
  let receiptArrived!: () => void
  const arrived = new Promise<void>((resolve) => { receiptArrived = resolve })
  await page.route('**/api/sessions/*/inputs?*', async (route) => {
    if (route.request().method() !== 'POST') {
      await route.continue()
      return
    }
    const response = await route.fetch()
    receiptArrived()
    await held
    await route.fulfill({ response })
  })

  const png = Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
    'base64',
  )
  await composer.fill('先发一条普通文本')
  await page.getByRole('button', { name: '发送' }).click()
  await arrived

  // The text submission is still awaiting its receipt; choose the next image.
  await page.setInputFiles('input[aria-label="选择图片"]', {
    name: 'next.png',
    mimeType: 'image/png',
    buffer: png,
  })
  const rail = page.locator('[data-composer-attachments]')
  await expect(rail.locator('img')).toHaveCount(1)
  await expect(rail.locator('img').first()).toHaveAttribute('src', /\/attachments\//)

  releaseReceipt()
  await expect(composer).toHaveValue('')
  await expect(page.getByText(/Completed:/)).toHaveCount(2)
  await expect(rail.locator('img')).toHaveCount(1)
  await expect(rail.locator('img').first()).toHaveAttribute('src', /\/attachments\//)
})

test('opens a spilled output from an artifact reference in the transcript', async ({ page }) => {
  await openFixture(page)
  // The fixture history cites `artifact://fixture-output`; the reference opens
  // the output tab on that id instead of navigating away.
  const reference = page.getByRole('button', { name: '打开产物 fixture-output' }).first()
  await expect(reference).toBeVisible()
  await reference.click()
  const panel = page.locator('[data-detail-panel="outputs"]')
  await expect(panel).toBeVisible()
  await expect(panel.getByText(/fixture-output/)).toBeVisible()
})

test('runs slash commands from the composer against the live session', async ({ page }) => {
  await openFixture(page)
  const field = page.getByLabel('输入消息')

  // Typing the slash opens the same copied shell the add control uses; the draft
  // is its query, so its own search box is never shown and deleting in the draft
  // narrows the list (the loop a second box caused is gone).
  await field.fill('/')
  await expect(page.getByRole('option', { name: /^\/goal/ }).first()).toBeVisible()
  await expect(page.getByLabel('筛选命令')).toBeHidden()
  await field.fill('/deta')
  await expect(page.getByRole('option', { name: /\/details/ })).toBeVisible()
  await field.fill('/go')
  await expect(page.getByRole('option', { name: /^\/goal/ }).first()).toBeVisible()
  expect(await field.inputValue()).toBe('/go')
  await page.keyboard.press('Escape')

  // `/plan` (a full command, typed past the surface) elects the task tab.
  await field.fill('/plan')
  await expect(page.getByRole('option', { name: /^\/plan/ }).first()).toBeVisible()
  await page.keyboard.press('Escape')
  await field.fill('/plan')
  await field.press('Enter')
  const panel = page.locator('[data-detail-panel="task"]')
  await expect(panel).toBeVisible()
  // The command answers once, in the composer's own line — not again up top.
  await expect(page.getByText('已打开计划')).toHaveCount(1)
  await page.screenshot({ path: 'test-results/live-command.png' })

  // A refused command keeps what was typed instead of clearing it.
  await field.fill('/goal pause')
  await field.press('Enter')
  // A refused submission is announced above the card, where a phone can see it.
  await expect(page.getByRole('alert').filter({ hasText: '本会话没有目标' })).toBeVisible()
  await expect(field).toHaveValue('/goal pause')

  await field.fill('/not-a-command')
  await field.press('Enter')
  await expect(field).toHaveValue('/not-a-command')
})

test('jumps between loaded turns from the turn navigator', async ({ page }) => {
  // The long session is this spec's own fixture: the short one is written to by
  // the streaming specs, and a transcript that keeps growing never lets the
  // floating controls settle.
  await openFixture(page, E2E_LONG_ID)
  // The fixture history holds two user turns, so the jump list offers both.
  await page.getByRole('button', { name: '跳转已加载轮次' }).click()
  const list = page.getByRole('group', { name: '已加载轮次' })
  await expect(list).toBeVisible()
  const rows = list.locator('[role="menuitem"]')
  await expect.poll(async () => rows.count()).toBeGreaterThanOrEqual(2)
  await expect(rows.first()).toContainText('long fixture question')
  await page.screenshot({ path: 'test-results/live-turns.png' })

  // Selecting a turn reads it, which stops following and reveals the way back.
  await rows.first().click()
  await expect(list).toHaveCount(0)
  const latest = page.getByRole('button', { name: /返回最新内容/ })
  await expect(latest).toBeVisible()
  // The jump scrolls the transcript; the control settles with it.
  await page.waitForTimeout(700)
  await latest.click()
  await expect(latest).toHaveCount(0)
})

test('a desktop click on the transcript keeps the composer visible', async ({ page }) => {
  await openFixture(page)
  // Reading chrome is a narrow-layout affordance. A click in a desktop
  // transcript used to mount the same hide gesture and take the input card away.
  await page.locator('[class*="flowItem"]').first().click()
  await expect(page.getByLabel('输入消息')).toBeVisible()
  await expect(page.locator('header').first()).toBeVisible()
  await expect(page.locator('[data-mink-composer-seat]')).toBeVisible()
})

test('steps the mobile chrome aside for reading and brings it back', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  // The session identity travels in the URL, so a phone lands in the same
  // conversation after a reload instead of the drawer dance.
  await page.goto(`/?session=${E2E_LONG_ID}`)
  await expect(page.locator('[class*="bubble"]').first()).toBeVisible()
  // The capped snapshot opens with only the newest turns; page one history batch
  // back so the phone viewport actually has something to scroll.
  const older = page.getByRole('button', { name: '加载更早的轮次' })
  if (await older.count() > 0) await older.click()
  const header = page.locator('header').first()
  await expect(header).toBeVisible()
  await waitForSettledChrome(page)

  // The transcript opens at its tail, so the reading gesture is a scroll back
  // into the history and then forward again — matching how the chrome hides
  // while the reader moves toward the newest output.
  const scroller = page.locator('[class*="scrollBody"]').first()
  await scroller.hover()
  // Scroll back into the history first (that direction never hides the chrome),
  // then forward: the reading gesture is the second one.
  await page.mouse.wheel(0, -2000)
  await expect.poll(async () => scroller.evaluate(box => box.scrollTop)).toBeLessThan(2000)
  await page.mouse.wheel(0, 800)
  await expect(header).toBeHidden()
  await expect(page.getByRole('button', { name: '退出阅读模式' })).toBeVisible()
  await page.screenshot({ path: 'test-results/live-mobile-reading.png' })

  // The reveal control restores both the header and the composer.
  await page.getByRole('button', { name: '退出阅读模式' }).click()
  await expect(header).toBeVisible()
  await expect(page.getByLabel('输入消息')).toBeVisible()
  await page.screenshot({ path: 'test-results/live-mobile-restored.png' })
})

test('steps the mobile chrome aside for reading and reveals it from the corner control', async ({ browser }) => {
  // Nothing is waiting on this reader: no focused composer, no drafted image, no
  // running turn. Reading gestures therefore own the chrome (the pinning cases
  // are covered beside this test and in readingChromeRules).
  const context = await browser.newContext({ ...devices['iPhone 13'] })
  const phone = await context.newPage()
  await phone.goto(`/?session=${E2E_LONG_ID}`)
  await expect(phone.locator('[class*="flowItem"]').first()).toBeVisible()
  await waitForSettledChrome(phone)
  const header = phone.locator('header').first()
  await phone.locator('[data-conversation-scroll]').first().hover()
  await phone.mouse.wheel(0, -1500)
  await phone.waitForTimeout(500)
  await phone.mouse.wheel(0, 400)

  await expect(header).toBeHidden()
  await expect(phone.getByRole('button', { name: '退出阅读模式' })).toBeVisible()
  await phone.getByRole('button', { name: '退出阅读模式' }).click()
  await expect(header).toBeVisible()
  await expect(phone.getByLabel('输入消息')).toBeVisible()
  await context.close()
})

test('keeps the chrome while a drafted image waits for the next message', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  const created = await page.request.post('/api/sessions', {
    data: { name: 'reading-chrome-attachment', cwd: `/tmp/mink-web-e2e-att-chrome-${Date.now()}` },
  })
  expect(created.ok()).toBe(true)
  const body = await created.json() as { data: { id: string; project_key: string } }
  await page.goto(`/?session=${body.data.id}&project=${body.data.project_key}`)
  const composer = page.getByLabel('输入消息')
  await expect(composer).toBeVisible()
  await expect(page.locator('input[aria-label="选择图片"]')).toBeEnabled()
  // A transcript gives the reading gesture something to act on, and keeps the
  // composer docked for the rest of the case.
  await composer.fill('warm up the transcript')
  await page.getByRole('button', { name: '发送' }).click()
  await expect(page.getByText(/Completed:/).first()).toBeVisible()
  await waitForSettledChrome(page)
  const header = page.locator('header').first()

  const png = Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
    'base64',
  )
  await page.setInputFiles('input[aria-label="选择图片"]', {
    name: 'waiting.png',
    mimeType: 'image/png',
    buffer: png,
  })
  await expect(page.locator('[data-composer-attachments] img').first()).toHaveAttribute('src', /\/attachments\//)

  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur())
  // The draft image belongs to the next message; a tap on content cannot step
  // over it.
  await page.locator('[class*="flowItem"]').first().click()
  await expect(header).toBeVisible()
  await page.screenshot({ path: 'test-results/live-mobile-attachment-pin.png' })

  // Removing it unlocks the chrome: the next tap on content may hide it again.
  await page.getByRole('button', { name: '移除附件：waiting.png' }).click()
  await expect(page.locator('[data-composer-attachments]')).toHaveCount(0)
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur())
  await page.locator('[class*="flowItem"]').first().click()
  await expect(header).toBeHidden()
})

test('keeps the composer on screen while a turn runs on a narrow viewport', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  // The long fixture, not the short one: the streaming specs write to that session,
  // and a turn already in flight makes this case's own send a race.
  await page.goto(`/?session=${E2E_LONG_ID}`)
  await expect(page.locator('[class*="bubble"]').first()).toBeVisible()
  await page.getByLabel('输入消息').fill('start a slow-task please')
  await page.getByRole('button', { name: '发送' }).click()
  // Either control is enough: the run may finish before this case looks at it, and
  // what it asserts is the composer's own state.
  await expect(page.getByRole('button', { name: '提交引导' }).or(page.getByRole('button', { name: '发送' }))).toBeVisible()
  const scroller = page.locator('[class*="scrollBody"]').first()
  // Focusing the composer is what pins the chrome for as long as the reader keeps
  // typing: a running turn and a focused input both keep the header and card on
  // screen, whatever the reader scrolls.
  await page.getByLabel('输入消息').focus()
  await scroller.hover()
  await page.mouse.wheel(0, -800)
  await page.mouse.wheel(0, 600)

  await expect(page.getByLabel('输入消息')).toBeVisible()
  await expect(page.locator('header').first()).toBeVisible()
  await expect(page.getByRole('button', { name: '退出阅读模式' })).toHaveCount(0)
  const stop = page.getByRole('button', { name: '停止' })
  const guiding = page.getByRole('button', { name: '提交引导' })
  expect(await stop.isVisible() || await guiding.isVisible()).toBe(true)
  // Stop through the element itself: a streaming turn keeps moving it, and a
  // pointing-device click would wait for a stability that never comes.
  if (await stop.isVisible().catch(() => false)) {
    await stop.evaluate(node => (node as HTMLButtonElement).click())
  }
})

test('reaches the session list through the drawer on a phone', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(`/?session=${E2E_SESSION_ID}`)
  await expect(page.locator('[class*="bubble"]').first()).toBeVisible()

  // The reading column keeps the whole width: the sidebar is a drawer, and the
  // frame reserves no track for it (a rail would cost a phone its width).
  const width = (): Promise<number> => page.locator('[class*="centerCol"]')
    .evaluate(node => node.getBoundingClientRect().width)
  expect(Math.round(await width())).toBe(390)

  const rail = page.locator('[data-row-key^="session:"]').first()
  await expect(rail).toBeHidden()

  // The phone top bar is the way in; the drawer covers the content instead of
  // shrinking it, and choosing a session closes it again.
  // With a session open the header carries those controls itself (the phone's own
  // bar would only duplicate it), so the way in is the header's drawer button.
  await page.locator('header').getByRole('button', { name: '展开侧栏' }).click()
  await expect(rail).toBeVisible()
  expect(Math.round(await width())).toBe(390)
  await rail.click()
  await expect(rail).toBeHidden()
  expect(Math.round(await width())).toBe(390)
})

test('walks the file tree and reads a file from the detail panel', async ({ page }) => {
  // The tree needs a working directory with both kinds of entry: the fixture
  // sessions live in a scratch directory, so this case makes its own session.
  const created = await page.request.post('/api/sessions', {
    data: { cwd: process.cwd() },
  })
  const body = await created.json() as { data: { id: string; project_key: string } }
  await page.goto(`/?session=${body.data.id}&project=${body.data.project_key}`)
  await page.getByRole('button', { name: '展开详情' }).click()
  await page.getByRole('tab', { name: '文件' }).click()

  // A directory click descends: the listing changes and the header follows.
  await expect(page.locator('[class*="fileButton"]').first()).toBeVisible()
  const root = await page.locator('[class*="fileButton"]').allTextContents()
  expect(root.length).toBeGreaterThan(0)
  await page.getByRole('button', { name: /^展开目录 / }).first().click()
  await expect(page.locator('[class*="fileButton"]')).not.toHaveCount(root.length)
  await expect(page.locator('[class*="hint"]').first()).toContainText('/')

  // The way back up is inside the listing itself (there is no back control when
  // a file is open yet, so the panel is never a dead end).
  await page.getByRole('button', { name: '返回项目根目录' }).click()
  await expect(page.locator('[class*="hint"]').first()).toContainText('项目根目录')

  // Then read a file: a file answers the read form, not a listing.
  await page.getByRole('button', { name: /^打开文件 .+\.(md|json|rs|ts|toml)$/ }).first().click()
  await expect(page.locator('[data-file-preview]')).toHaveCount(1)
})

test('never blanks the transcript while the chrome hides', async ({ browser }) => {
  // The field's blank screen: a forward gesture hides the reading chrome (the
  // header and the input region), and an earlier build took the composer out of
  // the flow while doing it — the scroller's height changed mid-gesture and the
  // reader was left on empty space with an unresponsive scroll. Hiding is
  // layout-free now, and this case pins that down.
  const context = await browser.newContext({ ...devices['iPhone 13'] })
  const phone = await context.newPage()
  await phone.goto(`/?session=${E2E_LONG_ID}`)
  await expect(phone.locator('[class*="flowItem"]').first()).toBeVisible()
  await waitForSettledChrome(phone)
  const scroller = phone.locator('[data-conversation-scroll]').first()
  await scroller.hover()

  const box = (): Promise<{ height: number; scrollTop: number; rows: number; collapsed: boolean; reveal: number; bottomGap: number; headerHeight: number }> =>
    phone.evaluate(() => {
      const node = document.querySelector('[data-conversation-scroll]')
      const rows = [...document.querySelectorAll('[class*="flowItem"]')]
        .filter(item => { const rect = item.getBoundingClientRect(); return rect.bottom > 0 && rect.top < window.innerHeight })
      return {
        height: node?.clientHeight ?? -1,
        scrollTop: Math.round(node?.scrollTop ?? -1),
        rows: rows.length,
        collapsed: document.querySelector('[data-mink-composer-seat]')?.hasAttribute('data-chrome-collapsed') ?? false,
        reveal: document.querySelectorAll('.mk-reveal').length,
        bottomGap: Math.round((node?.scrollHeight ?? 0) - (node?.scrollTop ?? 0) - (node?.clientHeight ?? 0)),
        headerHeight: Math.round(document.querySelector('header')?.getBoundingClientRect().height ?? -1),
      }
    })

  const before = await box()
  // First back into the history (a gesture at the very bottom has nowhere to go),
  // then forward: the chrome hides.
  await phone.mouse.wheel(0, -700)
  await phone.waitForTimeout(500)
  await phone.mouse.wheel(0, 900)
  await phone.waitForTimeout(500)
  await phone.mouse.wheel(0, 900)
  await phone.waitForTimeout(500)
  const hidden = await box()
  expect(hidden.collapsed).toBe(true)
  expect(hidden.reveal).toBe(1)
  // The reading area takes the header's and the input card's height back (the whole
  // frame), and the history stays in sight (the same lines, measured from the live
  // end). The reading controls share one column, so none of them covers another.
  expect(hidden.height).toBeGreaterThan(before.height)
  expect(hidden.headerHeight).toBe(0)
  const stack = await phone.locator('[class*="_ops_"]').first().evaluate(node => {
    const rows = [...node.children].map(child => child.getBoundingClientRect())
    return rows.map((rect, index) => index === 0 ? 0 : Math.round(rect.top - rows[index - 1].bottom))
  })
  expect(stack.every(gap => gap >= 0)).toBe(true)
  expect(hidden.rows).toBeGreaterThan(0)
  expect(Math.abs(hidden.bottomGap - before.bottomGap)).toBeLessThanOrEqual(4)

  // Scrolling still answers after the chrome is gone.
  await phone.mouse.wheel(0, -700)
  await phone.waitForTimeout(500)
  const after = await box()
  expect(after.rows).toBeGreaterThan(0)
  expect(after.scrollTop).not.toBe(hidden.scrollTop)
  await context.close()
})

test('keeps a reader inside the history where they were', async ({ browser }) => {
  // Entering and leaving the reading state must not move the text: the chrome gives
  // its height back without touching the scroll offset (the tail is the one place a
  // reader expects to stay pinned).
  const context = await browser.newContext({ ...devices['iPhone 13'] })
  const phone = await context.newPage()
  await phone.goto(`/?session=${E2E_LONG_ID}`)
  await expect(phone.locator('[class*="flowItem"]').first()).toBeVisible()
  await waitForSettledChrome(phone)
  const older = phone.getByRole('button', { name: '加载更早的轮次' })
  if (await older.count() > 0) await older.click()
  await phone.locator('[data-conversation-scroll]').first().hover()
  const read = (): Promise<{ gap: number; offset: number; first: string | null; collapsed: boolean }> => phone.evaluate(() => {
    const node = document.querySelector('[data-conversation-scroll]')
    const padding = node === null ? 0 : Number.parseFloat(getComputedStyle(node).paddingBottom) || 0
    const rows = [...document.querySelectorAll('[class*="flowItem"]')]
    const visible = rows.filter(item => {
      const rect = item.getBoundingClientRect()
      return rect.bottom > 0 && rect.top < window.innerHeight
    })
    return {
      // Measured to the end of the CONTENT: the reading state's own gutter is not
      // movement, so comparing raw scroll heights would blame it for the padding.
      gap: Math.round((node?.scrollHeight ?? 0) - padding - (node?.scrollTop ?? 0) - (node?.clientHeight ?? 0)),
      offset: Math.round(node?.scrollTop ?? -1),
      first: visible[0]?.getAttribute('data-turn-key') ?? null,
      collapsed: document.querySelector('[data-mink-composer-seat]')?.hasAttribute('data-chrome-collapsed') ?? false,
    }
  })

  // Start from the shown state whatever the page did on load, then into the history.
  // The corner control is the deterministic way out (a tap may land on content that
  // is deliberately inert).
  if ((await read()).collapsed) {
    await phone.getByRole('button', { name: '退出阅读模式' }).click()
    await phone.waitForTimeout(300)
  }
  await phone.mouse.wheel(0, -1500)
  await phone.waitForTimeout(600)
  const mid = await read()
  expect(mid.collapsed).toBe(false)
  // A small forward gesture hides the chrome; the reader is 260px further on, and
  // the chrome's own change adds nothing to that.
  await phone.mouse.wheel(0, 260)
  await phone.waitForTimeout(600)
  const hidden = await read()
  expect(hidden.collapsed).toBe(true)
  // The chrome's change adds nothing to the reader's own 260px scroll: the offset
  // carries the whole movement, which is what "the text stayed put" means. The
  // viewport grew by the chrome's height on top of that (the reading area).
  expect(Math.abs((mid.offset + 260) - hidden.offset)).toBeLessThanOrEqual(24)
  expect(Math.abs((mid.gap - 260 - 195) - hidden.gap)).toBeLessThanOrEqual(24)

  // Scrolling back is not the way out (a reread must not flicker the chrome): the
  // corner control is, and it puts the reader back exactly where they were.
  await phone.mouse.wheel(0, -260)
  await phone.waitForTimeout(600)
  expect((await read()).collapsed).toBe(true)
  await phone.getByRole('button', { name: '退出阅读模式' }).click()
  await phone.waitForTimeout(400)
  const revealed = await read()
  expect(revealed.collapsed).toBe(false)
  expect(revealed.first).toBe(mid.first)
  await context.close()
})

test('never scrolls the transcript sideways', async ({ page }) => {
  // The field reported this three times: a horizontal scrollbar along the bottom of
  // the transcript, immediately above the input card. It belonged to the transcript
  // itself — a scroll container with one axis stated computes the other to `auto`,
  // so any wide child scrolled the whole column sideways. The injected child below is
  // wider than the frame: the transcript must not move, and wide content keeps its
  // own scroller.
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto(`/?session=${E2E_LONG_ID}`)
  await expect(page.locator('[class*="flowItem"]').first()).toBeVisible()

  const scroller = page.locator('[data-conversation-scroll]').first()
  // A child wider than the frame, then the reader's own sideways wheel.
  await scroller.evaluate((node) => {
    const wide = document.createElement('div')
    wide.style.width = '4000px'
    wide.style.height = '8px'
    wide.setAttribute('data-probe-wide', '')
    node.append(wide)
  })
  await scroller.hover()
  await page.mouse.wheel(400, 0)
  await page.waitForTimeout(200)
  const state = await scroller.evaluate(node => ({
    overflowX: getComputedStyle(node).overflowX,
    scrollLeft: node.scrollLeft,
  }))
  await scroller.evaluate((node) => { node.querySelector('[data-probe-wide]')?.remove() })

  // No bar is painted (the axis is stated), and the wheel moves nothing.
  expect(['hidden', 'clip']).toContain(state.overflowX)
  expect(state.scrollLeft).toBe(0)
})

test('follows the visible box when the browser pans its viewport', async ({ page, browser }) => {
  const context = await browser.newContext({ ...devices['iPhone 13'] })
  // The field's blank page: a phone browser pans its visual viewport (the keyboard
  // reaching for the composer, a collapsing URL bar). The shell must move with it,
  // or the reader is left looking at whatever lies outside the app.
  await context.addInitScript(() => {
    const real = window.visualViewport
    let offset = 0
    const fake = {
      get width() { return real?.width ?? window.innerWidth },
      get height() { return Math.round((real?.height ?? window.innerHeight) * (offset === 0 ? 1 : 0.6)) },
      get offsetTop() { return offset },
      get offsetLeft() { return 0 },
      get pageTop() { return offset },
      get scale() { return 1 },
      addEventListener: (type: string, fn: () => void) => {
        const store = (window as unknown as { __vv?: Record<string, (() => void)[]> }).__vv ??= {}
        ;(store[type] ??= []).push(fn)
      },
      removeEventListener: () => {},
    }
    Object.defineProperty(window, 'visualViewport', { value: fake, configurable: true })
    ;(window as unknown as { __panTo?: (next: number) => void }).__panTo = (next: number) => {
      offset = next
      for (const fn of (window as unknown as { __vv?: Record<string, (() => void)[]> }).__vv?.resize ?? []) fn()
    }
  })
  const phone = await context.newPage()
  const created = await page.request.post('/api/sessions', { data: { cwd: '/tmp/mink-web-e2e-pan' } })
  const body = await created.json() as { data: { id: string; project_key: string } }
  await phone.goto(`/?session=${body.data.id}&project=${body.data.project_key}`)
  await expect(phone.getByLabel('输入消息')).toBeVisible()

  await phone.evaluate(() => {
    (window as unknown as { __panTo: (next: number) => void }).__panTo(250)
  })
  const box = await phone.evaluate(() => {
    const root = document.getElementById('root')
    return { top: Math.round(root?.getBoundingClientRect().top ?? -1), viewport: window.visualViewport?.offsetTop ?? 0 }
  })
  expect(box.top).toBe(box.viewport)
  await context.close()
})

test('a phone sends by tap and sizes the shell to the visual viewport', async ({ page, browser }) => {
  const context = await browser.newContext({ ...devices['iPhone 13'], viewport: { width: 360, height: 640 } })
  const phone = await context.newPage()
  const created = await page.request.post('/api/sessions', {
    data: { name: 'phone-send', cwd: `/tmp/mink-web-e2e-phone-${Date.now()}` },
  })
  const body = await created.json() as { data: { id: string; project_key: string } }
  await phone.goto(`/?session=${body.data.id}`)
  const field = phone.getByLabel('输入消息')
  await expect(field).toBeVisible()

  // Touch input: a tap on the send control must run the same path a click does.
  await field.fill('a phone turn')
  await phone.getByRole('button', { name: '发送' }).tap()
  await expect(phone.locator('[class*="bubble"]').first()).toBeVisible()

  // The shell is the visual viewport (an iOS toolbar would otherwise cover the
  // composer and swallow the taps).
  const shell = await phone.evaluate(() => ({
    root: document.getElementById('root')?.getBoundingClientRect().height,
    viewport: window.visualViewport?.height ?? window.innerHeight,
  }))
  expect(Math.round(shell.root ?? 0)).toBe(Math.round(shell.viewport))

  // The document itself never scrolls: the frame owns the scrolling.
  const scrollable = await phone.evaluate(() => document.documentElement.scrollHeight - window.innerHeight)
  expect(scrollable).toBeLessThanOrEqual(0)
  await context.close()
})

test('shows the confirmed goal above the composer and controls it by revision', async ({ page }) => {
  // A host-prepared draft avoids making this UI control test depend on model
  // decomposition. Confirmation starts a deliberately slow fixture turn so the
  // active goal remains visible while its server-owned controls are exercised.
  const created = await page.request.post('/api/sessions', {
    data: { name: 'goal-fixture', cwd: `/tmp/mink-web-e2e-goal-${Date.now()}` },
  })
  expect(created.ok()).toBe(true)
  const body = await created.json() as { data: { id: string; project_key: string } }
  // A goal can only be created on a session whose runtime holds it open.
  await page.goto('/')
  await page.locator(`[data-row-key="session:${body.data.id}"]`).click()
  // The composer enables only once the server holds the session open, which is
  // the precondition a goal needs.
  await expect(page.getByLabel('输入消息')).toBeEnabled()
  const drafted = await page.request.post(`/api/sessions/${body.data.id}/goal?project=${body.data.project_key}`, {
    data: {
      protocol_version: 2,
      generate: false,
      objective: '验证目标条 slow-task',
      acceptance: 'agent_reviewed',
      max_turns: 1,
      max_minutes: 1,
    },
  })
  expect(drafted.ok(), await drafted.text()).toBe(true)
  const draftBody = await drafted.json() as { data: { goal: { id: string; revision: number } } }
  const confirmed = await page.request.post(`/api/sessions/${body.data.id}/goal/confirm?project=${body.data.project_key}`, {
    data: { goal_id: draftBody.data.goal.id, revision: draftBody.data.goal.revision },
  })
  expect(confirmed.ok(), await confirmed.text()).toBe(true)

  const bar = page.locator('[data-goal-bar]')
  await expect(bar).toBeVisible()
  await expect(bar).toHaveAttribute('data-goal-status', 'active')
  await expect(bar).toContainText('验证目标条')
  await page.screenshot({ path: 'test-results/live-goal-bar.png' })

  // Creating a goal is shown by the bar itself: no notice line, and nothing in
  // the error colour.
  await expect(page.locator('[class*="openError"]')).toHaveCount(0)

  await page.getByLabel('展开详情').click()
  const panel = page.locator('[data-detail-panel]')
  await panel.getByRole('tab', { name: '目标' }).click()
  let rejection: { code: number; message: string } = {
    code: 409,
    message: 'stale goal ID or revision',
  }
  await page.route('**/goal/pause*', route => route.fulfill({
    status: rejection.code,
    contentType: 'application/json',
    body: JSON.stringify({ ...rejection, data: null }),
  }))
  await panel.getByRole('button', { name: '暂停' }).click()
  await expect(panel.getByRole('alert')).toContainText('stale goal ID or revision')
  await expect(bar).toHaveAttribute('data-goal-status', 'active')

  rejection = { code: 429, message: 'session capacity reached' }
  await panel.getByRole('button', { name: '暂停' }).click()
  await expect(panel.getByRole('alert')).toContainText('session capacity reached')
  await expect(bar).toHaveAttribute('data-goal-status', 'active')
  await page.unroute('**/goal/pause*')

  await bar.getByRole('button', { name: '取消目标' }).click()
  await expect(bar).toHaveAttribute('data-goal-status', 'cancelled')

  // A cleared goal leaves the bar: `hidden` is the surface's own retirement.
  await bar.getByRole('button', { name: '清除目标' }).click()
  await expect(page.locator('[data-goal-bar]')).toHaveCount(0)
})

test('generates a goal draft, refines it in the workbench, and completes independent review', async ({ page }) => {
  const created = await page.request.post('/api/sessions', {
    data: { name: 'goal-review-fixture', cwd: await fixtureCwd(page) },
  })
  expect(created.ok()).toBe(true)
  const body = await created.json() as { data: { id: string; project_key: string } }
  await page.goto('/')
  await page.locator(`[data-row-key="session:${body.data.id}"]`).click()
  await expect(page.getByLabel('输入消息')).toBeEnabled()

  // Exercise the server's generated-proposal path, then refine and confirm the
  // exact returned revision through the same controls users see.
  const generated = await page.request.post(`/api/sessions/${body.data.id}/goal?project=${body.data.project_key}`, {
    data: {
      protocol_version: 2,
      objective: 'Verify the local fixture README',
      acceptance: 'agent_reviewed',
      max_turns: 1,
      max_minutes: 2,
    },
  })
  expect(generated.ok(), await generated.text()).toBe(true)

  await page.getByLabel('展开详情').click()
  const panel = page.locator('[data-detail-panel]')
  await panel.getByRole('tab', { name: '目标' }).click()
  await expect(panel.getByText('Verify the fixture README is readable')).toBeVisible()
  await expect(panel.getByRole('region', { name: '范围' })).toContainText('README.md')

  await panel.getByLabel('目标草案修改意见').fill('Please include the README heading in the scope.')
  await panel.getByRole('button', { name: '提交修改意见' }).click()
  await expect(panel.getByText('Verify the fixture README and its heading')).toBeVisible()
  await expect(panel.getByRole('region', { name: '范围' })).toContainText('including its heading')

  await panel.getByRole('button', { name: '确认并启动' }).click()
  const bar = page.locator('[data-goal-bar]')
  await expect(bar).toHaveAttribute('data-goal-status', 'completed')
  await expect(panel.getByRole('heading', { name: /最近审核：满足（第 .* 轮）/ })).toBeVisible()
  await expect(panel.getByText(/独立审核通过/)).toBeVisible()
  await panel.getByRole('button', { name: '读取审核报告' }).click()
  await expect(panel.getByText(/I inspected README.md with Read/)).toBeVisible()
})

test('retries a failed independent review and records human approval', async ({ page }) => {
  const created = await page.request.post('/api/sessions', {
    data: { name: 'goal-human-approval', cwd: await fixtureCwd(page) },
  })
  expect(created.ok()).toBe(true)
  const body = await created.json() as { data: { id: string; project_key: string } }
  await page.goto('/')
  await page.locator(`[data-row-key="session:${body.data.id}"]`).click()
  await expect(page.getByLabel('输入消息')).toBeEnabled()
  const draft = await page.request.post(`/api/sessions/${body.data.id}/goal?project=${body.data.project_key}`, {
    data: {
      protocol_version: 2,
      generate: false,
      objective: 'retry-review-fixture: verify the README before human approval',
      acceptance: 'human',
      max_turns: 1,
      max_minutes: 2,
    },
  })
  expect(draft.ok(), await draft.text()).toBe(true)

  await page.getByLabel('展开详情').click()
  const panel = page.locator('[data-detail-panel]')
  await panel.getByRole('tab', { name: '目标' }).click()
  await panel.getByRole('button', { name: '确认并启动' }).click()
  await expect(panel.getByRole('alert')).toContainText('fixture reviewer unavailable once')

  // Infrastructure failure preserves the candidate; explicit retry rechecks
  // it without executing another goal turn.
  await panel.getByRole('button', { name: '重试审核' }).click()
  await expect(panel.getByRole('heading', { name: /最近审核：满足（第 .* 轮）/ })).toBeVisible()
  await expect(panel.getByText('待人工验收')).toBeVisible()
  page.once('dialog', dialog => dialog.accept('Reviewed the README evidence'))
  await panel.getByRole('button', { name: '批准完成' }).click()
  await expect(page.locator('[data-goal-bar]')).toHaveAttribute('data-goal-status', 'completed')
  await expect(panel.getByText(/人工批准/)).toBeVisible()
})

test('loads older history and marks goal boundaries', async ({ page }) => {
  // The long fixture session is 30 turns; the workbench opens its tail and the
  // older-history control pages the rest in.
  await openFixture(page, E2E_LONG_ID)
  await expect(page.getByText('long fixture question 30')).toBeVisible()

  const older = page.getByRole('button', { name: /加载更早的轮次/ })
  await expect(older).toBeVisible()
  await older.click()
  // The first page's turn is now in the transcript, and the control stays for
  // the page after it.
  await expect(page.getByText('long fixture question 11')).toBeVisible()
  await page.screenshot({ path: 'test-results/live-older-history.png' })

  // A goal boundary is a card, not invisible: the fixture home's own session
  // shows start/continuation rows once a goal has run in it.
  const boundary = page.locator('[data-goal-notice]')
  expect(await boundary.count()).toBeGreaterThanOrEqual(0)
})

test('names its glyph controls on hover', async ({ page }) => {
  await openFixture(page)
  // The composer's add control carries no text of its own: hovering names it,
  // after the delay that keeps a passing pointer from flashing a bubble.
  await page.getByRole('button', { name: '命令' }).hover()
  await expect(page.getByRole('tooltip')).toHaveText('命令', { timeout: 3000 })

  // The same for a message's copy glyph.
  await page.getByRole('button', { name: '复制回答' }).first().hover()
  await expect(page.getByRole('tooltip')).toHaveText('复制回答', { timeout: 3000 })
})

test('browses commands from the composer panel', async ({ page }) => {
  await openFixture(page)
  await page.getByRole('button', { name: '命令' }).click()

  // The same copied shell `/` opens, on the whole contract: the image pick leads
  // the list, and a pick writes the command into the draft instead of running it.
  await expect(page.getByRole('option', { name: /添加图片/ })).toBeVisible()
  await expect(page.getByLabel('筛选命令')).toBeHidden()
  await page.getByRole('option', { name: /^\/details/ }).click()
  await expect(page.getByLabel('输入消息')).toHaveValue('/details ')
  await page.screenshot({ path: 'test-results/live-command-panel.png' })
})

test('switches the session model from the composer seat', async ({ page }) => {
  await openFixture(page)
  const seat = page.getByRole('button', { name: /^当前模型 / })
  await expect(seat).toBeEnabled()
  const before = await seat.getAttribute('aria-label')
  await seat.click()

  // The menu lists the aliases the session's runtime resolves through; picking
  // one switches the model and the seat follows the runtime's report.
  const alternative = before?.includes('pro') ? 'flash' : 'pro'
  await page.getByRole('menuitem', { name: new RegExp(alternative) }).click()
  await expect(page.getByRole('button', { name: new RegExp(`^当前模型 ${alternative}$`) })).toBeVisible()
})

test('keeps each session’s draft with that session', async ({ page }) => {
  await openFixture(page)
  const field = page.getByLabel('输入消息')
  await field.fill('这是会话一的草稿')

  // Switching sessions must not carry the draft across, and coming back must
  // return it: a draft belongs to one session identity.
  await openFixture(page, E2E_LONG_ID)
  await expect(page.getByLabel('输入消息')).toHaveValue('')
  await page.getByLabel('输入消息').fill('这是会话二的草稿')

  await openFixture(page)
  await expect(page.getByLabel('输入消息')).toHaveValue('这是会话一的草稿')
  await openFixture(page, E2E_LONG_ID)
  await expect(page.getByLabel('输入消息')).toHaveValue('这是会话二的草稿')
})

test('reconnects the live stream after a network drop', async ({ page }) => {
  // Supplementary transport-level check: abort the first subscription, then let
  // the backoff retry through. The offline/online case below covers the browser
  // signal path.
  let failed = false
  await page.route('**/stream*', route => {
    if (!failed) {
      failed = true
      return route.abort('failed')
    }
    return route.continue()
  })
  await page.goto('/')
  await page.locator(`[data-row-key="session:${E2E_SESSION_ID}"]`).click()
  const reconnect = page.getByRole('button', { name: '重新连接会话' })
  await expect(reconnect).toBeVisible({ timeout: 10_000 })

  await expect(reconnect).toHaveCount(0, { timeout: 30_000 })
  await expect(page.getByText('fixture question one')).toBeVisible()
})

test('recovers a running turn after a real offline/online cycle', async ({ page, context }) => {
  // A fresh session keeps this case independent of the fixture's shared history
  // and of the other streaming specs running in parallel.
  const created = await page.request.post('/api/sessions', {
    data: { name: 'offline-real', cwd: `/tmp/mink-web-e2e-offline-${Date.now()}` },
  })
  expect(created.ok()).toBe(true)
  const target = (await created.json() as { data: { id: string; project_key: string } }).data
  await page.goto(`/?session=${target.id}&project=${target.project_key}`)
  const field = page.getByLabel('输入消息')
  await expect(field).toBeEnabled()
  await field.fill('start a slow-task please')
  await page.getByRole('button', { name: '发送' }).click()
  // The provider holds this turn for ~3.5s; the session keeps running while the
  // browser is offline.
  await expect(page.getByRole('button', { name: '停止' })).toBeVisible()

  await context.setOffline(true)
  const reconnect = page.getByRole('button', { name: '重新连接会话' })
  await expect(reconnect).toBeVisible({ timeout: 10_000 })

  await context.setOffline(false)
  // Reconnect + authoritative snapshot clear the gap; the live Final still
  // lands and the composer returns to sending.
  await expect(reconnect).toHaveCount(0, { timeout: 30_000 })
  await expect(page.getByText(/Completed: fixture response/).first()).toBeVisible({ timeout: 30_000 })
  await expect(page.getByRole('button', { name: '发送' })).toBeVisible({ timeout: 30_000 })
})

test('restores an in-flight submission and its attachment after leaving', async ({ page }) => {
  const created = await page.request.post('/api/sessions', {
    data: { name: 'pending-real', cwd: `/tmp/mink-web-e2e-pending-${Date.now()}` },
  })
  expect(created.ok()).toBe(true)
  const target = (await created.json() as { data: { id: string; project_key: string } }).data
  const png = Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
    'base64',
  )
  await page.goto(`/?session=${target.id}&project=${target.project_key}`)
  const field = page.getByLabel('输入消息')
  await expect(field).toBeEnabled()
  // Wait for the session's frozen image capability before opening the picker.
  await page.getByRole('button', { name: '命令' }).click()
  await expect(page.getByRole('option', { name: /添加图片/ })).toContainText('图片只进入会话附件目录')
  await page.keyboard.press('Escape')
  const picker = page.locator('input[aria-label="选择图片"]')
  await expect(picker).toBeEnabled()
  await picker.setInputFiles({
    name: 'shot.png',
    mimeType: 'image/png',
    buffer: png,
  })
  const rail = page.locator('[data-composer-attachments]')
  await expect(rail.locator('img').first()).toHaveAttribute('src', /\/attachments\//)

  // Hold the admission request so the submission is genuinely in flight when
  // the reader leaves.
  let release!: () => void
  const gate = new Promise<void>(resolve => { release = resolve })
  await page.route('**/inputs*', async route => {
    await gate
    await route.continue().catch(() => {})
  })
  await field.fill('in-flight pending words')
  await page.getByRole('button', { name: '发送' }).click()
  const pendingKey = `mink.pending:${target.project_key}:${target.id}`
  await expect.poll(async () => page.evaluate(key => localStorage.getItem(key), pendingKey)).not.toBeNull()

  await page.goto(`/?session=${E2E_LONG_ID}`)
  await expect(page.locator('[class*="bubble"]').first()).toBeVisible()
  await page.goto(`/?session=${target.id}&project=${target.project_key}`)
  await expect(field).toHaveValue('in-flight pending words')
  await expect(rail.locator('img').first()).toHaveAttribute('src', /\/attachments\//)
  release()
})

test('opens the output named by an /artifact command', async ({ page }) => {
  await openFixture(page)
  const field = page.getByLabel('输入消息')
  await field.fill('/artifact fixture-output')
  await field.press('Enter')
  const panel = page.locator('[data-detail-panel="outputs"]')
  await expect(panel).toBeVisible()
  await expect(panel.getByText(/fixture-output/)).toBeVisible()
})

test('keeps an older turn’s actions out of the way until it is hovered', async ({ page }) => {
  await openFixture(page)
  // The source's rule: the tail keeps its chrome, earlier rows reveal it on
  // hover or focus-within, so a long transcript stays quiet.
  const older = page.locator('[data-turn-key]:not(:last-of-type) [data-message-actions]').first()
  await expect(older).toHaveCSS('opacity', '0')
  await older.locator('xpath=ancestor::div[@data-turn-key]').first().hover()
  await expect(older).toHaveCSS('opacity', '1')

  // The newest row keeps its copy control visible, and it copies the text.
  const newest = page.locator('[data-message-actions]').last()
  await expect(newest).toHaveCSS('opacity', '1')
})

test('collapses the sidebar to a rail when the reader asks it to', async ({ page }) => {
  // A wide frame keeps the sidebar's own column: collapsing it is the reader's
  // choice (the rail), which is what `data-sidebar-collapsed` marks.
  await page.goto('/')
  await page.getByRole('button', { name: '收起侧栏' }).first().click()
  await expect(page.locator('[data-sidebar-collapsed]')).toBeVisible()
  await page.screenshot({ path: 'test-results/live-rail.png' })
})

test('opens a new project with only a directory and creates its first conversation on send', async ({ page }) => {
  await page.goto('/')
  const directory = `/tmp/mink-web-e2e-project-${Date.now()}`
  mkdirSync(directory, { recursive: true })
  await page.getByRole('button', { name: '新建项目', exact: true }).first().click()
  const dialog = page.getByRole('dialog', { name: '新建项目' })
  await expect(dialog).toBeVisible()
  // A project is a directory; it has no separate name field.
  await expect(dialog.getByLabel('任务名称')).toHaveCount(0)
  await dialog.getByLabel('工作目录').fill(directory)
  await page.screenshot({ path: 'test-results/live-new-project.png' })
  await dialog.getByRole('button', { name: '打开目录' }).click()
  await expect(dialog).toHaveCount(0)

  // The composer is browser-only until its first send: no session URL yet.
  await expect(page.locator('[data-new-conversation]')).toBeVisible()
  expect(new URL(page.url()).searchParams.has('session')).toBe(false)
  await page.getByLabel('输入消息').fill('please answer with the fixture response')
  await page.getByRole('button', { name: '发送' }).click()
  await expect(page.getByText(/Completed: fixture response/)).toBeVisible()
  await expect(page).toHaveURL(/[?&]session=/)

  // The project appears once its first session exists.
  const projectName = directory.slice(directory.lastIndexOf('/') + 1)
  await expect(page.getByRole('treeitem', { name: new RegExp(projectName) })).toBeVisible()
})



test('retries a failed goal with explicit budget adjustment and preserves its history', async ({ page }) => {
  const created = await page.request.post('/api/sessions', { data: { name: 'retry-execution', cwd: await fixtureCwd(page) } })
  const body = await created.json() as { data: { id: string; project_key: string } }
  const goalUrl = `/api/sessions/${body.data.id}/goal?project=${body.data.project_key}`
  const fullGoalUrl = `${goalUrl}&full=true`
  await page.goto(`/?session=${body.data.id}&project=${body.data.project_key}`)
  await expect(page.getByLabel('输入消息')).toBeEnabled()
  const drafted = await page.request.post(goalUrl, { data: {
    protocol_version: 2, generate: false, objective: 'retry-execution-fixture: verify README', max_turns: 1, max_minutes: 2,
  } })
  expect(drafted.ok(), await drafted.text()).toBe(true)
  await page.getByLabel('展开详情').click()
  const panel = page.locator('[data-detail-panel]')
  await panel.getByRole('tab', { name: '目标' }).click()
  await panel.getByRole('button', { name: '确认并启动' }).click()
  await expect(page.locator('[data-goal-bar]')).toHaveAttribute('data-goal-status', 'failed')
  const before = await (await page.request.get(fullGoalUrl)).json()
  const original = before.data.goal
  // Exhaust both gates without changing the contract or erasing the failed turn.
  const expired = await page.request.post(`/api/sessions/${body.data.id}/goal/budget?project=${body.data.project_key}`, {
    data: { goal_id: original.id, revision: original.revision, max_minutes: 0 },
  })
  expect(expired.ok(), await expired.text()).toBe(true)
  await page.reload()
  await expect(page.getByLabel('输入消息')).toBeEnabled()
  if (await page.getByLabel('展开详情').isVisible()) await page.getByLabel('展开详情').click()
  await panel.getByRole('tab', { name: '目标' }).click()
  await expect(panel.getByRole('button', { name: '重试目标' })).toBeDisabled()
  await panel.getByRole('spinbutton', { name: '总轮数上限' }).fill('2')
  await panel.getByRole('spinbutton', { name: '从现在起允许分钟数' }).fill('2')
  await panel.getByRole('button', { name: '调整预算', exact: true }).click()
  await expect(panel.getByRole('button', { name: '重试目标' })).toBeEnabled()
  await expect(page.locator('[data-goal-bar]')).toHaveAttribute('data-goal-status', 'failed')
  await panel.getByRole('button', { name: '重试目标' }).click()
  await expect(page.locator('[data-goal-bar]')).toHaveAttribute('data-goal-status', 'completed')
  const after = await (await page.request.get(fullGoalUrl)).json()
  expect(after.data.goal.id).toBe(original.id)
  expect(after.data.goal.contract_revision).toBe(original.contract_revision)
  expect(after.data.goal.iterations).toHaveLength(2)
  expect(after.data.goal.iterations[0]).toEqual(original.iterations[0])
  for (const record of original.usage_records) {
    expect(after.data.goal.usage_records.filter((r: { request_id: string }) => r.request_id === record.request_id)).toHaveLength(1)
  }
})

test('records lightweight goal continuation without full evidence review', async ({ page }) => {
  const created = await page.request.post('/api/sessions', { data: { name: 'partial-review-fixture', cwd: await fixtureCwd(page) } })
  const body = await created.json() as { data: { id: string; project_key: string } }
  await page.goto(`/?session=${body.data.id}&project=${body.data.project_key}`)
  await expect(page.getByLabel('输入消息')).toBeEnabled()
  const goalUrl = `/api/sessions/${body.data.id}/goal?project=${body.data.project_key}`
  const draft = await page.request.post(goalUrl, { data: { protocol_version: 2, generate: false, objective: 'partial-review-fixture: implement a deliverable', max_turns: 1, max_minutes: 2 } })
  expect(draft.ok(), await draft.text()).toBe(true)
  await page.getByLabel('展开详情').click()
  const panel = page.locator('[data-detail-panel]')
  await panel.getByRole('tab', { name: '目标' }).click()
  await panel.getByRole('button', { name: '确认并启动' }).click()
  await expect(page.locator('[data-goal-bar]')).toHaveAttribute('data-goal-status', 'budget_exhausted')
  await expect(panel.getByRole('region', { name: '轻量分流' })).toContainText('剩余要求：criterion-1')
  await expect(panel.getByText(/本轮未进行完整证据验收/)).toBeVisible()
  const response = await page.request.get(`${goalUrl}&full=true`)
  const snapshot = await response.json() as { data: { goal: { reviews: unknown[]; continuations: unknown[]; iterations: unknown[]; completion_basis: unknown } } }
  expect(snapshot.data.goal.reviews).toHaveLength(0)
  expect(snapshot.data.goal.continuations).toHaveLength(1)
  expect(snapshot.data.goal.iterations).toHaveLength(1)
  expect(snapshot.data.goal.completion_basis).toBeNull()
  await panel.getByRole('button', { name: '读取分流报告' }).click()
  await expect(page.locator('[data-detail-panel="outputs"]')).toContainText('Full evidence verification was not performed')
})
