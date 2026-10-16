import { expect, test } from '@playwright/test'

// Design-review lane: the fixture page needs no server, so the card families are
// checked deterministically. `npm run dev` serves it; the production build does
// not include it.

test('renders every tool-card family with the copied cards', async ({ page }) => {
  await page.goto('/fixtures.html')
  await expect(page.locator('[class*="bubble"]')).toBeVisible()

  // The process group holds the tool rows; open it, then every row.
  await page.getByRole('button', { name: /^执行过程/ }).first().click()
  const tools = page.locator('[data-tool]')
  const count = await tools.count()
  expect(count).toBeGreaterThanOrEqual(5)
  for (let index = 0; index < count; index += 1) await tools.nth(index).click()

  await expect(page.locator('[data-terminal]').first()).toBeVisible()
  await expect(page.locator('[data-read]').first()).toBeVisible()
  await expect(page.locator('[data-diff]').first()).toBeVisible()
  await expect(page.locator('[data-search="matches"]').first()).toBeVisible()
  await expect(page.locator('[data-search="paths"]').first()).toBeVisible()

  // The changed-files card closes the turn that edited: header counts, one row
  // per file, and a fold control once past four rows.
  const changes = page.locator('[data-changed-files]')
  await expect(changes).toBeVisible()
  await expect(changes).toContainText('已编辑 5 个文件')
  await expect(changes.locator('[class*="row"]')).toHaveCount(4)
  await changes.getByRole('button', { name: /展开全部 5 个文件/ }).click()
  await expect(changes.locator('[class*="row"]')).toHaveCount(5)
  await changes.getByRole('button', { name: /收起文件列表/ }).click()
  await expect(changes.locator('[class*="row"]')).toHaveCount(4)
  // A failed command settles its row red instead of showing a clean card.
  await expect(page.locator('[data-tool="Bash"][data-state="error"]')).toBeVisible()
  await page.screenshot({ path: 'test-results/fixture-cards.png', fullPage: true })
})

test('reads a delegation as one row per task', async ({ page }) => {
  await page.goto('/fixtures.html')
  // Open every process group: the delegation sits in the last turn's group, which
  // starts collapsed like the first.
  const groups = page.getByRole('button', { name: /^执行过程/ })
  for (let index = 0; index < await groups.count(); index += 1) await groups.nth(index).click()
  // The task list is shown as rows, not as the argument JSON a generic card
  // would print.
  const row = page.locator('[data-tool="SubAgent"]').first()
  await row.click()
  const card = row.locator('[data-sub-agent-card]')
  await expect(card).toBeVisible()
  await expect(card.locator('[class*="delegationDescription"]')).toHaveCount(2)
  await expect(card).toContainText('核对工具卡家族')
})

test('reads a todo answer and a plan as their own cards', async ({ page }) => {
  await page.goto('/fixtures.html')
  const groups = page.getByRole('button', { name: /^执行过程/ })
  for (let index = 0; index < await groups.count(); index += 1) await groups.nth(index).click()

  // The todo tool's answer is the list, and the plan tool's body is the plan.
  const todo = page.locator('[data-tool="TodoRead"]').first()
  await todo.click()
  const todoCard = todo.locator('[data-todo-card]')
  await expect(todoCard).toBeVisible()
  // The formatted answer is read as blocks: revision and counts, then the rows.
  await expect(todoCard).toContainText('revision 2')
  await expect(todoCard).toContainText('进行中 1')
  await expect(todo.locator('[class*="todoContent"]').first()).toContainText('重建外壳')

  const plan = page.locator('[data-tool="PlanDraft"]').first()
  await plan.click()
  await expect(plan.locator('[data-plan-card]')).toBeVisible()
  // The plan body is rendered as markdown from the presentation, with the
  // transition named above it and the raw result behind a disclosure.
  await expect(plan.locator('[data-plan-card]')).toContainText('计划草稿已保存')
  await expect(plan.locator('[data-plan-card]')).toContainText('对齐卡片')

  // A goal report shows what a reader checks: kind, summary, next step, evidence.
  const report = page.locator('[data-tool="GoalReport"]').first()
  await report.click()
  const card = report.locator('[data-goal-report-card]')
  await expect(card).toBeVisible()
  await expect(card).toContainText('报告完成')
  await expect(card).toContainText('视觉基线覆盖浅色/暗色/窄屏')
})

test('offers the file and the artifact a tool card names', async ({ page }) => {
  await page.goto('/fixtures.html')
  const groups = page.getByRole('button', { name: /^执行过程/ })
  for (let index = 0; index < await groups.count(); index += 1) await groups.nth(index).click()

  // The legacy card carried both entry points inline; so does this one.
  const read = page.locator('[data-tool="Read"]').first()
  await read.click()
  await expect(read.getByRole('button', { name: '查看当前文件' })).toBeVisible()

  const todo = page.locator('[data-tool="TodoRead"]').first()
  await todo.click()
  await expect(todo.getByRole('button', { name: /artifact:\/\/fixture-todo-read/ })).toBeVisible()
})

test('answers a changed-file row with its hunks', async ({ page }) => {
  await page.goto('/fixtures.html')
  // The row's hover preview is drawn from the hunks the turn recorded, so the
  // card and the preview can never disagree about what changed.
  const row = page.getByRole('button', { name: /^查看 .* 的改动$/ }).last()
  await row.hover()
  const preview = page.locator('[data-changed-file-preview]')
  await expect(preview).toBeVisible({ timeout: 3000 })
  await expect(preview.locator('[class*="path"]')).toContainText(/\S/)
})
