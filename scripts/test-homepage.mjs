// Uses the existing web test dependency; no browser or provider credentials.
// npm ci --prefix crates/mink-server/web && node scripts/test-homepage.mjs
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { Script } from "node:vm";
import { test } from "node:test";

const root = new URL("../", import.meta.url);
const webRequire = createRequire(new URL("crates/mink-server/web/package.json", root));
const { JSDOM } = await import(pathToFileURL(webRequire.resolve("jsdom")));
const html = readFileSync(new URL("docs/index.html", root), "utf8");
const source = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)].map(match => match[1]).join("\n");
new Script(source);
const marked = readFileSync(new URL("docs/assets/vendor/marked.min.js", root), "utf8");
const shared = readFileSync(new URL("docs/assets/docs-shared.js", root), "utf8");
const manifest = JSON.parse(readFileSync(new URL("docs/manifest.json", root), "utf8"));
const replay = JSON.parse(readFileSync(new URL("docs/assets/hero-replay.json", root), "utf8"));
const settle = () => new Promise(resolve => setTimeout(resolve, 20));

async function page(t, { hash = "", reduced = false, fetchDoc, fetchReplay, clock } = {}) {
  const dom = new JSDOM(html, {
    url: `http://localhost/${hash}`,
    runScripts: "dangerously",
    pretendToBeVisual: true,
    beforeParse(w) {
      w.matchMedia = () => ({ matches: reduced, addEventListener() {} });
      if (clock) {
        w.setTimeout = clock.setTimeout;
        w.clearTimeout = clock.clearTimeout;
      }
      w.CSS = { escape: value => value }; // Fixture heading IDs need no escaping.
      w.scrollTo = () => {};
      w.HTMLElement.prototype.scrollIntoView = () => {};
      Object.defineProperty(w.HTMLElement.prototype, "innerText", { get() { return this.textContent; } });
      w.fetch = async path => {
        if (path === "manifest.json") return { ok: true, json: async () => manifest };
        if (path === "assets/hero-replay.json") return fetchReplay ? fetchReplay() : { ok: true, json: async () => replay };
        return fetchDoc ? fetchDoc(path) : { ok: true, text: async () => `# ${path}\n\n正文\n\n## 下一步` };
      };
      w.eval(marked);
      w.eval(shared);
    },
  });
  t.after(() => dom.window.close());
  const w = dom.window;
  await settle();
  return w;
}

test("homepage assets are local, version matches the workspace and sidebar references exist", async t => {
  const w = await page(t);
  const ids = [...w.document.querySelectorAll("[id]")].map(el => el.id);
  assert.equal(new Set(ids).size, ids.length);
  for (const el of w.document.querySelectorAll("script[src],link[href]")) {
    const asset = el.getAttribute("src") || el.getAttribute("href");
    assert.ok(!asset.startsWith("http"), `external dependency: ${asset}`);
    assert.ok(existsSync(new URL(`docs/${asset}`, root)), `missing asset: ${asset}`);
  }
  const version = readFileSync(new URL("Cargo.toml", root), "utf8").match(/^version = "([^"]+)"/m)[1];
  assert.ok(w.document.querySelector(".brand-version").textContent.includes(version));
  assert.ok(w.document.getElementById("code-rust").textContent.includes(`version = "${version}"`));
  for (const link of w.document.querySelectorAll(".doc-tree a")) {
    assert.ok(link.getAttribute("href"));
    assert.ok(link.dataset.path === "CHANGELOG.md" || existsSync(new URL(`docs/${link.dataset.path}`, root)));
  }
});

