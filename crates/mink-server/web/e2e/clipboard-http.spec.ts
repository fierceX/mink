import { test, expect } from '@playwright/test';
import { E2E_SESSION_ID } from './global-setup';

test('copying succeeds on an insecure HTTP hostname without Clipboard API',async({page,context})=>{
  await context.grantPermissions(['clipboard-read','clipboard-write']);
  // Route the insecure hostname to the local test server without system DNS/proxy dependencies.
  await context.route('http://mink-http.test:18821/**',route=>route.continue({url:route.request().url().replace('mink-http.test','127.0.0.1')}));
  await page.goto(`http://mink-http.test:18821/?session=${E2E_SESSION_ID}`);
  await expect(page.getByRole('textbox',{name:'任务或补充指令'})).toBeVisible();
  expect(await page.evaluate(()=>({secure:isSecureContext,clipboard:!!navigator.clipboard}))).toEqual({secure:false,clipboard:false});
  const message=page.locator('.msg.user').last();const text=await message.locator('.bubble').textContent();
  await message.getByRole('button',{name:'复制',exact:true}).click();await expect(message.getByRole('button',{name:'已复制',exact:true})).toBeVisible();
  const reader=await context.newPage();await reader.goto('/');expect(await reader.evaluate(()=>navigator.clipboard.readText())).toBe(text);
});
