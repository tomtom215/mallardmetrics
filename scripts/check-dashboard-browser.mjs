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
// It also drives the states the server's error `code` selects (a query
// timeout, a busy server, session data that gdpr_mode rounds away), retention
// with unfinished weeks, "Show more" on a breakdown, entry-page columns, the
// figure definitions, a single offline banner, and that a repeat visit makes
// no request for the dashboard's own files.
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
//   --gdpr=URL      an instance running with gdpr_mode (rounded timestamps)
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
const GDPR = flag('gdpr');
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
  // One banner, not one error per panel.
  const sentences = (text.match(/cannot reach the server/gi) ?? []).length;
  if (sentences !== 1) fail(where, `"Cannot reach the server" appears ${sentences} times; expected one banner`);
  if ((await page.locator('.placeholder-error').count()) > 0) fail(where, 'panels still show their own error while the server is offline');
  if ((await page.getByRole('button', { name: /^(Retry|Try again)$/ }).count()) > 0) fail(where, 'panels offer their own Retry while the server is offline');
  if ((await page.locator('.placeholder-offline').count()) < 3) fail(where, 'offline panels are not collapsed to a line');
  await page.unroute('**/api/**');
  await page.getByRole('button', { name: 'Retry now' }).click();
  await page.waitForFunction(() => !document.querySelector('.banner-error'), null, { timeout: 15000 }).catch(() => fail(where, 'dashboard did not recover once the server returned'));
  await ctx.close();
}

/* ── 3b. States chosen by the server's error code ───────────────────────── */

const json = (status, body, headers = {}) => ({ status, contentType: 'application/json', headers, body: JSON.stringify(body) });

{
  const { ctx, page } = await newPage();
  const where = 'failure/timeout-and-busy';
  page.on('pageerror', (e) => fail(where, `page error: ${e.message}`));
  await page.route('**/api/stats/goals**', (r) => r.fulfill(json(504, { error: 'The query did not finish within 30 seconds. Try a shorter date range or fewer filters.', code: 'query_timeout' })));
  await page.route('**/api/stats/revenue**', (r) => r.fulfill(json(429, { error: 'too many concurrent queries', code: 'too_many_requests' }, { 'retry-after': '0' })));
  await page.goto(BASE, { waitUntil: 'load' });
  await loadEverything(page);
  const goals = await page.locator('#goals').innerText();
  if (!/took too long/i.test(goals) || !/shorter date range/i.test(goals)) fail(where, 'a query timeout is not explained with a remedy');
  if (/HTTP 504|30 seconds/.test(goals)) fail(where, 'a query timeout shows the status or the server wording');
  if ((await page.locator('#goals').getByRole('button', { name: 'Try again' }).count()) !== 1) fail(where, 'a query timeout offers no retry');
  if ((await page.locator('#goals .placeholder-error').count()) > 0) fail(where, 'a timeout is styled as a fault');
  const revenue = await page.locator('#revenue').innerText();
  if (!/busy/i.test(revenue) || /HTTP 429/.test(revenue)) fail(where, 'a persistently busy server is not explained');
  // Retry really retries.
  await page.unroute('**/api/stats/goals**');
  await page.locator('#goals').getByRole('button', { name: 'Try again' }).click();
  await page.waitForFunction(() => !/took too long/.test(document.getElementById('goals')?.innerText ?? ''), null, { timeout: 15000 }).catch(() => fail(where, 'Try again did not reload the panel'));
  await ctx.close();
}

