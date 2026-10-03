import { openSettings } from './settings-helper';
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
  await page.getByRole('button',{name:'关闭详情',exact:true}).click();
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
    await page.getByRole('button', { name: '关闭详情', exact: true }).click();
    await textarea.fill('draft remains editable');
    if (width < 768) { await textarea.press('Enter'); await expect(textarea).toHaveValue('draft remains editable\n'); }
    await textarea.fill('');
    await page.screenshot({animations:'disabled',path:`/private/tmp/mink-workbench-${width}.png`});
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
  await page.getByRole('button',{name:'详情',exact:true}).click(); await page.getByRole('button',{name:'关闭详情',exact:true}).click();
  expect(Math.abs(await transcript.evaluate(el=>el.scrollTop)-before)).toBeLessThan(5);
  await page.getByRole('button',{name:'↓ 返回最新内容'}).click(); await expect.poll(()=>transcript.evaluate(el=>el.scrollHeight-el.scrollTop-el.clientHeight)).toBeLessThan(5);
});
test('theme and task creation use accessible in-app controls',async({page})=>{
  await open(page); await openSettings(page); await page.getByRole('combobox',{name:'外观',exact:true}).selectOption('dark'); await page.getByRole('button',{name:'关闭设置',exact:true}).click(); await expect(page.locator('html')).toHaveAttribute('data-theme','dark');
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
  await page.getByRole('button',{name:'会话操作菜单',exact:true}).click();await page.getByRole('menuitem',{name:'释放运行时（停止当前任务）'}).click();
  await expect(page.getByRole('button',{name:'重新打开',exact:true})).toBeVisible();await expect(input).toHaveValue('retained draft');
  await page.getByRole('button',{name:'重新打开',exact:true}).click();await expect(page.getByRole('button',{name:'发送',exact:true})).toBeEnabled();
  const after=(await(await page.request.get(`/api/sessions/${session.id}?project=${session.project_key}`)).json()).data.generation;expect(after).not.toBe(before);await expect(input).toHaveValue('retained draft');
  await page.request.delete(`/api/sessions/${session.id}?project=${session.project_key}`);
});

test('action menu dismisses outside, restores keyboard focus, and closes after selection',async({page})=>{
  await open(page);
  const menu=page.getByRole('button',{name:'会话操作菜单',exact:true});
  await menu.click(); await expect(menu).toHaveAttribute('aria-expanded','true');
  await page.locator('.crumb').click(); await expect(menu).toHaveAttribute('aria-expanded','false');
  await menu.click(); await page.keyboard.press('Escape'); await expect(menu).toBeFocused();
  await expect(page.locator('.action-menu')).toHaveCount(0);
  const rowMenu=page.locator('.sess-row').first().getByLabel('会话操作'); await rowMenu.click();
  await page.getByRole('menuitem',{name:'删除会话',exact:true}).click();
  await expect(page.locator('.action-menu')).toHaveCount(0); await expect(page.locator('.delete-confirm')).toBeVisible();
  await page.locator('.delete-confirm').getByRole('button',{name:'取消',exact:true}).click();
});

