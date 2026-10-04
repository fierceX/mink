// npm ci --prefix crates/mink-server/web; node scripts/docs-site.mjs [--build]
import assert from 'node:assert/strict';
import { readFileSync, existsSync, mkdirSync, cpSync, rmSync, readdirSync, statSync } from 'node:fs';
import { resolve, dirname, relative } from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { execFileSync } from 'node:child_process';
const root = fileURLToPath(new URL('../', import.meta.url));
const requireWeb = createRequire(resolve(root, 'crates/mink-server/web/package.json'));
const { JSDOM } = await import(pathToFileURL(requireWeb.resolve('jsdom')));
const require = createRequire(import.meta.url);
const shared = require('../docs/assets/docs-shared.js');
const marked = require('../docs/assets/vendor/marked.min.js');
const manifest = JSON.parse(readFileSync(resolve(root, 'docs/manifest.json'), 'utf8'));
const paths = new Set(), sources = new Set();
const errors = [];
for (const entry of manifest) {
  assert.deepEqual(Object.keys(entry).sort(), ['description','group','path','source','title']);
  for (const value of Object.values(entry)) assert.ok(typeof value === 'string' && value.trim());
  assert.match(entry.path, /^(?:[a-z][a-z-]*\/[a-z][a-z-]*|CHANGELOG)\.md$/);
  assert.ok(!entry.source.includes('..') && !entry.source.startsWith('/'));
  assert.ok(!paths.has(entry.path) && !sources.has(entry.source), `duplicate entry: ${entry.path}`);
  assert.ok(entry.source === `docs/${entry.path}` || entry.path === 'CHANGELOG.md' && entry.source === 'CHANGELOG.md');
  assert.ok(existsSync(resolve(root, entry.source)), `missing: ${entry.source}`);
  assert.ok(!entry.source.includes('/development/'));
  paths.add(entry.path); sources.add(entry.source);
}
const documents = new Map();
const assets = new Set(['assets/hero-replay.json','assets/mink-wordmark.svg','assets/vendor/README.md','assets/vendor/marked.LICENSE.md','assets/vendor/highlight.LICENSE']);
const formal = [...sources, ...['README.md','AGENTS.md','mink_agent/README.md', ...['mink-core','mink-cli','mink-server'].map(p=>`crates/${p}/README.md`)]];
for (const name of readdirSync(resolve(root, 'docs/development'), { recursive: true })) {
  if (name.endsWith('.md')) formal.push(`docs/development/${name}`);
}
formal.push('docs/migration.md');
for (const source of formal) {
  const markdown = readFileSync(resolve(root, source), 'utf8');
  const dom = new JSDOM(marked.parse(markdown));
  shared.assignHeadingIds(dom.window.document.querySelectorAll('h1,h2,h3,h4,h5,h6'));
  const ids = [...dom.window.document.querySelectorAll('[id]')].map(el=>el.id);
  if (new Set(ids).size !== ids.length) errors.push(`${source}: duplicate heading anchor`);
  documents.set(source, { markdown, dom, ids: new Set(ids) });
}
for (const [source, { dom }] of documents) {
  for (const el of dom.window.document.querySelectorAll('a[href],img[src]')) {
    const href = el.getAttribute(el.tagName === 'IMG' ? 'src' : 'href');
    if (!href || /^(?:[a-z][a-z\d+.-]*:|\/\/)/i.test(href)) continue;
    try {
      const url = new URL(href, `https://repository.invalid/${source}`);
      const target = decodeURIComponent(url.pathname.slice(1));
      if (!existsSync(resolve(root, target))) errors.push(`${source}: missing ${href} -> ${target}`);
      else if (el.tagName === 'IMG' && !statSync(resolve(root, target)).isFile()) errors.push(`${source}: image is not a file: ${href}`);
      if (url.hash && documents.has(target) && !documents.get(target).ids.has(decodeURIComponent(url.hash.slice(1)))) errors.push(`${source}: missing anchor ${href}`);
      // Public Markdown links to repository-only material remain GitHub links in the reader.
      const resolved = shared.resolveLink(source, href, manifest, 'https://example.test/mink/');
      if (sources.has(target) && resolved.kind !== 'doc') errors.push(`${source}: public link not routed: ${href}`);
      if (sources.has(source) && resolved.kind === 'asset' && target.startsWith('docs/assets/')) assets.add(target.slice(5));
    } catch (error) { errors.push(`${source}: invalid link ${href}: ${error.message}`); }
  }
}
const oldNames = ['ARCHITECTURE','DESIGN','EMBEDDING','USAGE','PROTOCOL','TUI_OPTIMIZATION_ROADMAP','TUI_PERFORMANCE-2026-10-04','server','tools','设计哲学-信号系统','设计哲学-工具能力与提示词解耦','设计哲学-多模态读图能力'];
const oldPaths = [...oldNames.map(name => `docs/${name}.md`), ['docs','assets','cases','env-reader.md'].join('/')];
for (const oldPath of oldPaths) if (existsSync(resolve(root, oldPath))) errors.push(`obsolete formal file still exists: ${oldPath}`);
const tracked = execFileSync('git', ['ls-files','-z'], { cwd: root, encoding: 'utf8' }).split('\0').filter(Boolean);
for (const source of [...new Set([...tracked, ...sources])]) {
  if (!existsSync(resolve(root, source)) || source === 'docs/migration.md' || source.startsWith('docs/assets/vendor/')) continue;
  const text = readFileSync(resolve(root, source), 'utf8');
  for (const oldPath of oldPaths) if (text.includes(oldPath)) errors.push(`${source}: obsolete formal path ${oldPath}`);
}
const index = readFileSync(resolve(root, 'docs/index.html'), 'utf8');
for (const match of index.matchAll(/(?:src|href)="(assets\/[^"#]+)"/g)) assets.add(match[1]);
for (const asset of assets) if (!existsSync(resolve(root, 'docs', asset))) errors.push(`missing static asset ${asset}`);
const replay = JSON.parse(readFileSync(resolve(root,'docs/assets/hero-replay.json'),'utf8'));
assert.ok(paths.has(replay.meta.caseDoc));
if (errors.length) { console.error(errors.join('\n')); process.exitCode = 1; }
else {
  console.log(`Documentation check passed: ${manifest.length} public entries, ${documents.size} Markdown sources, ${assets.size} static assets.`);
  if (process.argv.includes('--build')) {
    const out = resolve(root, 'target/docs-site');
    rmSync(out, { recursive: true, force: true }); mkdirSync(out, { recursive: true });
    function copy(source, target) { mkdirSync(dirname(target), { recursive: true }); cpSync(source,target); }
    for (const entry of manifest) copy(resolve(root,entry.source),resolve(out,entry.path));
    for (const asset of assets) copy(resolve(root,'docs',asset),resolve(out,asset));
    for (const name of ['index.html','manifest.json','.nojekyll']) copy(resolve(root,'docs',name),resolve(out,name));
    assert.ok(!existsSync(resolve(out,'development')));
    console.log(`Assembled allowlisted site: ${relative(root,out)}`);
  }
}
for (const {dom} of documents.values()) dom.window.close();