{
  // Session data rounded away by gdpr_mode is information, not an error.
  const { ctx, page } = await newPage();
  const where = 'gdpr-mocked';
  const asked = [];
  page.on('request', (r) => asked.push(new URL(r.url()).pathname));
  page.on('pageerror', (e) => fail(where, `page error: ${e.message}`));
  await page.route('**/api/stats/main**', async (route) => {
    const res = await route.fetch();
    const body = await res.json();
    Object.assign(body, { behavioral_available: false, sessions_unavailable_reason: 'timestamps_rounded', total_sessions: null, bounce_rate: null, avg_visit_duration_secs: null, views_per_visit: null });
    await route.fulfill({ response: res, json: body });
  });
  await page.goto(BASE, { waitUntil: 'load' });
  await loadEverything(page);
  const text = await page.locator('body').innerText();
  if (!/gdpr_mode/.test(text) || !/rounds? away|rounded to the hour/i.test(text)) fail(where, 'rounded timestamps are not explained');
  if ((await page.locator('.placeholder-error').count()) > 0) fail(where, 'gdpr_mode is presented as an error');
  if ((await page.locator('.placeholder-unavailable').count()) > 0) fail(where, 'gdpr_mode is presented with the missing-extension warning');
  if (/MALLARD_EXTENSION_DIR/.test(await page.locator('body').innerHTML())) fail(where, 'gdpr_mode tells people to fix the extension');
  for (const gone of ['/api/stats/funnel', '/api/stats/sequences', '/api/stats/flow']) if (asked.includes(gone)) fail(where, `${gone} was requested although sessions are unavailable`);
  if (asked.includes('/health/detailed')) fail(where, 'the cause was inferred from /health/detailed instead of read from the response');
  // The panels beside the overview must not repeat its long explanation.
  const long = (text.match(/Timestamps are rounded to the hour, so visits cannot be reconstructed/g) ?? []).length;
  if (long < 3) fail(where, `the funnel, sequence and flow panels should each show the compact text (found ${long})`);
  if ((text.match(/With gdpr_mode every timestamp is rounded/g) ?? []).length !== 1) fail(where, 'the long gdpr_mode explanation is not shown exactly once');
  if ((await page.locator('#retention .retention-table').count()) !== 1) fail(where, 'weekly retention, which gdpr_mode does not affect, is not shown');
  await assertCleanContent(page, where);
  await runAxe(page, where);
  await ctx.close();
}

{
  // The cause comes from the response alone: a 503's `code` on a panel, and
  // `sessions_unavailable_reason` on /main. An older server that sends
  // neither gets the generic explanation, not a guess.
  const cases = [
    ['code-rounded', { behavioral_available: true }, { error: 'rounded', code: 'session_data_unavailable' }, /rounded to the hour/, /MALLARD_EXTENSION_DIR/],
    ['code-extension', { behavioral_available: true }, { error: 'no extension', code: 'behavioral_unavailable' }, /Behavioral analytics is unavailable/, /rounded to the hour/],
    ['reason-extension', { behavioral_available: false, sessions_unavailable_reason: 'extension_missing', total_sessions: null }, null, /MALLARD_EXTENSION_DIR/, /rounded to the hour/],
    ['reason-absent', { behavioral_available: false, total_sessions: null }, null, /MALLARD_EXTENSION_DIR/, /rounded to the hour/],
  ];
  for (const [label, mainPatch, panel503, expected, forbidden] of cases) {
    const { ctx, page } = await newPage();
    const where = `unavailable-cause/${label}`;
    const asked = [];
    page.on('request', (r) => asked.push(new URL(r.url()).pathname));
    page.on('pageerror', (e) => fail(where, `page error: ${e.message}`));
    await page.route('**/api/stats/main**', async (route) => {
      const res = await route.fetch();
      await route.fulfill({ response: res, json: { ...(await res.json()), ...mainPatch } });
    });
    if (panel503) await page.route('**/api/stats/funnel**', (r) => r.fulfill(json(503, panel503)));
    await page.goto(BASE, { waitUntil: 'load' });
    await loadEverything(page);
    const scope = panel503 ? await page.locator('#funnel').innerHTML() : await page.locator('body').innerHTML();
    if (!expected.test(scope)) fail(where, `the explanation does not match the cause (${expected})`);
    if (forbidden.test(scope)) fail(where, `the explanation names the wrong cause (${forbidden})`);
    if (asked.includes('/health/detailed')) fail(where, 'the cause was inferred from /health/detailed');
    await ctx.close();
  }
}

