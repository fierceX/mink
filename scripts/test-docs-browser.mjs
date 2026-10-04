// Serve target/docs-site at MINK_DOCS_URL (default http://127.0.0.1:8026/).
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { mkdirSync } from 'node:fs';
const require = createRequire(new URL('../crates/mink-server/web/package.json', import.meta.url));
const { chromium } = require('@playwright/test');
const browser = await chromium.launch({headless:true});
const base = process.env.MINK_DOCS_URL || 'http://127.0.0.1:8026/';
mkdirSync('target/docs-browser', {recursive:true});
for (let attempt=0; attempt<20; attempt++) {
  try { const response=await fetch(base); if(response.ok) break; } catch (_) { /* server starting */ }
  if(attempt===19) throw new Error(`Preview unavailable: ${base}`);
  await new Promise(resolve=>setTimeout(resolve,100));
}
try {
  for (const width of [320,390,768,1024,1440]) {
    const page = await browser.newPage({viewport:{width,height:900},reducedMotion:'reduce'});
    await page.context().grantPermissions(['clipboard-read','clipboard-write']);
    const errors=[]; page.on('pageerror', error=>errors.push(error.message));
    for (const path of ['start/overview.md','reference/configuration.md']) {
      await page.goto(`${base}#docs/${path}`);
      await page.locator('.doc-content h1').waitFor();
      assert.equal(await page.locator('.doc-pager').count(),1);
      const overflow = await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth);
      assert.equal(overflow,false,`${width} ${path}: page overflow`);
      if (path === 'reference/configuration.md') {
        const expected = await page.locator('.code-frame code').first().textContent();
        await page.locator('.code-copy').first().click();
        assert.equal(await page.evaluate(()=>navigator.clipboard.readText()), expected);
        await page.evaluate(()=>scrollTo(0,0));
      }
      await page.screenshot({path:`target/docs-browser/${width}-${path.split('/')[1]}.png`});
      if (width<=820) {
        await page.getByRole('button',{name:'文档导航',exact:true}).click();
        await page.getByRole('button',{name:'关闭导航'}).waitFor({state:'visible'});
        await page.keyboard.press('Shift+Tab');
        assert.ok(await page.evaluate(()=>document.getElementById('doc-sidebar').contains(document.activeElement)));
        await page.keyboard.press('Escape');
        assert.equal(await page.evaluate(()=>document.activeElement.id),'doc-nav-toggle');
      }
      if(width<=1240) {
        await page.getByRole('button',{name:'本文目录',exact:true}).click();
        await page.locator('#doc-toc-panel').waitFor({state:'visible'});
        await page.locator('#doc-toc-list a').first().click();
        assert.ok((await page.url()).includes('#'));
      }
    }
    await page.goto(`${base}#docs/guides/images.md`);
    await page.locator('.doc-content h1').waitFor();
    await page.locator('.doc-content a[href="#docs/reference/configuration.md#%E5%A4%9A%E6%A8%A1%E6%80%81%E5%9B%BE%E7%89%87%E8%BE%93%E5%85%A5"]').first().click();
    await page.waitForURL('**#docs/reference/configuration.md#*');
    await page.reload(); await page.locator('#多模态图片输入').waitFor();
    await page.goBack(); await page.locator('.doc-content h1').waitFor();
    await page.goForward(); await page.locator('#多模态图片输入').waitFor();
    assert.deepEqual(errors,[]);
    console.log(`Browser verification passed: ${width}px navigation/TOC/links/refresh/history/overflow`);
    await page.close();
  }
} finally {await browser.close();}
