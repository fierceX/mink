import { expect, test } from '@playwright/test'

// Visual guard for the copied design layer. The design-review page renders every
// tool-card family from synthetic items, so a baseline holds no session data and
// a pixel regression in the copied sheets fails here instead of shipping.
//
// The baseline is per platform (`-darwin` today): font rasterization differs
// across systems, so a platform without a committed baseline skips rather than
// failing. Add the platform's baseline when that runner is first used.

/** Runtime the committed baseline belongs to. */
const BASELINE_PLATFORM = 'darwin'

test('the design page matches its recorded baseline', async ({ page }, testInfo) => {
  test.skip(process.platform !== BASELINE_PLATFORM, `no visual baseline for ${process.platform}`)

  await page.goto('/fixtures.html')
  await expect(page.locator('[class*="bubble"]')).toBeVisible()
  // Open the process group and every row, so the page shows the families rather
  // than the collapsed default.
  await page.getByRole('button', { name: /^执行过程/ }).first().click()
  const tools = page.locator('[data-tool]')
  for (let index = 0; index < await tools.count(); index += 1) await tools.nth(index).click()
  // Shiki highlighting and the font faces settle asynchronously.
  await page.waitForTimeout(1200)

  await expect(page).toHaveScreenshot('design-page.png', {
    fullPage: true,
    // Antialiasing differs by a pixel here and there; the guard is for design
    // drift, not for the rasterizer.
    maxDiffPixelRatio: 0.002,
  })
  testInfo.annotations.push({ type: 'baseline', description: BASELINE_PLATFORM })
})

test('the chat surface matches its recorded baseline', async ({ page }) => {
  test.skip(process.platform !== BASELINE_PLATFORM, `no visual baseline for ${process.platform}`)

  // The composition (sidebar, header, transcript, goal bar, composer, pills) is
  // rendered from synthetic state, so this baseline carries no session data.
  await page.goto('/workbench.html')
  await expect(page.getByLabel('输入消息')).toBeVisible()
  await expect(page.locator('[data-goal-bar]')).toBeVisible()
  await page.waitForTimeout(1200)

  await expect(page).toHaveScreenshot('chat-surface.png', {
    maxDiffPixelRatio: 0.002,
  })
})

test('the empty state matches its recorded baseline', async ({ page }) => {
  test.skip(process.platform !== BASELINE_PLATFORM, `no visual baseline for ${process.platform}`)

  await page.goto('/workbench.html?state=empty')
  await expect(page.locator('[data-empty-state]')).toBeVisible()
  await page.waitForTimeout(1200)
  await expect(page).toHaveScreenshot('empty-state.png', {
    maxDiffPixelRatio: 0.002,
  })
})

test('the chat surface keeps its dark palette', async ({ page }) => {
  test.skip(process.platform !== BASELINE_PLATFORM, `no visual baseline for ${process.platform}`)

  await page.goto('/workbench.html')
  // The preference mechanism the settings dialog writes.
  await page.evaluate(() => { document.body.setAttribute('data-ds-dark-theme', '') })
  await expect(page.getByLabel('输入消息')).toBeVisible()
  await page.waitForTimeout(1200)
  await expect(page).toHaveScreenshot('chat-surface-dark.png', {
    maxDiffPixelRatio: 0.002,
  })
})

test('the chat surface holds together on a phone viewport', async ({ page }) => {
  test.skip(process.platform !== BASELINE_PLATFORM, `no visual baseline for ${process.platform}`)

  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/workbench.html')
  await expect(page.getByLabel('输入消息')).toBeVisible()
  await page.waitForTimeout(1200)
  await expect(page).toHaveScreenshot('chat-surface-narrow.png', {
    maxDiffPixelRatio: 0.002,
  })
})
