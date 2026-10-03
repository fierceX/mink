import { test, expect, type Page } from '@playwright/test';
import { E2E_SESSION_ID } from './global-setup';

async function open(page: Page, id = E2E_SESSION_ID) {
  await page.goto(`/?session=${id}`);
  await expect(page.getByRole('textbox', { name: '任务或补充指令' })).toBeVisible();
  await expect(page.getByRole('status').filter({ hasText: '正在恢复连接' })).toHaveCount(0);
}
async function create(page: Page) {
  const response = await page.request.post('/api/sessions', { data: { name: `browser-${Date.now()}-${Math.random()}`, cwd: '/tmp/mink-e2e-cwd' } });
  const data = (await response.json()).data as { id: string; project_key: string };
  await open(page, data.id); return data;
}
test('desktop project navigation, final replies and ordered process groups', async ({ page }) => {
  const errors: string[] = []; page.on('pageerror', error => errors.push(error.message));
  await open(page);
  await expect(page.locator('.navigation')).toBeVisible();
  await expect(page.locator('.turn')).toHaveCount(20);
  const first = page.locator('.turn').first();
  await expect(first.locator('.msg.user')).toContainText('fixture question 25');
  await expect(first.locator('.msg.agent')).toContainText('fixture answer 25');
  const group = first.locator('.process-group').first();
  await expect(group).not.toHaveAttribute('open');
  await group.locator('summary').first().click();
  await expect(group).toHaveAttribute('open');
  await expect(first.locator('.tool-card')).toHaveCount(1);
  await expect(first.locator('.tool-card')).toContainText('状态未记录');
  await expect(first.locator('.tool-card')).not.toHaveAttribute('open');
  for (const other of await first.locator('.process-group').all()) if (await other.getAttribute('open') === null) await other.locator('summary').first().click();
  await first.locator('.tool-card > summary').click();
  await expect(first.locator('.t-file')).toContainText('e2e project');
  await page.locator('.brand').click();
  await expect(page.locator('.empty')).toBeVisible();
  await open(page);
  await expect(page.locator('.process-group').first()).toHaveAttribute('open');
  await page.getByRole('button',{name:'本轮用量',exact:true}).first().click();
  await expect(page.locator('.turn-usage')).toContainText('本轮用量未记录');
  await page.getByRole('button',{name:'关闭详情'}).click();
  expect(errors).toEqual([]);
});
for (const width of [1440, 1024, 768, 390]) {
  test(`responsive workbench ${width}px: no body overflow, reachable composer and details`, async ({ page }) => {
    await page.setViewportSize({ width, height: 844 }); await open(page);
    const textarea = page.getByRole('textbox', { name: '任务或补充指令' });
    const geometry = await textarea.boundingBox(); expect(geometry!.y + geometry!.height).toBeLessThan(844);
    expect(await page.evaluate(() => document.scrollingElement!.scrollWidth <= innerWidth && document.scrollingElement!.scrollHeight <= innerHeight)).toBe(true);
    await page.getByRole('button', { name: '详情', exact: true }).click();
    await expect(page.locator('.detail-panel')).toBeVisible();
    await page.getByRole('button', { name: '文件', exact: true }).click();
    await page.locator('.file-tree').getByRole('button', { name: '· README.md', exact: true }).click();
    await expect(page.locator('.detail-body h1')).toHaveText('e2e project');
    await page.getByRole('button', { name: '关闭详情' }).click();
    await textarea.fill('draft remains editable');
    if (width < 768) { await textarea.press('Enter'); await expect(textarea).toHaveValue('draft remains editable\n'); }
    await textarea.fill('');
    await page.screenshot({path:`/private/tmp/mink-workbench-${width}.png`});
  });
}
test('running guidance, navigation, snapshot reconnect and stable user handoff', async ({ page }) => {
  const session = await create(page);
  const textarea = page.getByRole('textbox', { name: '任务或补充指令' });
  await textarea.fill('slow-task'); await page.getByRole('button', { name: '发送', exact: true }).click();
  await expect(page.getByRole('button', { name: '提交引导', exact: true })).toBeVisible();
  await expect(page.locator('.process-group > summary').last()).toContainText('正在思考');
  const liveProcess=page.locator('.process-group').last();
  await liveProcess.locator(':scope > summary').click(); await liveProcess.locator(':scope > summary').click();
  await liveProcess.locator('.thinking-panel > summary').click();
  await textarea.fill('guidance keep public API'); await page.getByRole('button', { name: '提交引导', exact: true }).click();
  await expect(page.locator('.pending-input')).toContainText('已接收');
  await textarea.fill('next draft');
  await page.locator('.brand').click(); await expect(page.locator('.empty')).toBeVisible();
  await open(page, session.id);
  await expect(page.getByRole('textbox', { name: '任务或补充指令' })).toHaveValue('next draft');
  await expect(page.locator('.guidance')).toContainText('guidance keep public API');
  await expect(page.locator('.msg.agent').last()).toContainText('guidance applied');
  await expect(page.locator('.pending-input')).toHaveCount(0);
  await expect(page.locator('.process-group').first()).toHaveAttribute('open');
  await expect(page.locator('.thinking-panel').first()).toHaveAttribute('open');
  await page.reload(); await expect(page.locator('.guidance')).toHaveCount(1);
  const rows = (await (await page.request.get(`/api/sessions/${session.id}/conversation?project=${session.project_key}`)).json()).data;
  expect(rows.filter((row: any) => row._mink?.guidance)).toHaveLength(1);
  await page.request.delete(`/api/sessions/${session.id}?project=${session.project_key}`);
});
test('stop preserves unapplied guidance; only explicit resume starts another turn', async ({ page }) => {
  const session = await create(page); const textarea=page.getByRole('textbox', { name: '任务或补充指令' });
  await textarea.fill('slow-task'); await page.getByRole('button', { name: '发送', exact: true }).click();
  await expect(page.getByRole('button', { name: '提交引导', exact: true })).toBeVisible();
  await expect(page.locator('.process-group > summary').last()).toContainText('正在思考');
  await textarea.fill('guidance after stop'); await page.getByRole('button', { name: '提交引导', exact: true }).click();
  await expect(page.locator('.pending-input')).toContainText('guidance after stop');
  await page.getByRole('button', { name: '停止', exact: true }).click();
  await expect(page.locator('.pending-input')).toContainText('本轮已结束，尚未应用');
  await page.reload(); await expect(page.locator('.pending-input')).toContainText('本轮已结束，尚未应用');
  await page.getByRole('button', { name: '用于下一轮' }).click();
  await expect(page.locator('.pending-input')).toHaveCount(0); await expect(page.locator('.msg.agent').last()).toContainText('guidance applied');
  await page.request.delete(`/api/sessions/${session.id}?project=${session.project_key}`);
});
test('image-only upload enters model context through a real Read; history thumbnail survives refresh', async ({ page }) => {
  const session=await create(page);
  const png=Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGP4DwQACfsD/fteaysAAAAASUVORK5CYII=', 'base64');
  await page.locator('input[type=file]').setInputFiles({name:'pixel.png',mimeType:'image/png',buffer:png});
  await expect(page.locator('.upload')).toContainText('已上传');
  await page.getByRole('button', {name:'发送',exact:true}).click();
  await expect(page.locator('.msg.agent').last()).toContainText('fixture response');
  const rows=(await (await page.request.get(`/api/sessions/${session.id}/conversation?project=${session.project_key}`)).json()).data;
  expect(rows.some((row:any)=>Array.isArray(row.content)&&row.content.some((block:any)=>block.type==='tool_attachment'))).toBe(true);
  await expect(page.locator('.history-images img')).toHaveCount(1);
  await page.reload(); await expect(page.locator('.history-images img')).toHaveCount(1);
  await expect.poll(()=>page.locator('.history-images img').evaluate((img:HTMLImageElement)=>img.naturalWidth)).toBe(1);
  await page.request.delete(`/api/sessions/${session.id}?project=${session.project_key}`);
});
test('reading intent stays independent of inner scroll and detail views', async ({page})=>{
  await open(page); const transcript=page.locator('.transcript');
  await transcript.hover(); await page.mouse.wheel(0,-600); await expect(page.getByRole('button',{name:'↓ 返回最新内容'})).toBeVisible();
  const before=await transcript.evaluate(el=>el.scrollTop);
  await page.getByRole('button',{name:'详情',exact:true}).click(); await page.getByRole('button',{name:'关闭详情'}).click();
  expect(Math.abs(await transcript.evaluate(el=>el.scrollTop)-before)).toBeLessThan(5);
  await page.getByRole('button',{name:'↓ 返回最新内容'}).click(); await expect.poll(()=>transcript.evaluate(el=>el.scrollHeight-el.scrollTop-el.clientHeight)).toBeLessThan(5);
});
test('theme and task creation use accessible in-app controls',async({page})=>{
  await open(page); await page.locator('footer select').first().selectOption('dark'); await expect(page.locator('html')).toHaveAttribute('data-theme','dark');
  await page.getByRole('button',{name:'＋ 新建任务'}).first().click(); await expect(page.getByRole('dialog')).toBeVisible();
  await page.getByLabel('工作目录').fill('/tmp/mink-e2e-cwd'); await page.getByLabel('任务名称（可选）').fill('same name'); await page.getByRole('button',{name:'创建',exact:true}).click(); await expect(page.getByRole('dialog')).not.toBeVisible();
  await expect(page.getByRole('textbox',{name:'任务或补充指令'})).toBeVisible();
});

