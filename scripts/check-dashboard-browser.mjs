#!/usr/bin/env node
//
// Drive the dashboard in a real browser and fail when it regresses.
//
// The dashboard is hand-written Preact with no build step, so nothing else
// executes it: `node --check` sees only syntax and the Rust tests never run
// the JavaScript. This is the check that notices a render crash, a handler
// wired to a renamed method, a fetch to a route that moved, a request the
// server answers 429, a colour that stopped meeting WCAG AA, or a panel that
// prints "undefined".
//
// What it asserts, per theme and viewport:
//   - a cold load raises no console error or warning (this includes
//     Content-Security-Policy violations) and no failed or >= 400 request,
//     stays within the client's in-flight cap and never needed a 429 retry
//   - every key panel renders, and no panel shows "undefined", "NaN",
//     "[object" or raw JSON
//   - the page never scrolls sideways, and layout shift stays small
//   - keyboard: a skip link comes first, every stop is visible and shows a
//     focus indicator, and Tab does not get stuck
//   - axe-core finds no serious or critical accessibility violation
// and, once, the behaviour that used to be missing: shareable URL state,
// removable filter chips, custom-range validation, comparison state,
// keyboard-operable tabs, the chart's table alternative, retry after a 429,
// and a readable message (not raw JSON) after a 500 or a lost connection.
//
// Not part of CI, because it needs a browser. Run it before shipping a
// dashboard change:
//
//   cargo build
//   MALLARD_DATA_DIR=/tmp/mm-ui MALLARD_PORT=18333 target/debug/mallard-metrics &
//   python3 scripts/seed-demo-data.py /tmp/mm-ui/mallard.duckdb   # then restart
//   node scripts/check-dashboard-browser.mjs http://127.0.0.1:18333
//
// Optional extra instances:
//   --empty=URL     an instance with no events (checks the empty states)
//   --nobeh=URL     an instance whose behavioral extension failed to load
//   --auth=URL --password=PW   an instance with an admin password
//
// Requires Playwright and axe-core, resolved from the normal module path or
// from NODE_PATH (ES modules ignore NODE_PATH, so it is searched here):
//   npm i -g playwright axe-core && npx playwright install chromium

import { createRequire } from 'node:module';
import path from 'node:path';

const args = process.argv.slice(2);
const flag = (name) => args.find((a) => a.startsWith(`--${name}=`))?.slice(name.length + 3);
const BASE = (args.find((a) => !a.startsWith('--')) ?? 'http://127.0.0.1:8000').replace(/\/$/, '');
const EMPTY = flag('empty');
const NOBEH = flag('nobeh');
const AUTH = flag('auth');
const PASSWORD = flag('password');

function resolveModule(name) {
  const dirs = [process.cwd(), ...(process.env.NODE_PATH ?? '').split(path.delimiter).filter(Boolean)];
  for (const dir of dirs) {
    try {
      const base = dir.endsWith('node_modules') ? dir : path.join(dir, 'node_modules');
      return createRequire(path.join(base, '_.js')).resolve(name);
    } catch (e) {
      /* try the next directory */
    }
  }
  return null;
}

let chromium;
try {
  ({ chromium } = await import('playwright'));
} catch {
  const found = resolveModule('playwright');
  if (found) ({ chromium } = createRequire(found)('playwright'));
}
if (!chromium) {
  console.error('playwright is not installed. Install it with:\n  npm install -g playwright axe-core && npx playwright install chromium');
  process.exit(2);
}
const axePath = resolveModule('axe-core/axe.min.js');
if (!axePath) {
  console.error('axe-core is not installed. Install it with:\n  npm install -g axe-core');
  process.exit(2);
}

const problems = [];
const notes = [];
const fail = (where, msg) => problems.push(`[${where}] ${msg}`);

const browser = await chromium.launch();

// A timeout or a thrown assertion halfway through is a failure too: report what
// was found so far, then the error, instead of a bare stack trace.
process.on('uncaughtException', (e) => {
  for (const p of [...new Set(problems)]) console.error(p);
  console.error(`check aborted: ${String(e.message).split('\n')[0]}`);
  process.exit(1);
});

