import { openSettings } from './settings-helper';
import { test, expect } from '@playwright/test';
import { E2E_CARDS_ID } from './tool-cards-fixture';

for(const width of [1440,390]) {
  test(`tool cards render formal structured history after reload at ${width}px`,async({page})=>{
    const errors:string[]=[];page.on('pageerror',error=>errors.push(error.message));
    await page.setViewportSize({width,height:844});await page.goto(`/?session=${E2E_CARDS_ID}`);
    await expect(page.locator('.msg.agent').last()).toContainText('已完成配置检查与验证');
    await page.reload();await expect(page.locator('.tool-card')).toHaveCount(8);
    for(const group of await page.locator('.process-group').all())await group.locator(':scope > summary').click();
    const card=(name:string)=>page.locator('.tool-card').filter({has:page.locator('.t-name').filter({hasText:new RegExp(`^${name}$`)})});
    for(const name of ['Edit','PlanDraft','TodoRead','PythonSandbox','SubAgent','Bash','Grep','Write'])await card(name).locator(':scope > summary').click();
    await expect(card('Edit').locator('.e-before').first()).toHaveText('timeout = 10');
    await expect(card('Edit').locator('.e-after').first()).toHaveText('timeout = 30');await expect(card('Edit').locator('.e-operation').last()).toContainText('全部匹配');
    await expect(card('Edit').locator('.e-original pre')).not.toBeVisible();
    await expect(card('PlanDraft').locator('.plan-body h1')).toHaveText('验证计划');await expect(card('PlanDraft').locator('.plan-body strong')).toHaveText('运行测试');
    await expect(card('TodoRead').locator('.t-task')).toHaveCount(3);await expect(card('TodoRead').locator('.t-head-block')).toContainText('已完成 1');
    await expect(card('PythonSandbox').locator('.input-content pre')).toHaveText('print("checks passed")');await expect(card('PythonSandbox').locator('.command-output')).toHaveText('checks passed');
    await expect(card('SubAgent').locator('.t-result strong')).toHaveText('接口兼容');
    await expect(card('Bash').locator('.input-content pre')).toHaveText('cargo test -p fixture');await expect(card('Bash').locator('.t-artifact')).toHaveText('artifact://fixture-output');
    await expect(card('Grep').locator('.parameter-fields')).toContainText('timeout');await expect(card('Write').locator('.input-content pre')).toHaveText('timeout = 30');
    await expect(card('Bash').locator('.raw-input pre')).not.toBeVisible();await card('Bash').locator('.raw-input summary').click();await expect(card('Bash').locator('.raw-input pre')).toContainText('"command": "cargo test -p fixture"');
    expect(await page.evaluate(()=>document.scrollingElement!.scrollWidth<=innerWidth)).toBe(true);
    for(const name of ['Edit','PlanDraft','TodoRead','SubAgent']) {
      await card(name).scrollIntoViewIfNeeded();await card(name).screenshot({animations:'disabled',path:`/private/tmp/mink-card-${name}-${width}.png`});
    }
    await page.screenshot({animations:'disabled',path:`/private/tmp/mink-cards-${width}.png`});
    await openSettings(page); await page.getByRole('combobox',{name:'外观',exact:true}).selectOption('dark'); await page.getByRole('button',{name:'关闭设置',exact:true}).click();
    await expect(page.locator('html')).toHaveAttribute('data-theme','dark');
    await card('PythonSandbox').scrollIntoViewIfNeeded();await card('PythonSandbox').screenshot({animations:'disabled',path:`/private/tmp/mink-card-command-dark-${width}.png`});
    expect(await page.evaluate(()=>document.scrollingElement!.scrollWidth<=innerWidth)).toBe(true);expect(errors).toEqual([]);
  });
}
