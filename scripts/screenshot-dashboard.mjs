#!/usr/bin/env node
//
// Render the real dashboard in a browser and save PNGs for review.
//
// A check that only reads extracted text cannot see a collision, a clipped
// label or an unreadable colour; this writes the pictures to look at. It never
// writes inside the repository unless you point it there: PNGs do not belong in
// git.
//
//   node scripts/screenshot-dashboard.mjs http://127.0.0.1:8000 /tmp/shots
//   node scripts/screenshot-dashboard.mjs BASE OUTDIR --site=demo.example.com \
//        --empty=URL --nobeh=URL --auth=URL --password=PW
//
// Per theme (light, dark) and width (1440, 820, 390) it saves a full-page
// capture; then the states that are easy to break: a filter chip, comparison
// on, a rejected custom range, the chart table, the install snippet, loading
// skeletons, per-panel errors, a lost connection, and (when the extra
// instances are given) the empty, behavioral-unavailable and sign-in views.
//
// Requires Playwright (see check-dashboard-browser.mjs for resolution rules).

import { createRequire } from 'node:module';
import path from 'node:path';
import fs from 'node:fs';

const args = process.argv.slice(2);
const flag = (name) => args.find((a) => a.startsWith(`--${name}=`))?.slice(name.length + 3);
const [BASE, OUT] = args.filter((a) => !a.startsWith('--'));
if (!BASE || !OUT) {
  console.error('usage: screenshot-dashboard.mjs BASE_URL OUT_DIR [--site=ID] [--empty=URL] [--nobeh=URL] [--auth=URL --password=PW]');
  process.exit(2);
}
const SITE = flag('site');
const root = BASE.replace(/\/$/, '');
const first = SITE ? `${root}/?site=${encodeURIComponent(SITE)}` : `${root}/`;

let chromium;
try {
  ({ chromium } = await import('playwright'));
} catch {
  for (const dir of (process.env.NODE_PATH ?? '').split(path.delimiter).filter(Boolean)) {
    try {
      ({ chromium } = createRequire(path.join(dir, '_.js'))('playwright'));
      break;
    } catch (e) {
      /* try the next directory */
    }
  }
}
if (!chromium) {
  console.error('playwright is not installed: npm install -g playwright && npx playwright install chromium');
  process.exit(2);
}

fs.mkdirSync(OUT, { recursive: true });
const browser = await chromium.launch();
const SIZES = { desktop: { width: 1440, height: 900 }, tablet: { width: 820, height: 1180 }, mobile: { width: 390, height: 844 } };

async function open(url, { size = SIZES.desktop, theme = 'light', setup } = {}) {
  const ctx = await browser.newContext({ viewport: size, colorScheme: theme });
  const page = await ctx.newPage();
  if (setup) await setup(page);
  await page.goto(url, { waitUntil: 'load' });
  return { ctx, page };
}
const settle = async (page) => {
  await page.waitForLoadState('networkidle');
  await page.waitForTimeout(700);
};
const snap = (page, name, fullPage = false) => page.screenshot({ path: path.join(OUT, `${name}.png`), fullPage });

/** Sweep the page so lazily loaded panels are requested before a full-page capture. */
async function loadAll(page) {
  for (let pass = 0; pass < 6; pass += 1) {
    for (let y = 0; y < 40000; y += 500) {
      await page.evaluate((top) => window.scrollTo(0, top), y);
      await page.waitForTimeout(120);
      if (y > (await page.evaluate(() => document.documentElement.scrollHeight))) break;
    }
    await page.waitForLoadState('networkidle');
    if (!(await page.evaluate(() => document.querySelector('.skeleton')))) break;
  }
  await page.evaluate(() => window.scrollTo(0, 0));
}

for (const theme of ['light', 'dark']) {
  for (const [name, size] of Object.entries(SIZES)) {
    const { ctx, page } = await open(first, { size, theme });
    await settle(page);
    await loadAll(page);
    await snap(page, `${name}-${theme}-full`, true);
    await ctx.close();
  }
}

{
  const { ctx, page } = await open(first);
  await settle(page);
  if (await page.locator('.bar-link').count()) {
    await page.locator('.bar-link').first().click();
    await settle(page);
    await snap(page, 'state-filter-chip');
  }
  await page.getByRole('button', { name: 'Previous period' }).click();
  await settle(page);
  await snap(page, 'state-compare-on');
  await page.getByRole('button', { name: 'Custom' }).click();
  await page.getByLabel('From', { exact: true }).fill('2026-02-10');
  await page.getByLabel('To', { exact: true }).fill('2026-02-01');
  await page.getByRole('button', { name: 'Apply range' }).click();
  await snap(page, 'state-range-error');
  await page.getByRole('button', { name: 'Custom' }).click();
  await page.getByRole('button', { name: 'View as table' }).click();
  await snap(page, 'state-chart-table');
  await page.getByRole('button', { name: 'Install snippet' }).click();
  await snap(page, 'state-install');
  await ctx.close();
}

{
  // Loading skeletons: hold every report back for a few seconds.
  const { ctx, page } = await open(first, {
    setup: (p) => p.route('**/api/stats/**', async (r) => { await new Promise((x) => setTimeout(x, 4000)); r.continue(); }),
  });
  await page.waitForTimeout(1200);
  await snap(page, 'state-loading');
  await ctx.close();
}

{
  // Per-panel failures: a 500 with a body that must never be shown, and a 429 that never clears.
  const { ctx, page } = await open(first, {
    setup: async (p) => {
      await p.route('**/api/stats/breakdown/**', (r) => r.fulfill({ status: 500, contentType: 'application/json', body: '{"error":"the database is unavailable"}' }));
      await p.route('**/api/stats/goals**', (r) => r.fulfill({ status: 429, headers: { 'retry-after': '1' }, contentType: 'application/json', body: '{"error":"too many concurrent queries"}' }));
    },
  });
  await settle(page);
  await page.evaluate(() => document.getElementById('bd-pages')?.scrollIntoView());
  await page.waitForTimeout(1500);
  await snap(page, 'state-panel-errors');
  await ctx.close();
}

{
  const { ctx, page } = await open(first);
  await settle(page);
  await page.route('**/api/**', (r) => r.abort('connectionrefused'));
  await page.getByRole('button', { name: 'Refresh' }).click();
  await page.waitForTimeout(2500);
  await snap(page, 'state-offline');
  await ctx.close();
}

const extra = [
  ['empty', flag('empty')],
  ['no-behavioral', flag('nobeh')],
];
for (const [label, url] of extra) {
  if (!url) continue;
  for (const [name, size] of [['desktop', SIZES.desktop], ['mobile', SIZES.mobile]]) {
    const { ctx, page } = await open(url, { size });
    await settle(page);
    await loadAll(page);
    await snap(page, `state-${label}-${name}`, true);
    await ctx.close();
  }
}

if (flag('auth') && flag('password')) {
  const { ctx, page } = await open(flag('auth'));
  await settle(page);
  await snap(page, 'state-sign-in');
  await page.fill('input[type=password]', 'not-the-password');
  await page.click('button[type=submit]');
  await page.waitForTimeout(600);
  await snap(page, 'state-sign-in-error');
  await page.fill('input[type=password]', flag('password'));
  await page.click('button[type=submit]');
  await settle(page);
  await page.getByRole('button', { name: 'API keys' }).click();
  await snap(page, 'state-api-keys');
  await ctx.close();
}

await browser.close();
console.log(`screenshots written to ${OUT}`);
