import { openSettings } from './settings-helper';
import { test, expect } from '@playwright/test';
import { E2E_MOBILE_ID, LONG_LINE } from './mobile-fixture';

for(const width of [320,390,768,1440]) {
  test(`reading width and wrapping remain bounded at ${width}px`,async({page}, testInfo)=>{
    await page.setViewportSize({width,height:844});await page.goto(`/?session=${E2E_MOBILE_ID}`);
    const transcript=page.locator('.transcript');const code=page.locator('.msg.agent pre');
    await expect(code).toHaveText(LONG_LINE);await expect(page.locator('html')).toHaveAttribute('data-wrap','on');
    for(const element of [code,page.locator('.msg.agent table')])expect(await element.evaluate(el=>el.scrollWidth<=el.clientWidth+1)).toBe(true);
    expect(await transcript.evaluate(el=>el.scrollWidth<=el.clientWidth+1)).toBe(true);
    await page.locator('.process-group > summary').click();await page.locator('.tool-card > summary').click();
    expect(await page.locator('.t-file').evaluate(el=>el.scrollWidth<=el.clientWidth+1)).toBe(true);
    await code.scrollIntoViewIfNeeded();await page.screenshot({animations:'disabled',path:testInfo.outputPath(`mink-mobile-wrap-${width}.png`)});
    await page.getByRole('button',{name:'跳转已加载轮次',exact:true}).click();
    const wrap=page.getByRole('menuitemcheckbox',{name:'自动换行'});await expect(wrap).toHaveAttribute('aria-checked','true');await wrap.click();await expect(wrap).toHaveAttribute('aria-checked','false');
    await page.keyboard.press('Escape');await expect(page.locator('html')).toHaveAttribute('data-wrap','off');
    expect(await code.evaluate(el=>el.scrollWidth>el.clientWidth)).toBe(true);expect(await transcript.evaluate(el=>el.scrollWidth<=el.clientWidth+1)).toBe(true);
    await page.reload();await expect(page.locator('html')).toHaveAttribute('data-wrap','off');await expect(code).toHaveText(LONG_LINE);
    await openSettings(page);
    await page.getByRole('checkbox',{name:'自动换行',exact:true}).check();await expect(page.locator('html')).toHaveAttribute('data-wrap','on');
    await page.getByRole('button',{name:'关闭设置',exact:true}).click();
    await page.getByRole('button',{name:'详情',exact:true}).click();await page.getByRole('button',{name:'文件',exact:true}).click();
    await page.locator('.file-tree').getByRole('button',{name:'· long-lines.txt',exact:true}).click();
    expect(await page.locator('.file-content').evaluate(el=>el.scrollWidth<=el.clientWidth+1)).toBe(true);
    expect(await page.evaluate(()=>document.scrollingElement!.scrollWidth<=innerWidth)).toBe(true);
  });
}