/** Collects everything that should never happen on a healthy load. */
function watch(page, where, { allow = () => false } = {}) {
  page.on('pageerror', (e) => fail(where, `page error: ${e.message}`));
  page.on('console', (m) => {
    if (['error', 'warning'].includes(m.type()) && !allow(m.text())) fail(where, `console ${m.type()}: ${m.text()}`);
  });
  page.on('requestfailed', (r) => {
    if (r.failure()?.errorText !== 'net::ERR_ABORTED') fail(where, `request failed: ${r.url()}`);
  });
  page.on('response', (r) => {
    if (r.status() >= 400 && !allow(`HTTP ${r.status()} ${r.url()}`)) fail(where, `HTTP ${r.status()} ${r.url()}`);
  });
}

async function newPage(opts = {}) {
  const ctx = await browser.newContext({
    viewport: opts.viewport ?? { width: 1440, height: 900 },
    colorScheme: opts.theme ?? 'light',
    permissions: ['clipboard-read', 'clipboard-write'],
  });
  // An init script is not subject to the page's CSP, so axe loads without
  // weakening the policy this check is also verifying.
  await ctx.addInitScript({ path: axePath });
  await ctx.addInitScript(() => {
    window.__cls = 0;
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) if (!e.hadRecentInput) window.__cls += e.value;
    }).observe({ type: 'layout-shift', buffered: true });
  });
  const page = await ctx.newPage();
  return { ctx, page };
}

/** Scroll the whole page so every lazily loaded panel is asked for, then wait for quiet. */
async function loadEverything(page) {
  await page.waitForSelector('#main', { timeout: 20000 });
  await page.waitForSelector('.metric-card, .placeholder, .banner-error', { timeout: 20000 });
  // The page grows as panels render (slowly, when requests are being retried),
  // so keep sweeping top to bottom until nothing is left loading.
  for (let pass = 0; pass < 12; pass += 1) {
    for (let y = 0; y < 40000; y += 500) {
      await page.evaluate((top) => window.scrollTo(0, top), y);
      await page.waitForTimeout(120);
      if (y > (await page.evaluate(() => document.documentElement.scrollHeight))) break;
    }
    await page.waitForLoadState('networkidle');
    await page.waitForTimeout(300);
    if (!(await page.evaluate(() => document.querySelector('.skeleton, [aria-busy="true"]')))) break;
  }
  await page.waitForFunction(() => !document.querySelector('.skeleton, [aria-busy="true"]'), null, { timeout: 30000 }).catch(async (e) => {
    const stuck = await page.evaluate(() => [...document.querySelectorAll('.skeleton, [aria-busy="true"]')].map((el) => el.closest('section')?.id ?? el.className));
    throw new Error(`panels still loading after 30 s: ${[...new Set(stuck)].join(', ')} (${e.message.split('\n')[0]})`);
  });
  await page.evaluate(() => window.scrollTo(0, 0));
}

const KEY_PANELS = ['Overview', 'Right now', 'Pages', 'Acquisition', 'Locations', 'Devices', 'Goals & custom events', 'Revenue', 'Funnel', 'Retention cohorts', 'Sequence', 'Flow'];

async function assertCleanContent(page, where) {
  const text = await page.locator('body').innerText();
  for (const bad of ['undefined', 'NaN', '[object', 'null%', '{"', 'HTTP 4', 'HTTP 5']) {
    if (text.includes(bad)) fail(where, `page text contains "${bad}"`);
  }
}

async function assertNoOverflow(page, where) {
  const over = await page.evaluate(() => ({ sw: document.documentElement.scrollWidth, iw: window.innerWidth }));
  if (over.sw > over.iw + 1) fail(where, `page scrolls sideways: scrollWidth ${over.sw} > viewport ${over.iw}`);
}