test('mobile drawers have reachable touch targets, local focus and restored reading position',async({page})=>{
  await page.setViewportSize({width:390,height:844}); await open(page);
  const navigation=page.getByRole('button',{name:'切换导航'});
  for (const button of await page.locator('.topbar button,.composer-actions button').all()) {
    const bounds=await button.boundingBox(); if(bounds) { expect(bounds.height).toBeGreaterThanOrEqual(32); expect(bounds.height).toBeLessThanOrEqual(40); expect(bounds.width).toBeGreaterThanOrEqual(32); expect(bounds.width).toBeLessThanOrEqual(40); }
  }
  expect((await page.locator('.composer').boundingBox())!.height).toBeLessThan(126);
  await navigation.click(); const drawer=page.getByRole('dialog',{name:'项目会话导航'});
  await expect(drawer).toBeVisible(); const close=page.getByRole('button',{name:'关闭导航',exact:true}); await expect(close).toBeFocused(); expect((await close.boundingBox())!.width).toBe(34); await page.screenshot({animations:'disabled',path:'/private/tmp/mink-web-polish-navigation-390.png'});
  await drawer.getByRole('button',{name:'会话操作',exact:true}).last().focus(); await drawer.getByRole('button',{name:'会话操作',exact:true}).last().press('Tab');
  await expect(drawer.getByRole('button',{name:'＋ 新建任务'})).toBeFocused();
  await page.keyboard.press('Escape'); await expect(navigation).toBeFocused();
  // Selecting the already active session also dismisses the drawer.
  await navigation.click(); await page.locator('.session-select[aria-current="page"]').click(); await expect(navigation).toHaveAttribute('aria-expanded','false');
  await navigation.click(); await drawer.getByRole('button',{name:'＋ 新建任务'}).click();
  await expect(page.getByLabel('工作目录')).toBeFocused(); await page.keyboard.press('Tab'); await expect(page.getByLabel('任务名称（可选）')).toBeFocused();
  await page.keyboard.press('Escape'); await expect(page.locator('dialog[open]')).toHaveCount(0); await expect(drawer).toBeVisible();
  await close.click();
  const transcript=page.locator('.transcript'); await transcript.hover(); await page.mouse.wheel(0,-400); await expect(page.getByRole('button',{name:'↓ 返回最新内容'})).toBeVisible();
  const before=await transcript.evaluate(el=>el.scrollTop);
  const details=page.getByRole('button',{name:'详情',exact:true}); await details.click();
  await expect(page.getByRole('button',{name:'关闭详情',exact:true})).toBeFocused();
  await page.keyboard.press('Escape'); await expect(details).toBeFocused();
  expect(Math.abs(await transcript.evaluate(el=>el.scrollTop)-before)).toBeLessThan(5);
});

test('nested file navigation and restored multiline drafts remain usable on a small viewport',async({page})=>{
  await page.setViewportSize({width:390,height:844}); await open(page);
  const input=page.getByRole('textbox',{name:'任务或补充指令'});
  await input.fill('one\ntwo\nthree\nfour\nfive'); const expanded=(await input.boundingBox())!.height;
  await page.reload(); await expect(input).toHaveValue('one\ntwo\nthree\nfour\nfive');
  await expect.poll(async()=>Math.abs((await input.boundingBox())!.height-expanded)).toBeLessThan(3);
  await page.getByRole('button',{name:'详情',exact:true}).click(); await page.getByRole('button',{name:'文件',exact:true}).click();
  await page.locator('.file-tree').getByRole('button',{name:'▸ src',exact:true}).click();
  await expect(page.locator('.directory-path')).toHaveText('src/');
  await page.locator('.file-tree').getByRole('button',{name:'· main.ts',exact:true}).click();
  await expect(page.locator('.file-content')).toContainText('createApp'); await expect(page.locator('.file-tree')).toHaveCount(0); await page.screenshot({animations:'disabled',path:'/private/tmp/mink-web-polish-details-390.png'});
  await page.getByRole('button',{name:'返回目录',exact:true}).click(); await expect(page.locator('.directory-path')).toHaveText('src/');
  await page.getByRole('button',{name:'上一级',exact:true}).click(); await expect(page.locator('.directory-path')).toHaveText('项目根目录');
  await page.getByRole('button',{name:'关闭详情',exact:true}).click(); await input.fill('');
  expect((await input.boundingBox())!.height).toBeLessThan(expanded);
  await page.setViewportSize({width:390,height:430}); await input.fill('keyboard-sized viewport');
  const send=await page.getByRole('button',{name:'发送',exact:true}).boundingBox(); expect(send!.y+send!.height).toBeLessThanOrEqual(430);
  expect(await page.evaluate(()=>document.scrollingElement!.scrollWidth<=innerWidth)).toBe(true);
});