test.describe('mobile reading controls',()=>{
  test.use({hasTouch:true,isMobile:true,viewport:{width:390,height:844}});
  async function swipe(page:import('@playwright/test').Page,distance:number,x=190,y=350) {
    const cdp=await page.context().newCDPSession(page);
    const frame=()=>page.evaluate(()=>new Promise<void>(resolve=>requestAnimationFrame(()=>resolve())));
    try {
      // Use trusted touch input; native gesture synthesis can omit movement on Linux headless Chromium.
      await cdp.send('Input.dispatchTouchEvent',{type:'touchStart',touchPoints:[{x,y,id:0}]});
      const steps=Math.ceil(Math.abs(distance)/12);
      for(let step=1;step<=steps;step++) {
        await cdp.send('Input.dispatchTouchEvent',{type:'touchMove',touchPoints:[{x,y:y+distance*step/steps,id:0}]});
        await frame();
      }
      // Hold the final point before release so inertia cannot shift the captured reading anchor.
      for(let held=0;held<5;held++)await frame();
      await cdp.send('Input.dispatchTouchEvent',{type:'touchEnd',touchPoints:[]});
      await frame();await frame();
    } finally { await cdp.detach(); }
  }
  test('upward swipes hide chrome; down, taps and reveal controls restore it without losing the draft or anchor',async({page}, testInfo)=>{
    await page.goto('/?session=e2e-session');const input=page.getByRole('textbox',{name:'任务或补充指令'});await expect(input).toBeVisible();
    await input.fill('保留手机草稿');await input.evaluate(el=>(el as HTMLTextAreaElement).blur());
    const transcript=page.locator('.transcript');await transcript.evaluate(el=>el.scrollTop=400);await expect(page.locator('.topbar')).toBeVisible();
    const height=await transcript.evaluate(el=>el.clientHeight);
    await swipe(page,-220);await expect(page.getByRole('button',{name:'显示顶部栏',exact:true})).toBeVisible();await expect(input).not.toBeVisible();
    expect(await transcript.evaluate(el=>el.clientHeight)).toBeGreaterThan(height+100);
    await page.screenshot({animations:'disabled',path:testInfo.outputPath('mink-mobile-reading-hidden.png')});
    const anchor=await transcript.evaluate(el=>{const top=el.getBoundingClientRect().top;const node=[...el.querySelectorAll<HTMLElement>('[data-item-key]')].find(n=>n.getBoundingClientRect().bottom>top+1)!;return {key:node.dataset.itemKey,offset:node.getBoundingClientRect().top-top}});
    await page.getByRole('button',{name:'显示顶部栏',exact:true}).tap();await expect(input).toBeVisible();
    await expect.poll(()=>transcript.evaluate((el,anchor)=>{const node=[...el.querySelectorAll<HTMLElement>('[data-item-key]')].find(n=>n.dataset.itemKey===anchor.key)!;return Math.abs(node.getBoundingClientRect().top-el.getBoundingClientRect().top-anchor.offset)},anchor)).toBeLessThan(4);
    await swipe(page,-180);await expect(input).not.toBeVisible();await swipe(page,180);await expect(input).toBeVisible();
    await swipe(page,-180);await expect(input).not.toBeVisible();
    const paragraphs=page.locator('.msg.agent p');const index=await paragraphs.evaluateAll(nodes=>nodes.findIndex(n=>{const r=n.getBoundingClientRect();return r.y>80&&r.bottom<650}));expect(index).toBeGreaterThanOrEqual(0);await paragraphs.nth(index).tap();await expect(input).toBeVisible();
    await swipe(page,-180);await expect(input).not.toBeVisible();await page.getByRole('button',{name:'显示输入框',exact:true}).tap();await expect(input).toBeFocused();await expect(input).toHaveValue('保留手机草稿');
    await swipe(page,-180);await expect(input).toBeVisible(); // focused typing takes precedence over reading mode
    await input.evaluate(el=>(el as HTMLTextAreaElement).blur());await swipe(page,-180);await expect(input).not.toBeVisible();
    await page.keyboard.press('Escape');await expect(input).toBeVisible();await swipe(page,-180);await expect(input).not.toBeVisible();
    await page.setViewportSize({width:1024,height:844});await expect(page.locator('.topbar')).toBeVisible();await expect(input).toBeVisible();
  });
  test('scrolling inside long tool output never hides the outer controls',async({page})=>{
    await page.goto(`/?session=${E2E_MOBILE_ID}`);await expect(page.locator('.msg.agent h1')).toBeVisible();
    await page.locator('.process-group > summary').click();await page.locator('.tool-card > summary').click();const body=page.locator('.tool-card .t-body');await body.scrollIntoViewIfNeeded();
    const box=(await body.boundingBox())!;const before=await body.evaluate(el=>el.scrollTop);await swipe(page,-140,box.x+40,box.y+80);
    expect(await body.evaluate(el=>el.scrollTop)).toBeGreaterThan(before);await expect(page.locator('.topbar')).toBeVisible();await expect(page.getByRole('textbox',{name:'任务或补充指令'})).toBeVisible();
  });
  test('reading mode keeps Stop reachable and restores the composer on stop',async({page}, testInfo)=>{
    await page.goto(`/?session=${E2E_MOBILE_ID}`);const input=page.getByRole('textbox',{name:'任务或补充指令'});await expect(input).toBeVisible();
    await input.fill('slow-task');await page.getByRole('button',{name:'发送',exact:true}).tap();await expect(page.getByRole('button',{name:'提交引导',exact:true})).toBeVisible();
    await expect(page.locator('.process-group > summary').last()).toContainText('正在思考');
    await page.locator('.transcript').evaluate(el=>el.scrollTop=300);await swipe(page,-160);
    await expect(page.getByRole('button',{name:'显示输入框',exact:true})).toBeVisible();await expect(page.getByRole('button',{name:'停止',exact:true})).toBeVisible();
    await page.screenshot({animations:'disabled',path:testInfo.outputPath('mink-mobile-reading-running.png')});
    await page.getByRole('button',{name:'停止',exact:true}).tap();await expect(page.getByRole('button',{name:'发送',exact:true})).toBeVisible();await expect(input).toBeVisible();
  });
  test('accepted guidance reveals the composer immediately and remains visible after Stop',async({page})=>{
    await page.goto(`/?session=${E2E_MOBILE_ID}`);const input=page.getByRole('textbox',{name:'任务或补充指令'});await expect(input).toBeVisible();
    await input.fill('slow-task');await page.getByRole('button',{name:'发送',exact:true}).tap();await expect(page.locator('.process-group > summary').last()).toContainText('正在思考');
    await page.locator('.transcript').evaluate(el=>el.scrollTop=300);await swipe(page,-160);await expect(input).not.toBeVisible();
    const session=(await(await page.request.get(`/api/sessions/${E2E_MOBILE_ID}`)).json()).data;
    const response=await page.request.post(`/api/sessions/${E2E_MOBILE_ID}/inputs`,{data:{request_id:`mobile-guidance-${Date.now()}`,text:'guidance keep the composer visible',attachment_ids:[],target_turn_id:session.current_turn}});expect(response.ok()).toBe(true);
    await expect(page.locator('.pending-input')).toContainText('guidance keep the composer visible');await expect(input).toBeVisible();await expect(page.locator('.topbar')).toBeVisible();
    await page.getByRole('button',{name:'停止',exact:true}).tap();await expect(page.locator('.pending-input')).toContainText('本轮已结束，尚未应用');
  });
});
