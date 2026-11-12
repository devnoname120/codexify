import assert from 'node:assert/strict';
import {existsSync, mkdirSync, readFileSync} from 'node:fs';
import {createRequire} from 'node:module';
import {fileURLToPath} from 'node:url';
import test, {after, before} from 'node:test';
import {ownerHtml, routeOwner} from './owner-chat-source.mjs';

const {chromium} = createRequire(new URL('../browser-companion/package.json', import.meta.url))('patchright');
const manifest = new URL('../browser-companion/.browsers/win32-x64/chrome-executable.json', import.meta.url);
const executablePath = process.env.CODEXIFY_CHROME_EXECUTABLE || (process.platform === 'win32' && existsSync(manifest) ? fileURLToPath(new URL(JSON.parse(readFileSync(manifest, 'utf8')).executable, manifest)) : undefined);
const origin = 'http://127.0.0.1:43261';
const meta = 'io.github.devnoname120/codexify/markdown-chat';
let browser;
before(async () => {browser = await chromium.launch({executablePath});});
after(async () => {await browser?.close();});

async function fixture(t, changes) {
  const page = await browser.newPage({viewport: {width: 1280, height: 900}, locale: 'en-US'});
  t.after(() => page.close());
  page.setDefaultTimeout(6000);
  await page.addInitScript(() => {window.testHidden = false; Object.defineProperty(document, 'hidden', {get: () => window.testHidden});});
  const now = Date.now();
  const chats = [1, 2].map(n => ({id: String(n).repeat(64), title: `Project ${n}`, workspace: `project-${n}`, projectPath: `/fixture/project-${n}`, goalStatus: 'active', lastEntryEnd: 100, lastEntryAtMs: now, totalToolCalls: 10, lastAgentCallAtMs: now}));
  const errors = [], requests = [];
  page.on('pageerror', error => errors.push(error.message));
  await routeOwner(page, origin + '/**', async route => {
    const path = new URL(route.request().url()).pathname;
    if (path === '/') return route.fulfill({contentType: 'text/html', body: ownerHtml});
    if (path === '/api/settings/connector') return route.fulfill({json: {configured: true, connectorName: 'Codexify'}});
    if (path === '/api/chats') return route.fulfill({json: {chats, serverTimeMs: now}});
    if (path === '/api/global') return route.fulfill({json: {version: '2.3.15', readiness: {ready: true}}});
    if (path === '/api/health') return route.fulfill({json: {status: 'ok'}});
    if (path === '/api/settings') return route.fulfill({json: {revision: 'one', document: {}, defaults: {}, raw: '{}'}});
    if (path.startsWith('/api/analytics')) return route.fulfill({json: {samples: 0, models: []}});
    if (path.endsWith('/changes')) {requests.push(path); return changes(route, path);}
    if (path.endsWith('/automation')) return route.fulfill({json: {enabled: true, pairing: {status: 'verified'}, selection: {}, action: {status: 'idle'}, companion: {authenticated: true, status: 'idle'}, presence: {lastAgentCallAtMs: now}}});
    if (path.endsWith('/read')) return route.fulfill({json: {ownerReadThrough: 100}});
    return route.fulfill({json: {_meta: {[meta]: {chat_file: '/fixture/CHAT.md', revision: 'one', delivered_through: 100, messages: [{id: 'message', role: 'agent', markdown: 'Implementation continues.', start: 0, end: 100}], total_tool_calls: 10, read_through: 100, has_more: false, server_time_ms: now}}}});
  });
  await page.goto(`${origin}/?chat=${chats[0].id}#fixture-token`, {timeout: 20000});
  await page.locator('#chat-host #composer').waitFor();
  return {page, chats, errors, requests, counter: page.locator('#chat-host #code-changes')};
}

test('compact additions/removals appear above chat without delaying its composer, and fit mobile', async t => {
  let release;
  const gate = new Promise(resolve => {release = resolve;});
  t.after(() => release());
  const f = await fixture(t, async route => {await gate; await route.fulfill({json: {available: true, summary: {files: 3, additions: 1234, deletions: 56, binaryFiles: 1}}});});
  await f.page.locator('#chat-host #draft').fill('Keep working');
  assert.equal(await f.counter.isVisible(), false);
  release();
  await f.counter.waitFor();
  assert.equal(await f.counter.locator('.added').textContent(), '+1,234');
  assert.equal(await f.counter.locator('.removed').textContent(), '−56');
  assert.match(await f.counter.getAttribute('title'), /including other agents/);
  await f.page.setViewportSize({width: 390, height: 860});
  assert.equal(await f.page.locator('#chat-host .header').evaluate(node => node.scrollWidth > node.clientWidth), false);
  const directory = fileURLToPath(new URL('../browser-companion/artifacts/ready-20261030/', import.meta.url));
  mkdirSync(directory, {recursive: true});
  await f.page.locator('#chat-host .header').screenshot({path: directory + 'code-change-counter.png'});
  assert.deepEqual(f.errors, []);
});

test('counter hides unavailable data and stops polling in hidden chats', async t => {
  const f = await fixture(t, route => route.fulfill({json: {available: false, refreshing: false}}));
  await f.counter.waitFor({state: 'attached'});
  await f.page.waitForTimeout(150);
  assert.equal(await f.counter.isVisible(), false);
  assert.equal(f.requests.length, 1);
  await f.page.evaluate(() => {window.testHidden = true; document.dispatchEvent(new Event('visibilitychange'));});
  await f.page.clock.install();
  await f.page.clock.fastForward(60_000);
  assert.equal(f.requests.length, 1);
  assert.deepEqual(f.errors, []);
});

test('switching chats discards a delayed previous-project count', async t => {
  let release;
  const gate = new Promise(resolve => {release = resolve;});
  t.after(() => release());
  const f = await fixture(t, async (route, path) => {
    const first = path.includes('1'.repeat(64));
    if (first) await gate;
    await route.fulfill({json: {available: true, summary: {files: 1, additions: first ? 999 : 7, deletions: 2, partial: true}, stale: true}}).catch(() => {});
  });
  await f.page.locator(`.conversation-row[data-chat-id="${f.chats[1].id}"] .conversation`).click();
  await f.counter.waitFor();
  release();
  await f.page.waitForTimeout(100);
  assert.equal(await f.counter.locator('.added').textContent(), '+7');
  assert.match(await f.counter.getAttribute('aria-label'), /partial count.*last successful/);
  assert.deepEqual(f.errors, []);
});