test('whole-turn pagination preserves the visible message anchor',async({page})=>{
  await open(page); const scroll=page.locator('.transcript');
  await scroll.evaluate(el=>{el.scrollTop=0}); await scroll.hover(); await page.mouse.wheel(0,-1);
  const before=await page.locator('.msg.user').first().boundingBox();
  await page.getByRole('button',{name:'加载更早的轮次',exact:true}).click();
  await expect(page.locator('.turn')).toHaveCount(40);
  const old=page.locator('.msg.user').filter({hasText:'fixture question 25'});
  await expect.poll(async()=>Math.abs((await old.boundingBox())!.y-before!.y)).toBeLessThan(5);
});

test('release runtime and reopen keeps the session draft with a fresh generation',async({page})=>{
  const session=await create(page);const input=page.getByRole('textbox',{name:'任务或补充指令'}); await input.fill('retained draft');
  const before=(await(await page.request.get(`/api/sessions/${session.id}?project=${session.project_key}`)).json()).data.generation;
  await page.getByLabel('会话操作菜单').click();await page.getByRole('button',{name:'释放运行时（停止当前任务）'}).click();
  await expect(page.getByRole('button',{name:'重新打开',exact:true})).toBeVisible();await expect(input).toHaveValue('retained draft');
  await page.getByRole('button',{name:'重新打开',exact:true}).click();await expect(page.getByRole('button',{name:'发送',exact:true})).toBeEnabled();
  const after=(await(await page.request.get(`/api/sessions/${session.id}?project=${session.project_key}`)).json()).data.generation;expect(after).not.toBe(before);await expect(input).toHaveValue('retained draft');
  await page.request.delete(`/api/sessions/${session.id}?project=${session.project_key}`);
});