{
  const retention = (identity, cohorts) => ({ identity_supports_cohorts: identity, visitor_salt_rotation_days: identity ? 30 : 7, caveat: identity ? null : 'wording the dashboard must not parse', cohorts });
  const cohorts = [
    { cohort_date: '2026-09-28', cohort_size: 120, retained: [120, 54, 31, 22], retention_rates: [1, 0.45, 0.2583, 0.1833], complete_weeks: 4, partial_first_week: true },
    { cohort_date: '2026-10-05', cohort_size: 98, retained: [98, 41, 27, 0], retention_rates: [1, 0.4184, 0.2755, 0], complete_weeks: 3, partial_first_week: false },
    { cohort_date: '2026-10-19', cohort_size: 37, retained: [37, 0, 0, 0], retention_rates: [1, 0, 0, 0], complete_weeks: 0, partial_first_week: false },
  ];
  for (const [name, viewport] of [['desktop', VIEWPORTS.desktop], ['mobile', VIEWPORTS.mobile]]) {
    const { ctx, page } = await newPage({ viewport });
    const where = `retention/${name}`;
    watch(page, where);
    await page.route('**/api/stats/retention**', (r) => r.fulfill(json(200, retention(true, cohorts))));
    await page.goto(BASE, { waitUntil: 'load' });
    await loadEverything(page);
    const cells = await page.locator('.retention-table tbody td').allInnerTexts();
    // Unfinished weeks are dashes: 3 in the third cohort's rows plus 1 in the second.
    if (cells.filter((c) => c.trim().startsWith('–')).length !== 4) fail(where, `expected 4 unfinished-week dashes, got ${cells.filter((c) => c.trim().startsWith('–')).length}`);
    if (cells.filter((c) => c.trim() === '0%').length > 0) fail(where, 'an unfinished week is printed as 0%');
    if ((await page.locator('.retention-table .tag').count()) !== 1) fail(where, 'the partial first week is not labelled');
    if (!/has not finished yet/.test(await page.locator('#retention').innerText())) fail(where, 'the dashes are not explained');
    if (name === 'mobile') {
      const scroll = await page.locator('.retention-scroll').evaluate((el) => ({ sw: el.scrollWidth, cw: el.clientWidth, tab: el.tabIndex }));
      if (!(scroll.sw > scroll.cw)) fail(where, 'the retention table does not scroll sideways on a phone');
      if (scroll.tab !== 0) fail(where, 'the scrolling retention table cannot be focused from the keyboard');
      await assertNoOverflow(page, where);
    }
    await runAxe(page, where);
    await ctx.close();
  }

  const { ctx, page } = await newPage();
  const where = 'retention/identity-caveat';
  watch(page, where);
  await page.route('**/api/stats/retention**', (r) => r.fulfill(json(200, retention(false, cohorts))));
  await page.goto(BASE, { waitUntil: 'load' });
  await loadEverything(page);
  const note = await page.locator('#retention .notice').innerText();
  if (!/visitor_salt_rotation_days/.test(note) || !/at least 21/.test(note)) fail(where, 'the caveat does not name the setting and the value to use');
  if (!/every 7 days/.test(note)) fail(where, 'the caveat does not state the rotation period the server reported');
  if (/wording the dashboard must not parse/.test(note)) fail(where, "the server's caveat wording is displayed verbatim");
  if ((await page.locator('#retention .placeholder-error, #retention .notice[role="alert"]').count()) > 0) fail(where, 'the caveat is styled as an error');
  const dashes = (await page.locator('.retention-table tbody td').allInnerTexts()).filter((c) => c.trim().startsWith('–')).length;
  if (dashes !== 9) fail(where, `weeks that cannot be measured should be dashes, got ${dashes}`);
  await runAxe(page, where);
  await ctx.close();
}