test("quickstart tabs support click, arrow keys and Home/End with a single keyboard stop", async t => {
  const w = await page(t);
  const tabs = [...w.document.querySelectorAll(".tab-button")];
  tabs[1].click();
  tabs[1].focus();
  assert.equal(w.document.querySelector(".tab-panel.active").id, "tab-python");
  tabs[1].dispatchEvent(new w.KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
  assert.equal(w.document.activeElement.id, "quick-tab-rust");
  assert.equal(w.document.querySelectorAll('.tab-button[aria-selected="true"]').length, 1);
  assert.equal(tabs.filter(tab => tab.tabIndex === 0).length, 1);
  w.document.activeElement.dispatchEvent(new w.KeyboardEvent("keydown", { key: "End", bubbles: true }));
  assert.equal(w.document.querySelector(".tab-panel.active").id, "tab-server");
  w.document.activeElement.dispatchEvent(new w.KeyboardEvent("keydown", { key: "Home", bubbles: true }));
  assert.equal(w.document.querySelector(".tab-panel.active").id, "tab-terminal");
});

test("documentation deep links restore on load and homepage navigation returns", async t => {
  const w = await page(t, { hash: "#docs/reference/http-api.md" });
  assert.ok(w.document.getElementById("page-docs").classList.contains("active"));
  assert.ok(w.document.querySelector("#doc-main h1").textContent.includes("reference/http-api.md"));
  w.document.querySelector(".skip-link").click();
  assert.equal(w.document.activeElement.id, "doc-main");
  assert.equal(w.location.hash, "#docs/reference/http-api.md");
  w.document.querySelector(".brand").click();
  assert.ok(w.document.getElementById("page-home").classList.contains("active"));
  assert.equal(w.location.hash, "#home");
  w.history.back();
  await settle();
  assert.ok(w.document.getElementById("page-docs").classList.contains("active"));
  assert.equal(w.document.getElementById("doc-current-path").textContent, "docs/reference/http-api.md");
});

test("heading and table-of-contents links retain the document route and reduced-motion scrolling", async t => {
  const w = await page(t, { hash: "#docs/start/quickstart.md", reduced: true,
    fetchDoc: () => ({ ok: true, text: async () => "# Manual\n\n[TOC]\n\n## Details\n\nBody\n\n## Next" }) });
  const scrolls = [];
  w.HTMLElement.prototype.scrollIntoView = options => scrolls.push(options);
  w.document.querySelector("#details .heading-anchor").click();
  assert.equal(w.location.hash, "#docs/start/quickstart.md#details");
  assert.equal(scrolls.at(-1).behavior, "auto");
  w.document.querySelector("#doc-toc-list li:last-child a").click();
  assert.equal(w.location.hash, "#docs/start/quickstart.md#next");
  w.handleRoute();
  await settle();
  assert.ok(w.document.getElementById("page-docs").classList.contains("active"));
  assert.equal(w.document.getElementById("doc-current-path").textContent, "docs/start/quickstart.md");
  assert.equal(scrolls.at(-1).behavior, "auto");
});

test("late document results do not replace a newer selection or scroll the homepage", async t => {
  let release;
  const w = await page(t, { fetchDoc: path => path === "guides/terminal.md" ? new Promise(resolve => { release = resolve; }) : { ok: true, text: async () => `# ${path}` } });
  const first = w.loadDoc("guides/terminal.md");
  await settle();
  await w.loadDoc("integration/rust.md");
  release({ ok: true, text: async () => "# Old document" });
  await first;
  assert.ok(w.document.querySelector("#doc-main h1").textContent.includes("integration/rust"));
  const pending = w.loadDoc("guides/terminal.md");
  await settle();
  w.showPage("home");
  release({ ok: true, text: async () => "# Late document" });
  await pending;
  assert.ok(w.document.getElementById("page-home").classList.contains("active"));
  assert.equal(w.location.hash, "#home");
});

test("missing local changelog offers the correct GitHub source without leaving the site", async t => {
  const w = await page(t, { fetchDoc: async () => ({ ok: false, status: 404 }) });
  await w.loadDoc("CHANGELOG.md");
  assert.equal(w.location.origin, "http://localhost");
  assert.equal(w.document.querySelector("#doc-main .error a").href, "https://github.com/fierceX/mink/blob/main/CHANGELOG.md");
});

test("copy uses raw multiline code and only reports success after a successful write", async t => {
  const w = await page(t);
  let copied;
  Object.defineProperty(w.navigator, "clipboard", { value: { writeText: async text => { copied = text; } } });
  const button = w.document.querySelector('[data-copy="code-terminal"]');
  button.click();
  await settle();
  assert.ok(copied.includes("\ncd mink\n"));
  assert.ok(copied.includes("cargo build --release -p mink-cli"));
  assert.equal(button.textContent, "已复制 ✓");
});

test("clipboard fallback restores focus and failed copying never claims success", async t => {
  const w = await page(t);
  const button = w.document.querySelector("[data-copy]");
  button.focus();
  w.document.execCommand = () => true;
  await w.copyText("test", button);
  assert.equal(w.document.activeElement, button);
  assert.equal(w.document.querySelectorAll("textarea").length, 0);
  button.textContent = "复制代码 ⧉";
  w.document.execCommand = () => false;
  await w.copyText("test", button);
  assert.equal(button.textContent, "复制代码 ⧉");
  assert.equal(w.document.getElementById("copy-toast").textContent, "复制失败，请手动选择代码复制");
});

test("reduced motion shows complete recorded rows without replaying animation", async t => {
  const w = await page(t, { reduced: true });
  assert.equal(w.document.getElementById("demo-pause").textContent, "播放");
  assert.equal(w.document.querySelectorAll(".tui-line").length, replay.meta.maxVisibleRows);
  assert.ok(w.document.querySelector(".tui-line:last-child").textContent.endsWith(replay.steps.at(-1).text));
  const content = w.document.getElementById("hero-tui-transcript").innerHTML;
  await settle();
  assert.equal(w.document.getElementById("hero-tui-transcript").innerHTML, content);
});

test("real case retains guidance, red/green verification and settled recorded stats", () => {
  assert.ok(existsSync(new URL(`docs/${replay.meta.caseDoc}`, root)));
  assert.equal(replay.meta.version, "0.6.6");
  assert.ok(replay.meta.guidance.includes("不展开变量"));
  const guidance = replay.steps.find(step => step.kind === "guidance");
  const input = replay.steps.find(step => step.kind === "input");
  assert.equal(input.text, replay.meta.guidance);
  assert.equal(input.sourceMessage, guidance.sourceMessage);
  assert.ok(replay.steps.some(step => step.line?.includes("test result: FAILED")));
  assert.ok(replay.steps.some(step => step.line?.includes("6 passed; 0 failed")));
  assert.ok(replay.steps.every(step => Number.isInteger(step.statusSourceEvent)));
  assert.ok(guidance.statusItems.some(item => item.text === "R:1"));
  assert.ok(guidance.statusItems.some(item => item.text === "I:2.6k(4%)"));
  assert.ok(guidance.statusItems.some(item => item.text === "B:—"));
  assert.ok(replay.steps.some(step => step.statusItems.some(item => item.text === "R:5")));
  assert.deepEqual(replay.steps.at(-1).statusItems.map(item => item.text), ["flash", "@env-reader", "B:0.92", "T:1", "R:9", "I:44.5k(80%)", "O:5.6k", "C:8.8k(0%)"]);
  assert.deepEqual(replay.steps.map(step => step.sourceMessage), replay.steps.map(step => step.sourceMessage).sort((a,b) => a-b));
});

function replayClock() {
  let id = 0;
  const timers = new Map();
  return {
    setTimeout(callback) { const key = ++id; timers.set(key, callback); return key; },
    clearTimeout(key) { timers.delete(key); },
    next() { const timer = timers.entries().next().value; assert.ok(timer, "scheduled replay tick"); timers.delete(timer[0]); timer[1](); },
    get size() { return timers.size; },
  };
}

test("status follows TUI field priority, restores fields after resize and reserves work state", async t => {
  const w = await page(t, { reduced: true });
  const status = w.document.getElementById("hero-tui-status");
  let columns = 100;
  Object.defineProperty(status, "clientWidth", { get: () => columns });
  Object.defineProperty(status, "scrollWidth", { get: () => [...status.children].reduce((total, item) => total + Array.from(item.textContent).length + 1, 0) });
  w.fitHeroStatus();
  assert.ok(status.querySelector(".tui-status-requests"));
  columns = 43;
  w.fitHeroStatus();
  assert.ok(!status.querySelector(".tui-status-requests"));
  assert.ok(!status.querySelector(".tui-status-turns"));
  assert.ok(!status.querySelector(".tui-status-output"));
  assert.ok(status.querySelector(".tui-status-context"));
  assert.equal(status.lastElementChild.textContent, "[idle]");
  columns = 16;
  w.fitHeroStatus();
  assert.deepEqual([...status.children].map(item => item.textContent), ["flash", "[idle]"]);
  columns = 100;
  w.fitHeroStatus();
  assert.ok(status.querySelector(".tui-status-requests"));
  assert.ok(status.querySelector(".tui-status-cwd"));
  w.updateHeroStatus(replay.steps.at(-1), true);
  assert.equal(status.lastElementChild.textContent, "[generating]");
  assert.equal(status.querySelector(".tui-status-input").textContent, "I:36.0k(77%)");
  assert.equal(status.querySelector(".tui-status-requests").textContent, "R:8");
  assert.equal(w.document.getElementById("hero-input-title").textContent, "Enter: submit guidance");
});

test("typed replay streams text, pauses without skipping, resumes after navigation and restarts", async t => {
  const clock = replayClock();
  const w = await page(t, { clock });
  const transcript = w.document.getElementById("hero-tui-transcript");
  assert.equal(transcript.children.length, 1);
  clock.next(); // Thinking begins with its prefix, not an entire paragraph.
  const row = transcript.lastElementChild;
  assert.ok(!row.textContent.includes(replay.steps[1].text));
  clock.next();
  const partial = row.textContent;
  assert.ok(partial.endsWith(Array.from(replay.steps[1].text)[0]));
  w.toggleHeroReplay();
  assert.equal(clock.size, 0);
  assert.equal(row.textContent, partial);
  w.toggleHeroReplay();
  clock.next();
  assert.ok(row.textContent.endsWith(Array.from(replay.steps[1].text).slice(0, 2).join("")));
  const position = row.textContent;
  await w.loadDoc("guides/terminal.md");
  assert.equal(clock.size, 0);
  w.showPage("home");
  clock.next();
  assert.ok(row.textContent.startsWith(position));
  assert.ok(row.textContent.length > position.length);
  w.restartHeroReplay();
  assert.equal(transcript.children.length, 1);
  assert.ok(transcript.firstElementChild.textContent.includes(".env 解析器"));
  // Finish the full recording to catch a skipped or duplicated typed step.
  for (let ticks = 0; ticks < 5000 && !transcript.lastElementChild.textContent.endsWith(replay.steps.at(-1).text); ticks++) clock.next();
  assert.ok(transcript.lastElementChild.textContent.endsWith(replay.steps.at(-1).text));
  assert.equal(transcript.children.length, replay.meta.maxVisibleRows);
  assert.equal(w.document.querySelector(".tui-status-work").textContent, "[idle]");
  assert.ok(w.document.getElementById("hero-tui-status").textContent.includes("I:44.5k(80%)"));
});

test("core chapter routes and guidance jump preserve recorded input and committed rows", async t => {
  const clock = replayClock();
  const w = await page(t, { hash: "#context", clock, reduced: true });
  assert.ok(w.document.getElementById("page-home").classList.contains("active"));
  assert.equal(w.document.querySelectorAll(".capability-row").length, 3);
  w.document.querySelector('.principle-nav a[href="#runtime"]').click();
  assert.equal(w.location.hash, "#runtime");
  w.playGuidanceExcerpt();
  assert.equal(w.location.hash, "#home");
  assert.equal(w.document.getElementById("demo-pause").textContent, "暂停");
  for (let i=0; i<1000 && !w.document.querySelector(".tui-line.guidance"); i++) clock.next();
  assert.ok(w.document.querySelector(".tui-line.guidance").textContent.endsWith(replay.meta.guidance));
  assert.equal(w.document.getElementById("hero-tui-input").textContent, "");
  assert.equal(w.document.querySelector(".tui-status-requests").textContent, "R:1");
  assert.equal(w.document.querySelector(".tui-status-input").textContent, "I:2.6k(4%)");
  assert.equal(w.document.getElementById("case-guidance").textContent, replay.meta.guidance);
});

test("failed replay loading reports an error instead of inventing a replacement session", async t => {
  const w = await page(t, { fetchReplay: () => ({ ok: false, status: 404 }) });
  assert.ok(w.document.getElementById("hero-tui-transcript").textContent.includes("加载失败"));
  assert.ok(w.document.getElementById("demo-pause").disabled);
  assert.ok(w.document.getElementById("demo-restart").disabled);
});

test("manifest drives grouped navigation, catalogue, pager and rejects removed paths", async t => {
  const w = await page(t, { hash: '#docs/start/overview.md' });
  assert.equal(w.document.querySelectorAll('.doc-tree a').length, manifest.length);
  assert.equal(w.document.querySelectorAll('.doc-catalogue a').length, manifest.length - 1);
  assert.equal(w.document.querySelector('.doc-pager a').getAttribute('href'), '#docs/start/quickstart.md');
  let fetched = false;
  w.fetch = async () => { fetched = true; throw new Error('must not fetch unknown path'); };
  await w.loadDoc('../private.md');
  assert.equal(fetched, false);
  assert.match(w.document.getElementById('doc-main').textContent, /文档不存在/);
  assert.match(w.document.querySelector('#doc-main a').href, /start\/overview/);
});

test("nested links, source paths, Chinese duplicate headings and images resolve under Pages prefix", async t => {
  const w = await page(t, { hash: '#docs/guides/images.md', fetchDoc: () => ({ok:true,text:async()=> '# 图片\n\n## 中文 标题\n\n## 中文 标题\n\n## 中文 标题-1\n\n[参考](../reference/tools.md#read) [源码](../../crates/mink-core/src/lib.rs) ![标志](../assets/mink-mark.svg)'} ) });
  assert.ok(w.document.getElementById('中文-标题-1'));
  assert.ok(w.document.getElementById('中文-标题-1-1'));
  const headingIds = [...w.document.querySelectorAll('#doc-main h1, #doc-main h2')].map(el => el.id);
  assert.equal(new Set(headingIds).size, headingIds.length);
  assert.equal(w.document.querySelector('#doc-main a[href*="reference/tools"]').getAttribute('href'), '#docs/reference/tools.md#read');
  assert.equal(w.document.querySelector('#doc-main a[href*="github.com"]').href, 'https://github.com/fierceX/mink/blob/main/crates/mink-core/src/lib.rs');
  const entry = manifest.find(e => e.path === 'guides/images.md');
  const resolved = w.MinkDocs.resolveLink(entry.source, '../assets/mink-mark.svg', manifest, 'https://example.test/mink/');
  assert.equal(resolved.href, 'https://example.test/mink/assets/mink-mark.svg');
});

test("mobile navigation traps keyboard focus and restores its trigger; TOC is independent", async t => {
  const w = await page(t, { hash: '#docs/start/overview.md' });
  Object.defineProperty(w, 'innerWidth', {value:390, configurable:true});
  const trigger = w.document.getElementById('doc-nav-toggle'); trigger.focus(); trigger.click();
  const first = w.document.querySelector('#doc-sidebar button');
  assert.equal(w.document.activeElement, first);
  first.dispatchEvent(new w.KeyboardEvent('keydown', {key:'Tab',shiftKey:true,bubbles:true}));
  assert.notEqual(w.document.activeElement, first);
  w.document.activeElement.dispatchEvent(new w.KeyboardEvent('keydown', {key:'Tab',bubbles:true}));
  assert.equal(w.document.activeElement, first);
  first.dispatchEvent(new w.KeyboardEvent('keydown', {key:'Escape',bubbles:true}));
  assert.equal(w.document.activeElement, trigger);
  assert.equal(trigger.getAttribute('aria-expanded'), 'false');
  w.document.getElementById('doc-toc-toggle').click();
  assert.ok(w.document.getElementById('doc-toc-panel').classList.contains('toc-open'));
  assert.ok(!w.document.querySelector('.sidebar-open'));
  trigger.focus(); trigger.click();
  w.showPage('home', true);
  assert.ok(!w.document.querySelector('.sidebar-open'));
  assert.ok(w.document.getElementById('doc-nav-backdrop').hidden);
  assert.equal(w.document.querySelector('.nav').inert, false);
  assert.ok(!w.document.body.classList.contains('doc-nav-open'));
});

test("document code copying preserves tabs, spaces and final newlines; scrollspy uses viewport positions", async t => {
  const w = await page(t, { hash:'#docs/guides/terminal.md', fetchDoc:()=>({ok:true,text:async()=> '# 操作\n\n## 一\n\n```text\n\t first  \nsecond\n```\n\n## 二'}) });
  let copied;
  Object.defineProperty(w.navigator,'clipboard',{value:{writeText:async text=>{copied=text;}}});
  w.document.querySelector('.code-copy').click(); await settle();
  assert.equal(copied, '\t first  \nsecond\n');
  w.document.getElementById('一').getBoundingClientRect = ()=>({top:-100});
  w.document.getElementById('二').getBoundingClientRect = ()=>({top:500});
  w.updateScrollChrome();
  assert.equal(w.document.querySelector('.doc-toc-list a.active').getAttribute('href'), '#%E4%B8%80');
});
