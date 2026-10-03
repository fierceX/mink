import { test, expect } from '@playwright/test';
import { openSettings } from './settings-helper';

for (const width of [1440, 390]) {
  test(`settings are on demand and restore focus at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 844 });
    await page.goto('/?session=e2e-session');
    await expect(page.getByRole('textbox', { name: '任务或补充指令' })).toBeVisible();
    await expect(page.locator('.sessions-sidebar footer')).toHaveCount(0);
    await expect(page.getByRole('dialog', { name: '设置', exact: true })).not.toBeVisible();
    await openSettings(page);
    const settings = page.getByRole('dialog', { name: '设置', exact: true });
    await expect(settings).toBeVisible();
    await expect(page.getByRole('button', { name: '关闭设置', exact: true })).toBeFocused();
    await page.keyboard.press('Shift+Tab');
    expect(await settings.evaluate(el => el.contains(document.activeElement))).toBe(true);
    await page.getByRole('combobox', { name: '外观', exact: true }).selectOption('dark');
    await page.getByRole('combobox', { name: '过程展示', exact: true }).selectOption('detailed');
    await page.screenshot({ animations: 'disabled', path: `/private/tmp/mink-settings-${width}.png` });
    expect(await page.evaluate(() => document.scrollingElement!.scrollWidth <= innerWidth)).toBe(true);
    await page.keyboard.press('Escape');
    await expect(settings).not.toBeVisible();
    await expect(page.getByRole('button', { name: '会话操作菜单', exact: true })).toBeFocused();
    await page.reload();
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
    await openSettings(page);
    await expect(page.getByRole('combobox', { name: '过程展示', exact: true })).toHaveValue('detailed');
  });
}

test('root address stays on Home despite legacy saved selection; session reload does not flash Home', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('mink.currentSession', 'e2e-session'));
  await page.goto('/');
  await expect(page.getByRole('heading', { name: '继续工作' })).toBeVisible();
  await expect(page.locator('.session-page')).toHaveCount(0);
  await expect(page.getByRole('button', { name: '首页', exact: true })).toHaveAttribute('aria-current', 'page');
  await page.reload();
  await expect(page.getByRole('heading', { name: '继续工作' })).toBeVisible();
  let release!: () => void;
  const held = new Promise<void>(resolve => release = resolve);
  await page.route('**/api/sessions', async route => { await held; await route.continue(); });
  await page.goto('/?session=e2e-session');
  await expect(page.getByText('正在打开会话…', { exact: true })).toBeVisible();
  await expect(page.getByRole('heading', { name: '继续工作' })).toHaveCount(0);
  release();
  await expect(page.locator('.msg.agent').last()).toContainText('fixture answer 44');
  expect(new URL(page.url()).searchParams.get('project')).toBeTruthy();
});

test('explicit Home during startup wins over the delayed initial session', async ({ page }) => {
  let release!: () => void;
  const held = new Promise<void>(resolve => release = resolve);
  await page.route('**/api/sessions', async route => { await held; await route.continue(); });
  await page.goto('/?session=e2e-session');
  await expect(page.getByText('正在打开会话…', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: '首页', exact: true }).click();
  await expect(page.getByRole('heading', { name: '继续工作' })).toBeVisible();
  const catalog = page.waitForResponse(response => new URL(response.url()).pathname === '/api/sessions');
  release(); await catalog;
  await expect(page.locator('.sess-row')).not.toHaveCount(0);
  await expect(page.locator('.session-page')).toHaveCount(0);
  expect(new URL(page.url()).searchParams.has('session')).toBe(false);
});

test('missing session links return to Home with an explicit explanation', async ({ page }) => {
  await page.goto('/?session=missing-session');
  await expect(page.getByRole('heading', { name: '继续工作' })).toBeVisible();
  await expect(page.locator('.app-toast')).toHaveText('会话不存在，已返回首页');
  expect(new URL(page.url()).searchParams.has('session')).toBe(false);
});

test('mobile Home leaves a running task in the background and retains the draft', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  const session = (await (await page.request.post('/api/sessions', { data: { name: `home-${Date.now()}`, cwd: '/tmp/mink-e2e-cwd' } })).json()).data;
  try {
    await page.goto(`/?session=${session.id}&project=${session.project_key}`);
    const input = page.getByRole('textbox', { name: '任务或补充指令' });
    await expect(input).toBeVisible(); await input.fill('slow-task');
    await page.getByRole('button', { name: '发送', exact: true }).click();
    await expect(page.getByRole('button', { name: '提交引导', exact: true })).toBeVisible();
    await input.fill('保留首页切换草稿');
    await page.getByRole('button', { name: '切换导航', exact: true }).click();
    await page.getByRole('button', { name: '首页', exact: true }).click();
    await expect(page.getByRole('heading', { name: '继续工作' })).toBeVisible();
    expect((await page.locator('.hero').boundingBox())!.y).toBeGreaterThan(48);
    expect(await page.evaluate(() => document.scrollingElement!.scrollWidth <= innerWidth)).toBe(true);
    await page.screenshot({ animations: 'disabled', path: '/private/tmp/mink-home-390.png' });
    expect(new URL(page.url()).searchParams.has('session')).toBe(false);
    const runtime = (await (await page.request.get(`/api/sessions/${session.id}?project=${session.project_key}`)).json()).data;
    expect(runtime.running).toBe(true);
    await page.reload(); await expect(page.getByRole('heading', { name: '继续工作' })).toBeVisible();
    await page.getByRole('button', { name: '切换导航', exact: true }).click();
    await page.locator(`.sess-row[data-id="${session.id}"] .session-select`).click();
    await expect(input).toHaveValue('保留首页切换草稿');
    expect(new URL(page.url()).searchParams.get('session')).toBe(session.id);
    await page.getByRole('button', { name: '停止', exact: true }).click();
    await expect(page.getByRole('button', { name: '发送', exact: true })).toBeVisible();
  } finally { await page.request.delete(`/api/sessions/${session.id}?project=${session.project_key}`); }
});

for (const width of [1440, 1024, 768, 390, 320]) {
  test(`catalog scope isolates names and paths at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 844 }); await page.goto('/');
    if (width < 1024) await page.getByRole('button', { name: '切换导航', exact: true }).click();
    const catalog = page.locator('.session-list');
    await expect(catalog.locator('.sess-row')).not.toHaveCount(0);
    const count = await catalog.locator('.sess-row').count();
    const scope = page.getByRole('combobox', { name: '筛选范围', exact: true });
    const query = page.getByRole('textbox', { name: '搜索会话', exact: true });
    await scope.selectOption('project'); await query.fill('mink-e2e-cwd');
    await expect(catalog.locator('.sess-row')).toHaveCount(count);
    await scope.selectOption('session');
    await expect(query).toHaveValue('mink-e2e-cwd');
    await expect(catalog).toContainText('没有匹配的会话');
    await query.fill('e2e-cards');
    await expect(catalog.locator('.sess-row')).toHaveCount(1);
    await expect(catalog).toContainText('工具卡片验证');
    await scope.selectOption('path');
    await expect(catalog.locator('.sess-row')).toHaveCount(0);
    await query.fill('/tmp/mink-e2e-cwd');
    await expect(catalog.locator('.sess-row')).toHaveCount(count);
    await scope.selectOption('project');
    await expect(catalog.locator('.sess-row')).toHaveCount(0);
    await page.getByRole('button', { name: '清空筛选', exact: true }).click();
    await expect(query).toHaveValue('');
    await expect(query).toBeFocused();
    await expect(catalog.locator('.sess-row')).toHaveCount(count);
    await expect(page.getByRole('button', { name: '首页', exact: true })).toBeVisible();
    await scope.selectOption('session'); await query.fill('工具卡片');
    await page.screenshot({ animations: 'disabled', path: `/private/tmp/mink-filter-${width}.png` });
    expect(await page.locator('.sessions-sidebar').evaluate(el => el.scrollWidth <= el.clientWidth)).toBe(true);
    expect(await page.evaluate(() => document.scrollingElement!.scrollWidth <= innerWidth)).toBe(true);
    await page.locator('.sess-row .session-select').click();
    await expect(page.getByRole('textbox', { name: '任务或补充指令' })).toBeVisible();
    expect(new URL(page.url()).searchParams.get('session')).toBe('e2e-cards');
  });
}