{
  // The site selector: busiest site first and selected, with its 30-day count; a URL that names a site wins.
  const { ctx, page } = await newPage();
  const where = 'site-selector';
  watch(page, where);
  await page.route('**/api/sites', (r) => r.fulfill(json(200, { sites: ['busy.example', 'quiet.example', 'idle.example'], details: [{ site_id: 'busy.example', events_30d: 18423 }, { site_id: 'quiet.example', events_30d: 12 }, { site_id: 'idle.example', events_30d: 0 }] })));
  await page.goto(BASE, { waitUntil: 'load' });
  await page.waitForSelector('select');
  const options = await page.locator('.toolbar select option').allInnerTexts();
  if (options.length !== 3 || !/^busy\.example\b.*18,423 events \(30d\)/.test(options[0]) || !/idle\.example.*0 events/.test(options[2])) fail(where, `options do not show the 30-day counts: ${JSON.stringify(options)}`);
  if ((await page.locator('.toolbar select').inputValue()) !== 'busy.example') fail(where, 'the busiest site is not selected by default');
  await ctx.close();
  const named = await newPage();
  watch(named.page, `${where}/url`);
  await named.page.route('**/api/sites', (r) => r.fulfill(json(200, { sites: ['busy.example', 'quiet.example'], details: [{ site_id: 'busy.example', events_30d: 5 }, { site_id: 'quiet.example', events_30d: 1 }] })));
  await named.page.goto(`${BASE}/?site=quiet.example`, { waitUntil: 'load' });
  await named.page.waitForSelector('select');
  if ((await named.page.locator('.toolbar select').inputValue()) !== 'quiet.example') fail(where, 'a site named in the URL does not win over the busiest');
  await named.ctx.close();
}

{
  const { ctx, page } = await newPage();
  const where = 'breakdowns';
  watch(page, where);
  const asked = [];
  page.on('request', (r) => asked.push(r.url()));
  await page.goto(BASE, { waitUntil: 'load' });
  await loadEverything(page);
  if (!asked.some((u) => /breakdown\/pages\?.*limit=10(&|$)/.test(u))) fail(where, 'the first request for a breakdown does not ask for 10 rows');
  const more = page.locator('#bd-pages').getByRole('button', { name: /Show more/ });
  if ((await more.count()) === 0) {
    notes.push('[breakdowns] the demo site has fewer than 10 pages: Show more was not exercised');
  } else {
    await more.click();
    await page.waitForFunction(() => document.querySelectorAll('#bd-pages tbody tr').length > 10, null, { timeout: 15000 }).catch(() => fail(where, 'Show more did not add rows'));
    if (!asked.some((u) => /breakdown\/pages\?.*limit=50(&|$)/.test(u))) fail(where, 'Show more did not ask the server for more rows');
    if ((await page.locator('#bd-pages .breakdown-scroll').count()) !== 1) fail(where, 'a long list does not scroll inside its panel');
    await assertNoOverflow(page, where);
    await runAxe(page, `${where}/long-list`);
    await page.locator('#bd-pages').getByRole('button', { name: /Show top 10/ }).click();
    await page.waitForFunction(() => document.querySelectorAll('#bd-pages tbody tr').length <= 10, null, { timeout: 15000 }).catch(() => fail(where, 'Show top 10 did not collapse the list'));
  }
  // Entry pages count visits that began there, in their own column.
  await page.getByRole('tab', { name: 'Entry pages' }).click();
  await page.waitForFunction(() => /Entrances/.test(document.getElementById('bd-pages')?.innerText ?? ''), null, { timeout: 15000 }).catch(() => fail(where, 'entry pages do not show an Entrances column'));
  await page.getByRole('tab', { name: 'Exit pages' }).click();
  await page.waitForFunction(() => /\bExits\b/.test(document.getElementById('bd-pages')?.innerText ?? ''), null, { timeout: 15000 }).catch(() => fail(where, 'exit pages do not show an Exits column'));
  if ((await page.locator('#bd-pages .bar-link').count()) > 0) fail(where, 'exit pages offer a filter the server rejects');
  await ctx.close();
}

{
  const { ctx, page } = await newPage();
  const where = 'definitions-and-periods';
  watch(page, where);
  await page.goto(BASE, { waitUntil: 'load' });
  await loadEverything(page);
  await page.locator('.definitions summary').click();
  const defs = await page.locator('.definitions').innerText();
  for (const term of ['Bounce rate', 'Visit duration', 'Views per visit', 'Entry and exit pages', 'Filters']) if (!defs.includes(term)) fail(where, `the definitions omit "${term}"`);
  if (!/exactly one event/.test(defs) || !/at least one pageview/.test(defs)) fail(where, 'the bounce and visit definitions do not match the server');
  await page.getByRole('button', { name: '7D' }).click();
  await page.waitForFunction(() => /Last 7 days, including today/.test(document.querySelector('.range-summary')?.innerText ?? ''));
  const summary = await page.locator('.range-summary').innerText();
  const m = /(\w{3} \d+, \d{4}) – (\w{3} \d+, \d{4})/.exec(summary);
  if (!m) fail(where, `7D shows no dates: "${summary}"`);
  else if (Math.round((Date.parse(`${m[2]} UTC`) - Date.parse(`${m[1]} UTC`)) / 86400000) !== 6) fail(where, `7D should span seven days including today, got "${summary}"`);
  const chartDays = await page.locator('.chart-readout').count();
  void chartDays;
  await page.getByRole('button', { name: 'View as table' }).click();
  const rows = await page.locator('.chart-table tbody tr').count();
  if (rows !== 7) fail(where, `7D chart has ${rows} points; expected 7`);
  await ctx.close();
}