async function runAxe(page, where) {
  const result = await page.evaluate(async () => {
    const r = await window.axe.run(document, { runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa', 'best-practice'] } });
    return r.violations.map((v) => ({ id: v.id, impact: v.impact, help: v.help, nodes: v.nodes.slice(0, 3).map((n) => `${n.target.join(' ')} :: ${(n.failureSummary || '').split('\n').slice(0, 2).join(' ').slice(0, 160)}`), count: v.nodes.length }));
  });
  for (const v of result) {
    if (v.impact === 'serious' || v.impact === 'critical') fail(where, `axe ${v.impact} ${v.id} (${v.count}x): ${v.help}; e.g. ${v.nodes[0]}`);
    else notes.push(`[${where}] axe ${v.impact} ${v.id} (${v.count}x): ${v.help}`);
  }
}

/** Tab through the page: skip link first, every stop visible with an indicator, no trap. */
async function checkKeyboard(page, where) {
  await page.evaluate(() => { window.scrollTo(0, 0); document.activeElement?.blur(); });
  const stops = [];
  for (let i = 0; i < 40; i += 1) {
    await page.keyboard.press('Tab');
    const info = await page.evaluate(() => {
      const el = document.activeElement;
      if (!el || el === document.body) return { body: true };
      const r = el.getBoundingClientRect();
      const cs = getComputedStyle(el);
      const indicator = (cs.outlineStyle !== 'none' && parseFloat(cs.outlineWidth) > 0) || cs.boxShadow !== 'none';
      return { name: `${el.tagName.toLowerCase()}${el.id ? `#${el.id}` : ''}.${el.className || ''}`.slice(0, 60), w: r.width, h: r.height, indicator, label: (el.getAttribute('aria-label') || el.innerText || el.value || '').trim().slice(0, 40) };
    });
    if (info.body) break;
    stops.push(info);
  }
  if (stops.length < 8) fail(where, `only ${stops.length} keyboard stops reached; focus left the page early`);
  if (stops[0] && !/skip/i.test(stops[0].label)) fail(where, `first Tab stop is "${stops[0].label}", not the skip link`);
  for (const s of stops) {
    if (s.w === 0 || s.h === 0) fail(where, `keyboard stop with no size: ${s.name}`);
    if (!s.indicator) fail(where, `no visible focus indicator on ${s.name} "${s.label}"`);
  }
  const names = stops.map((s) => s.name);
  if (new Set(names).size < Math.min(names.length, 6)) fail(where, 'Tab appears to be trapped on a few elements');
}

/** Cold load of one theme/viewport. */
async function coldLoad(label, { viewport, theme, url = BASE, query = '', expectPanels = true, allow }) {
  const { ctx, page } = await newPage({ viewport, theme });
  const where = label;
  watch(page, where, { allow });
  const t0 = Date.now();
  await page.goto(`${url}/${query}`, { waitUntil: 'load', timeout: 30000 });
  await loadEverything(page);
  const perf = await page.evaluate(() => {
    const paint = Object.fromEntries(performance.getEntriesByType('paint').map((e) => [e.name, Math.round(e.startTime)]));
    const nav = performance.getEntriesByType('navigation')[0];
    return { fcp: paint['first-contentful-paint'], dcl: Math.round(nav.domContentLoadedEventEnd), cls: window.__cls, api: window.__mallardApi, requests: performance.getEntriesByType('resource').filter((r) => r.name.includes('/api/')).length };
  });
  notes.push(`[${where}] first contentful paint ${perf.fcp} ms, DOMContentLoaded ${perf.dcl} ms, layout shift ${perf.cls.toFixed(3)}, ${perf.requests} API requests (peak in flight ${perf.api?.peak}, 429 retries ${perf.api?.retried}), all loaded in ${Date.now() - t0} ms`);
  if (perf.cls > 0.1) fail(where, `cumulative layout shift ${perf.cls.toFixed(3)} exceeds 0.1`);
  if (perf.api && perf.api.retried > 0) fail(where, `a cold load needed ${perf.api.retried} 429 retries; the client should stay under the server's concurrency cap`);
  if (perf.api && perf.api.peak > 4) fail(where, `${perf.api.peak} requests were in flight at once; the cap is 4`);
  if (expectPanels) {
    const headings = await page.locator('h2').allInnerTexts();
    for (const h of KEY_PANELS) if (!headings.some((t) => t.trim() === h)) fail(where, `panel "${h}" is missing`);
  }
  await assertCleanContent(page, where);
  await assertNoOverflow(page, where);
  await runAxe(page, where);
  await checkKeyboard(page, where);
  await ctx.close();
}

/* ── 1. Cold loads across themes and viewports ──────────────────────────── */

const VIEWPORTS = { desktop: { width: 1440, height: 900 }, tablet: { width: 820, height: 1180 }, mobile: { width: 390, height: 844 } };
for (const theme of ['light', 'dark']) {
  for (const [name, viewport] of Object.entries(VIEWPORTS)) {
    await coldLoad(`${name}/${theme}`, { viewport, theme });
  }
}

/* ── 2. Behaviour on the populated instance ─────────────────────────────── */

{
  const { ctx, page } = await newPage();
  const where = 'behaviour';
  watch(page, where);
  await page.goto(BASE, { waitUntil: 'load' });
  await loadEverything(page);

  // Filter chips: click a row, expect a chip and a shareable URL; toggle; remove.
  const rows = await page.locator('.bar-link').count();
  if (rows === 0) {
    notes.push('[behaviour] no breakdown rows to click: seed some events for the interaction checks');
  } else {
    const rowText = (await page.locator('.bar-link').first().innerText()).trim();
    await page.locator('.bar-link').first().click();
    await page.waitForSelector('.filter-chip');
    const search = await page.evaluate(() => location.search);
    if (!search.includes('filters=')) fail(where, `filter chip did not reach the URL (${search})`);
    if (!(await page.locator('.filter-chip-value').first().innerText()).includes(rowText.slice(0, 20))) fail(where, 'chip does not show the clicked value');
    // The URL alone must reproduce the view.
    await page.reload({ waitUntil: 'load' });
    await page.waitForSelector('.filter-chip');
    await page.locator('.filter-chip-op').first().click();
    await page.waitForFunction(() => /is not/.test(document.querySelector('.filter-chip-op')?.textContent || ''));
    if (!decodeURIComponent(await page.evaluate(() => location.search)).includes('!=')) fail(where, 'toggling is/is not did not reach the URL');
    await page.locator('.filter-chip-remove').first().click();
    await page.waitForFunction(() => !document.querySelector('.filter-chip'));
    if ((await page.evaluate(() => location.search)).includes('filters=')) fail(where, 'removing the chip left filters in the URL');
    await page.goBack();
    await page.waitForSelector('.filter-chip', { timeout: 5000 }).catch(() => fail(where, 'Back did not restore the filtered view'));
    await page.goto(BASE, { waitUntil: 'load' });
    await loadEverything(page);
  }

  // Comparison toggle has a visible, announced state.
  await page.getByRole('button', { name: 'Previous period' }).click();
  await page.waitForSelector('.comparison-note');
  if ((await page.getByRole('button', { name: 'Previous period' }).getAttribute('aria-pressed')) !== 'true') fail(where, 'comparison toggle is not aria-pressed when on');
  if (!(await page.evaluate(() => location.search)).includes('compare=previous_period')) fail(where, 'comparison mode is not in the URL');
  await page.getByRole('button', { name: 'Off' }).click();

  // Custom range: invalid input is explained beside the field, valid input applies.
  await page.getByRole('button', { name: 'Custom' }).click();
  await page.getByLabel('From', { exact: true }).fill('2026-02-10');
  await page.getByLabel('To', { exact: true }).fill('2026-02-01');
  await page.getByRole('button', { name: 'Apply range' }).click();
  const err = (await page.locator('#range-error').innerText()).trim();
  if (!/start date must be/i.test(err)) fail(where, `inverted range gave "${err}" instead of an explanation`);
  await page.getByRole('button', { name: 'Last month' }).click();
  await page.waitForFunction(() => location.search.includes('start='));
  if (!(await page.evaluate(() => location.search)).includes('end=')) fail(where, 'custom range is not in the URL');
  await page.getByRole('button', { name: '30D' }).click();
  await page.waitForFunction(() => !location.search.includes('start='));

  // Tabs: arrow keys move between tabs and the panel follows.
  const tab = page.getByRole('tab', { name: 'Top pages' });
  await tab.focus();
  await page.keyboard.press('ArrowRight');
  if (!(await page.getByRole('tab', { name: 'Entry pages' }).getAttribute('aria-selected')).includes('true')) fail(where, 'ArrowRight did not select the next tab');
  await page.keyboard.press('Home');
  await page.getByRole('tab', { name: 'Top pages' }).click();

  // The chart has a text alternative.
  await page.getByRole('button', { name: 'View as table' }).click();
  if ((await page.locator('.chart-table tbody tr').count()) < 2) fail(where, 'chart table alternative is empty');
  const summary = await page.locator('#chart-summary').innerText();
  if (!/pageviews in total/.test(summary)) fail(where, 'chart has no textual summary');

  // Export downloads a real file through fetch, so a failure can be explained.
  const [download] = await Promise.all([page.waitForEvent('download', { timeout: 15000 }).catch(() => null), page.getByRole('button', { name: 'Daily CSV' }).click()]);
  if (!download) fail(where, 'Daily CSV produced no download');
  else if (!/\.csv$/.test(download.suggestedFilename())) fail(where, `export saved as "${download.suggestedFilename()}"`);

  // Copy-to-clipboard for the tracking snippet.
  await page.getByRole('button', { name: 'Install snippet' }).click();
  await page.getByRole('button', { name: 'Copy snippet' }).click();
  const clip = await page.evaluate(() => navigator.clipboard.readText()).catch(() => '');
  if (!clip.includes('/mallard.js') || !clip.includes('data-domain')) fail(where, `copied snippet was "${clip.slice(0, 60)}"`);
  await page.keyboard.press('Escape');

  await ctx.close();
}

/* ── 3. Failure behaviour: the UI must degrade per panel, never show raw JSON ── */

{
  const { ctx, page } = await newPage();
  const where = 'failure/429-retry';
  // The first request to each report is refused with 429; the client must retry and succeed.
  const seen = new Set();
  await page.route('**/api/stats/**', (route) => {
    const key = new URL(route.request().url()).pathname;
    if (!seen.has(key) && !key.endsWith('/export')) {
      seen.add(key);
      return route.fulfill({ status: 429, headers: { 'retry-after': '0', 'content-type': 'application/json' }, body: '{"error":"too many concurrent queries"}' });
    }
    return route.continue();
  });
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  await page.goto(BASE, { waitUntil: 'load' });
  await loadEverything(page);
  const text = await page.locator('body').innerText();
  if (/too many concurrent|HTTP 429|busy/i.test(text)) fail(where, 'a 429 reached the screen instead of being retried');
  if ((await page.locator('.placeholder-error').count()) > 0) fail(where, 'a panel is stuck in an error state after retryable 429s');
  const stats = await page.evaluate(() => window.__mallardApi);
  if (!stats || stats.retried === 0) fail(where, 'no retries were recorded, so the 429 path was not exercised');
  for (const e of errors) fail(where, `page error: ${e}`);
  await ctx.close();
}

{
  const { ctx, page } = await newPage();
  const where = 'failure/500';
  await page.route('**/api/stats/breakdown/pages**', (route) => route.fulfill({ status: 500, contentType: 'application/json', body: '{"sql":"SELECT secret FROM events","detail":{"nested":true}}' }));
  await page.route('**/api/stats/goals**', (route) => route.fulfill({ status: 502, contentType: 'text/html', body: '<html>Bad gateway</html>' }));
  await page.route('**/api/stats/revenue**', (route) => route.fulfill({ status: 200, contentType: 'application/json', body: '{"unexpected":"shape"}' }));
  // Failed resources are logged by the browser itself; those are the point of this test.
  page.on('pageerror', (e) => fail(where, `page error: ${e.message}`));
  await page.goto(BASE, { waitUntil: 'load' });
  await loadEverything(page);
  const text = await page.locator('body').innerText();
  if (text.includes('"sql"') || text.includes('SELECT secret') || text.includes('<html>')) fail(where, 'raw response body leaked into the page');
  if ((await page.locator('.placeholder-error').count()) < 2) fail(where, 'failed panels did not show an error state');
  if ((await page.getByRole('button', { name: 'Retry' }).count()) < 1) fail(where, 'failed panels offer no Retry button');
  // The rest of the dashboard must still be usable.
  if (!(await page.locator('.metric-value').first().innerText()).match(/\d/)) fail(where, 'one failing panel took down the headline figures');
  await ctx.close();
}

{
  const { ctx, page } = await newPage();
  const where = 'failure/offline';
  page.on('pageerror', (e) => fail(where, `page error: ${e.message}`));
  await page.goto(BASE, { waitUntil: 'load' });
  await loadEverything(page);
  await page.route('**/api/**', (route) => route.abort('connectionrefused'));
  await page.getByRole('button', { name: 'Refresh' }).click();
  await page.waitForSelector('.banner-error', { timeout: 15000 }).catch(() => fail(where, 'no offline banner after the server became unreachable'));
  const text = await page.locator('body').innerText();
  if (!/cannot reach the server/i.test(text)) fail(where, 'offline state is not explained');
  for (const bad of ['undefined', 'NaN', 'Failed to fetch', 'TypeError']) if (text.includes(bad)) fail(where, `offline page shows "${bad}"`);
  await page.unroute('**/api/**');
  await page.getByRole('button', { name: 'Retry now' }).click();
  await page.waitForFunction(() => !document.querySelector('.banner-error'), null, { timeout: 15000 }).catch(() => fail(where, 'dashboard did not recover once the server returned'));
  await ctx.close();
}

/* ── 4. Optional instances ──────────────────────────────────────────────── */

if (EMPTY) {
  const { ctx, page } = await newPage();
  const where = 'empty-instance';
  watch(page, where);
  await page.goto(EMPTY, { waitUntil: 'load' });
  await page.waitForSelector('#main');
  await page.waitForLoadState('networkidle');
  const text = await page.locator('body').innerText();
  if (!/Welcome to Mallard Metrics/.test(text)) fail(where, 'an empty instance shows no welcome panel');
  if (!text.includes('/mallard.js')) fail(where, 'the empty state does not show the tracking snippet');
  await assertCleanContent(page, where);
  await runAxe(page, where);
  await ctx.close();
}

if (NOBEH) {
  const { ctx, page } = await newPage();
  const where = 'no-behavioral';
  watch(page, where);
  await page.goto(NOBEH, { waitUntil: 'load' });
  await loadEverything(page);
  const text = await page.locator('body').innerText();
  if (!/Behavioral analytics is unavailable/.test(text)) fail(where, 'missing the behavioral-unavailable explanation');
  if (!/MALLARD_EXTENSION_DIR/.test(await page.locator('body').innerHTML())) fail(where, 'explanation does not say how to fix it');
  await assertCleanContent(page, where);
  await runAxe(page, where);
  await ctx.close();
}

if (AUTH && PASSWORD) {
  const { ctx, page } = await newPage();
  const where = 'auth-instance';
  // The very first /api/auth/status or stats probe may legitimately answer 401.
  watch(page, where, { allow: (t) => /401/.test(t) });
  await page.goto(AUTH, { waitUntil: 'load' });
  await page.waitForSelector('input[type=password]');
  if (!(await page.evaluate(() => document.activeElement?.type === 'password'))) fail(where, 'the password field is not focused on the sign-in form');
  await runAxe(page, `${where}/login`);
  await page.fill('input[type=password]', 'wrong-password');
  await page.click('button[type=submit]');
  await page.waitForSelector('.auth-error', { timeout: 10000 }).catch(() => fail(where, 'a wrong password shows no error'));
  await page.fill('input[type=password]', PASSWORD);
  await page.click('button[type=submit]');
  await page.waitForSelector('#main', { timeout: 15000 });
  await loadEverything(page);
  await page.getByRole('button', { name: 'API keys' }).click();
  await page.getByLabel('Name', { exact: true }).fill('browser-check');
  await page.getByRole('button', { name: 'Create key' }).click();
  await page.waitForSelector('.key-value', { timeout: 10000 }).catch(() => fail(where, 'creating an API key showed no key'));
  await runAxe(page, `${where}/keys`);
  page.once('dialog', (d) => d.accept());
  await page.getByRole('button', { name: /^Revoke/ }).first().click();
  await ctx.close();
}

await browser.close();

for (const n of notes) console.log(`note: ${n}`);
if (problems.length === 0) {
  console.log('dashboard: rendered, interacted and failed gracefully, with no regressions');
  process.exit(0);
}
for (const p of [...new Set(problems)]) console.error(p);
console.error(`\n${new Set(problems).size} problem(s)`);
process.exit(1);