test('diagnostics match TUI cache partition and survive refresh with complete counters',async({page})=>{
  await page.setViewportSize({width:1440,height:844});
  const session=await create(page); const input=page.getByRole('textbox',{name:'任务或补充指令'});
  await input.fill('cache-metrics-fixture'); await page.getByRole('button',{name:'发送',exact:true}).click();
  await expect(page.locator('.msg.agent').last()).toContainText('fixture response');
  await expect(page.getByRole('status').filter({hasText:'空闲'})).toBeVisible();
  await page.getByRole('button',{name:'详情',exact:true}).click(); await page.getByRole('button',{name:'诊断',exact:true}).click();
  await expect(page.getByRole('button',{name:'诊断',exact:true})).toHaveAttribute('aria-pressed','true');
  await expect(page.locator('.detail-panel header .selected')).toHaveText('诊断');
  const metric=(label:string)=>page.locator('.diagnostic-group dt').filter({hasText:new RegExp(`^${label}$`)}).locator('+ dd');
  await expect(metric('缓存命中率')).toHaveText('60%'); await expect(metric('缓存读取')).toHaveText('60');
  await expect(metric('缓存创建')).toHaveText('10'); await expect(metric('未缓存输入')).toHaveText('30');
  await expect(metric('输入')).toHaveText('90'); await expect(metric('输出')).toHaveText('20');
  await expect(metric('轮次')).toHaveText('1'); await expect(metric('模型请求')).toHaveText('1');
  await expect(metric('计划')).toHaveText('无'); await expect(metric('Todo')).toHaveText('进行中 0 / 待办 0');
  await page.screenshot({animations:'disabled',path:'/private/tmp/mink-ui-review-diagnostics-1440.png'});
  await page.reload(); await page.getByRole('button',{name:'详情',exact:true}).click(); await expect(metric('缓存命中率')).toHaveText('60%'); await expect(metric('模型请求')).toHaveText('1');
  await page.setViewportSize({width:390,height:844}); await page.screenshot({animations:'disabled',path:'/private/tmp/mink-ui-review-diagnostics-390.png'});
  await page.request.delete(`/api/sessions/${session.id}?project=${session.project_key}`);
});

test('compact composer supports multiline, attachment and dark mode without persistent footer bars',async({page})=>{
  await page.setViewportSize({width:1440,height:844});
  const session=await create(page);const input=page.getByRole('textbox',{name:'任务或补充指令'});
  await input.fill('检查配置并运行验证\n保持公共接口兼容');
  const png=Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGP4DwQACfsD/fteaysAAAAASUVORK5CYII=','base64');
  await page.locator('input[type=file]').setInputFiles({name:'reference.png',mimeType:'image/png',buffer:png});
  await expect(page.locator('.upload')).toContainText('已上传');
  await page.screenshot({animations:'disabled',path:'/private/tmp/mink-ui-review-composer-1440.png'});
  await page.setViewportSize({width:390,height:844});await page.screenshot({animations:'disabled',path:'/private/tmp/mink-ui-review-composer-390.png'});
  await page.getByRole('button',{name:'移除附件',exact:true}).click();await expect(page.locator('.upload')).toHaveCount(0);await expect(page.getByRole('button',{name:'发送',exact:true})).toBeEnabled();
  await openSettings(page); await page.getByRole('combobox',{name:'外观',exact:true}).selectOption('dark'); await page.getByRole('button',{name:'关闭设置',exact:true}).click();await expect(page.getByRole('button',{name:'发送',exact:true})).toBeEnabled();
  await page.screenshot({animations:'disabled',path:'/private/tmp/mink-ui-review-dark-390.png'});
  await page.request.delete(`/api/sessions/${session.id}?project=${session.project_key}`);
});

test('loaded-turn menu jumps without adding a footer row and Escape keeps the mobile drawer open',async({page})=>{
  await open(page);await page.getByRole('button',{name:'跳转已加载轮次',exact:true}).click();
  await page.getByRole('menuitem').filter({hasText:'fixture question 25'}).click();
  await expect(page.getByRole('button',{name:'↓ 返回最新内容'})).toBeVisible();
  await expect.poll(async()=>Math.abs((await page.locator('.turn').first().boundingBox())!.y-(await page.locator('.transcript').boundingBox())!.y)).toBeLessThan(30);
  await page.setViewportSize({width:390,height:844});await page.getByRole('button',{name:'切换导航'}).click();
  const drawer=page.getByRole('dialog',{name:'项目会话导航'});await drawer.locator('.sess-row').first().getByLabel('会话操作').click();
  await page.keyboard.press('ArrowDown');await expect(page.getByRole('menuitem',{name:'删除会话'})).toBeFocused();
  await page.keyboard.press('Escape');await expect(page.locator('.action-menu')).toHaveCount(0);await expect(drawer).toBeVisible();
});