{
  // The panel for live activity says so when nobody is there, and never "No data for this period".
  const { ctx, page } = await newPage();
  const where = 'realtime';
  watch(page, where);
  await page.goto(BASE, { waitUntil: 'load' });
  await loadEverything(page);
  const live = await page.locator('#realtime').innerText();
  if (!/current visitors|No one is on/.test(live)) fail(where, `the realtime panel says "${live.slice(0, 80)}"`);
  if (/No data for this period/.test(live)) fail(where, 'the realtime panel uses the date-range wording');
  await ctx.close();
}

{
  // A repeat visit takes the dashboard's own files from the browser cache.
  const { ctx, page } = await newPage();
  const where = 'caching';
  await page.goto(BASE, { waitUntil: 'load' });
  await loadEverything(page);
  await page.reload({ waitUntil: 'load' });
  await loadEverything(page);
  const assets = await page.evaluate(() =>
    performance.getEntriesByType('resource').filter((e) => /\.(js|css|svg)(\?|$)/.test(e.name) && !e.name.includes('/api/')).map((e) => ({ name: new URL(e.name).pathname + new URL(e.name).search, transferSize: e.transferSize })));
  if (assets.length < 6) fail(where, `only ${assets.length} asset requests were recorded`);
  for (const a of assets) {
    if (a.transferSize !== 0) fail(where, `${a.name} went to the network on a repeat visit (${a.transferSize} bytes)`);
    if (!/\?v=[0-9a-f]{12}$/.test(a.name)) fail(where, `${a.name} is requested without a version`);
  }
  notes.push(`[caching] repeat visit: ${assets.length} dashboard files, ${assets.filter((a) => a.transferSize === 0).length} served from the browser cache`);
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
  if (/gdpr_mode/.test(text)) fail(where, 'a missing extension is reported as gdpr_mode');
  if (!/MALLARD_EXTENSION_DIR/.test(await page.locator('body').innerHTML())) fail(where, 'explanation does not say how to fix it');
  await assertCleanContent(page, where);
  await runAxe(page, where);
  await ctx.close();
}

if (GDPR) {
  const { ctx, page } = await newPage();
  const where = 'gdpr-instance';
  watch(page, where);
  await page.goto(GDPR, { waitUntil: 'load' });
  await loadEverything(page);
  const text = await page.locator('body').innerText();
  if (!/This report needs session data that gdpr_mode rounds away/.test(text)) fail(where, 'missing the gdpr_mode explanation');
  if ((await page.locator('.placeholder-error').count()) > 0) fail(where, 'gdpr_mode is presented as an error');
  if ((await page.locator('.placeholder-unavailable').count()) > 0) fail(where, 'gdpr_mode is presented with the missing-extension warning');
  if (/Behavioral analytics is unavailable/.test(text)) fail(where, 'gdpr_mode is reported as a missing extension');
  if (!(await page.locator('.metric-value').first().innerText()).match(/\d/)) fail(where, 'the headline figures are missing');
  if ((await page.locator('#retention .retention-table, #retention .placeholder:not(.placeholder-info)').count()) < 1) fail(where, 'weekly retention is not shown');
  // Entry pages are session-derived, so the tab explains instead of failing.
  await page.getByRole('tab', { name: 'Entry pages' }).click();
  await page.waitForTimeout(800);
  await page.waitForLoadState('networkidle');
  if (!/rounded to the hour/.test(await page.locator('#bd-pages').innerText())) fail(where, 'the entry-pages tab does not explain gdpr_mode');
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
