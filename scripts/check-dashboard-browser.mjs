#!/usr/bin/env node
//
// Drive the dashboard in a real browser and fail on any console or page error.
//
// The dashboard is hand-written Preact with no build step, so nothing else
// executes it: `node --check` sees only syntax, and the Rust tests never run
// the JavaScript at all. This is the only check that would notice a render
// crash, a handler wired to a renamed method, or a fetch to a route that moved.
//
// Not part of CI, because it needs a browser download that every run would pay
// for. Run it before shipping a dashboard change:
//
//   cargo build
//   MALLARD_DATA_DIR=/tmp/mm-ui MALLARD_PORT=18333 target/debug/mallard-metrics &
//   # ...post a few events, then:
//   node scripts/check-dashboard-browser.mjs http://127.0.0.1:18333
//
// Requires Playwright (`npm i -g playwright && npx playwright install chromium`).

const BASE = process.argv[2] ?? 'http://127.0.0.1:8000';

let chromium;
try {
  ({ chromium } = await import('playwright'));
} catch {
  console.error(
    'playwright is not installed. Install it with:\n' +
      '  npm install -g playwright && npx playwright install chromium',
  );
  process.exit(2);
}

const problems = [];
const browser = await chromium.launch();
const page = await browser.newPage();
page.on('pageerror', (e) => problems.push(`page error: ${e.message}`));
page.on('console', (m) => {
  if (m.type() === 'error') problems.push(`console error: ${m.text()}`);
});
page.on('requestfailed', (r) => problems.push(`request failed: ${r.url()}`));

try {
  await page.goto(BASE, { waitUntil: 'networkidle', timeout: 30000 });
  await page.waitForTimeout(2500);

  const body = await page.locator('body').innerText();
  if (!body.includes('Mallard Metrics')) {
    problems.push('the dashboard shell did not render');
  }

  // Exercise the segment filters: click a breakdown row, expect a chip, clear it.
  const rows = await page.locator('.bar-link').count();
  if (rows > 0) {
    await page.locator('.bar-link').first().click();
    await page.waitForTimeout(1500);
    if ((await page.locator('.filter-chip').count()) === 0) {
      problems.push('clicking a breakdown row added no filter chip');
    }
    await page.locator('.filter-clear').click();
    await page.waitForTimeout(1200);
    if ((await page.locator('.filter-chip').count()) !== 0) {
      problems.push('"Clear all" left a filter chip behind');
    }
  } else {
    console.log('note: no data to click — seed some events for a fuller check');
  }
} finally {
  await browser.close();
}

if (problems.length === 0) {
  console.log('dashboard: rendered and interacted cleanly');
  process.exit(0);
}
for (const p of problems) console.error(p);
process.exit(1);