test('running diagnostics retain activity after snapshot reconnect',async({page})=>{
  const session=await create(page);const input=page.getByRole('textbox',{name:'任务或补充指令'});
  await input.fill('slow-task');await page.getByRole('button',{name:'发送',exact:true}).click();
  await expect(page.locator('.process-group > summary').last()).toContainText('正在思考');
  await page.reload();await page.getByRole('button',{name:'详情',exact:true}).click();await page.getByRole('button',{name:'诊断',exact:true}).click();
  await expect(page.locator('.diagnostic-group dt').filter({hasText:'工作状态'}).locator('+ dd')).toHaveText('思考中');
  await page.screenshot({animations:'disabled',path:'/private/tmp/mink-ui-review-running.png'});
  await page.getByRole('button',{name:'关闭详情',exact:true}).click();
  await page.getByRole('button',{name:'停止',exact:true}).click();await expect(page.getByRole('button',{name:'发送',exact:true})).toBeVisible();
  await page.getByRole('button',{name:'详情',exact:true}).click();
  await expect(page.locator('.diagnostic-group dt').filter({hasText:'工作状态'}).locator('+ dd')).toHaveText('空闲');
  await page.request.delete(`/api/sessions/${session.id}?project=${session.project_key}`);
});


for (const mode of ['native','absent','denied'] as const) {
  test(`copying messages and file paths writes the real clipboard (${mode})`,async({page,context})=>{
    await context.grantPermissions(['clipboard-read','clipboard-write']);await open(page);
    await page.evaluate(mode=>{
      const original=navigator.clipboard;(window as any).fixtureReadClipboard=()=>original.readText();
      if(mode!=='native')Object.defineProperty(navigator,'clipboard',{configurable:true,value:mode==='absent'?undefined:{writeText:()=>Promise.reject(new DOMException('denied','NotAllowedError'))}});
    },mode);
    const message=page.locator('.msg.user').last();const text=await message.locator('.bubble').textContent();
    await message.getByRole('button',{name:'复制',exact:true}).click();await expect(message.getByRole('button',{name:'已复制',exact:true})).toBeVisible();
    expect(await page.evaluate(()=>(window as any).fixtureReadClipboard())).toBe(text);
    await page.getByRole('button',{name:'详情',exact:true}).click();await page.getByRole('button',{name:'文件',exact:true}).click();
    await page.locator('.file-tree').getByRole('button',{name:'· README.md',exact:true}).click();await page.getByRole('button',{name:'复制路径',exact:true}).click();
    await expect(page.getByRole('status').filter({hasText:'已复制路径'})).toBeVisible();expect(await page.evaluate(()=>(window as any).fixtureReadClipboard())).toBe('README.md');
  });
}
test('blocked copying reports a useful error without a false success',async({page})=>{
  await open(page);await page.evaluate(()=>{Object.defineProperty(navigator,'clipboard',{configurable:true,value:undefined});document.execCommand=()=>false;});
  const message=page.locator('.msg.user').last();await message.getByRole('button',{name:'复制',exact:true}).click();
  await expect(page.getByRole('status').filter({hasText:'浏览器未允许复制'})).toBeVisible();await expect(message.getByRole('button',{name:'已复制',exact:true})).toHaveCount(0);
});

test('mobile turn selection is a bounded vertical list with keyboard and touch-sized rows',async({page})=>{
  await page.setViewportSize({width:390,height:844});await open(page);
  await page.getByRole('button',{name:'跳转已加载轮次',exact:true}).click();
  const menu=page.getByRole('menu',{name:'跳转已加载轮次',exact:true});await expect(menu).toBeVisible();
  const rows=menu.getByRole('menuitem');const boxes=await rows.evaluateAll(nodes=>nodes.map(node=>{const r=node.getBoundingClientRect();return {x:r.x,y:r.y,width:r.width,height:r.height}}));
  expect(boxes).toHaveLength(20);expect(boxes[1].y).toBeGreaterThanOrEqual(boxes[0].y+boxes[0].height-1);expect(boxes[1].x).toBeCloseTo(boxes[0].x,0);
  const geometry=(await menu.boundingBox())!;expect(geometry.height).toBeLessThanOrEqual(260);expect(geometry.width).toBeLessThanOrEqual(374);
  await page.screenshot({animations:'disabled',path:'/private/tmp/mink-mobile-turn-list.png'});
  await page.keyboard.press('End');await expect(rows.last()).toBeFocused();await page.keyboard.press('Enter');await expect(menu).toHaveCount(0);
  await page.getByRole('button',{name:'跳转已加载轮次',exact:true}).click();await rows.first().click();await expect(menu).toHaveCount(0);await expect(page.locator('.msg.user').first()).toContainText('fixture question 25');
});
