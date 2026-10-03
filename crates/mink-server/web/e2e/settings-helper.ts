import type { Page } from '@playwright/test';
export async function openSettings(page: Page) {
  await page.getByRole('button', { name: /^(会话操作菜单|工作台菜单)$/ }).click();
  await page.getByRole('menuitem', { name: '设置', exact: true }).click();
}
