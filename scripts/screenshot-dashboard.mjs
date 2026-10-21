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
//        --empty=URL --nobeh=URL --gdpr=URL --auth=URL --password=PW
//
// Every capture is taken at all three widths (desktop 1440, tablet 820, mobile
// 390) in both themes, named state-NAME-WIDTH-THEME.png, so a defect that only
// shows at one width or in one theme cannot hide. First a full-page capture of
// the populated dashboard, then the states that are easy to break: a filter chip, comparison
// on, a rejected custom range, the chart table, the install snippet, loading
// skeletons, per-panel errors, a query timeout, a lost connection, retention
// with unfinished weeks and with the identity caveat, a long breakdown, the
// figure definitions, and (when the extra instances are given) the empty,
// behavioral-unavailable, gdpr_mode and sign-in views.
//
// Requires Playwright (see check-dashboard-browser.mjs for resolution rules).

import { createRequire } from 'node:module';
import path from 'node:path';
import fs from 'node:fs';

const args = process.argv.slice(2);
const flag = (name) => args.find((a) => a.startsWith(`--${name}=`))?.slice(name.length + 3);
const [BASE, OUT] = args.filter((a) => !a.startsWith('--'));
if (!BASE || !OUT) {
  console.error('usage: screenshot-dashboard.mjs BASE_URL OUT_DIR [--site=ID] [--only=NAME,...] (a prefix of the state label, e.g. offline) [--empty=URL] [--nobeh=URL] [--gdpr=URL] [--auth=URL --password=PW]');
  process.exit(2);
}
const SITE = flag('site');
// --only=populated,gdpr renders just the states whose name starts with one of these.
const ONLY = flag('only')?.split(',');
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


const VIEWS = [];
for (const theme of ['light', 'dark']) for (const [size, dims] of Object.entries(SIZES)) VIEWS.push({ theme, size, dims });
const tag = (v) => `${v.size}-${v.theme}`;

/** Run `run(page, name)` once per width and theme, each in a fresh context. */
async function eachView(label, url, run, { setup, full = false } = {}) {
  if (ONLY && !ONLY.some((o) => label.startsWith(o))) return;
  for (const v of VIEWS) {
    const { ctx, page } = await open(url, { size: v.dims, theme: v.theme, setup });
    await run(page, (extra = '', whole = full) => snap(page, `state-${label}${extra}-${tag(v)}`, whole), v);
    await ctx.close();
  }
}
const jsonReply = (status, body, headers = {}) => (r) => r.fulfill({ status, headers, contentType: 'application/json', body: typeof body === 'string' ? body : JSON.stringify(body) });

await eachView('populated', first, async (page, shot) => {
  await settle(page);
  await loadAll(page);
  await shot();
}, { full: true });

await eachView('filter-chip', first, async (page, shot) => {
  await settle(page);
  if (await page.locator('.bar-link').count()) {
    await page.locator('.bar-link').first().click();
    await settle(page);
    await shot();
  }
});

await eachView('compare-on', first, async (page, shot) => {
  await settle(page);
  await page.getByRole('button', { name: 'Previous period' }).click();
  await settle(page);
  await shot();
});

await eachView('range-error', first, async (page, shot) => {
  await settle(page);
  await page.getByRole('button', { name: 'Custom' }).click();
  await page.getByLabel('From', { exact: true }).fill('2026-02-10');
  await page.getByLabel('To', { exact: true }).fill('2026-02-01');
  await page.getByRole('button', { name: 'Apply range' }).click();
  await shot();
});

await eachView('chart-table', first, async (page, shot) => {
  await settle(page);
  await page.getByRole('button', { name: 'View as table' }).click();
  await shot();
});

await eachView('install', first, async (page, shot) => {
  await settle(page);
  await page.getByRole('button', { name: 'Install snippet' }).click();
  await shot();
});

// Loading skeletons: hold every report back for a few seconds.
await eachView('loading', first, async (page, shot) => {
  await page.waitForTimeout(1200);
  await shot();
}, { setup: (p) => p.route('**/api/stats/**', async (r) => { await new Promise((x) => setTimeout(x, 4000)); r.continue(); }) });

// Per-panel failures: a 500 with a body that must never be shown, and a 429 that never clears.
await eachView('panel-errors', first, async (page, shot) => {
  await settle(page);
  await page.evaluate(() => document.getElementById('bd-pages')?.scrollIntoView());
  await page.waitForTimeout(1500);
  await shot();
}, {
  setup: async (p) => {
    await p.route('**/api/stats/breakdown/**', jsonReply(500, '{"error":"the database is unavailable"}'));
    await p.route('**/api/stats/goals**', jsonReply(429, '{"error":"too many concurrent queries"}', { 'retry-after': '1' }));
  },
});

// Server gone: one banner, and the panels collapse to a line each.
await eachView('offline', first, async (page, shot) => {
  await settle(page);
  await loadAll(page);
  await page.route('**/api/**', (r) => r.abort('connectionrefused'));
  await page.getByRole('button', { name: 'Refresh' }).click();
  await page.waitForTimeout(2500);
  await shot('-top');
  await shot('-full', true);
});

// A query that overran its deadline, and a server that stays busy.
await eachView('timeout-and-busy', first, async (page, shot) => {
  await settle(page);
  await page.evaluate(() => document.getElementById('goals')?.scrollIntoView());
  await page.waitForFunction(() => /busy/i.test(document.getElementById('revenue')?.innerText ?? ''), null, { timeout: 20000 }).catch(() => {});
  await page.waitForTimeout(500);
  await shot();
}, {
  setup: async (p) => {
    await p.route('**/api/stats/goals**', jsonReply(504, '{"error":"The query did not finish within 30 seconds. Try a shorter date range or fewer filters.","code":"query_timeout"}'));
    await p.route('**/api/stats/revenue**', jsonReply(429, '{"error":"too many concurrent queries","code":"too_many_requests"}', { 'retry-after': '0' }));
  },
});

// Retention as the server reports it: unfinished weeks, a partial first week.
{
  const mock = (identity) => jsonReply(200, {
    identity_supports_cohorts: identity,
    visitor_salt_rotation_days: identity ? 30 : 7,
    caveat: identity ? null : 'server wording that the dashboard does not parse',
    cohorts: [
      { cohort_date: '2026-09-28', cohort_size: 120, retained: [120, 54, 31, 22], retention_rates: [1, 0.45, 0.2583, 0.1833], complete_weeks: 4, partial_first_week: true },
      { cohort_date: '2026-10-05', cohort_size: 98, retained: [98, 41, 27, 0], retention_rates: [1, 0.4184, 0.2755, 0], complete_weeks: 3, partial_first_week: false },
      { cohort_date: '2026-10-12', cohort_size: 143, retained: [143, 60, 0, 0], retention_rates: [1, 0.4196, 0, 0], complete_weeks: 2, partial_first_week: false },
      { cohort_date: '2026-10-19', cohort_size: 37, retained: [37, 0, 0, 0], retention_rates: [1, 0, 0, 0], complete_weeks: 0, partial_first_week: false },
    ],
  });
  for (const [label, identity] of [['retention-weeks', true], ['retention-caveat', false]]) {
    await eachView(label, first, async (page, shot) => {
      await settle(page);
      await page.evaluate(() => document.getElementById('retention')?.scrollIntoView());
      await page.waitForTimeout(1200);
      await shot();
    }, { setup: (p) => p.route('**/api/stats/retention**', mock(identity)) });
  }
}

// Unavailable reasons chosen by the server's own fields, including an older server that names none.
for (const [label, patch] of [
  ['unavailable-rounded-mock', { behavioral_available: false, sessions_unavailable_reason: 'timestamps_rounded' }],
  ['unavailable-extension-mock', { behavioral_available: false, sessions_unavailable_reason: 'extension_missing' }],
  ['unavailable-unknown-mock', { behavioral_available: false }],
]) {
  await eachView(label, first, async (page, shot) => {
    await settle(page);
    await loadAll(page);
    await shot();
  }, {
    full: true,
    setup: async (p) => {
      await p.route('**/api/stats/main**', async (route) => {
        const res = await route.fetch();
        await route.fulfill({ response: res, json: { ...(await res.json()), ...patch, total_sessions: null, bounce_rate: null, avg_visit_duration_secs: null, views_per_visit: null } });
      });
    },
  });
}

// A long breakdown (Show more), entry pages with their own columns, and the definitions.
await eachView('breakdown-more', first, async (page, shot) => {
  await settle(page);
  await page.evaluate(() => document.getElementById('bd-pages')?.scrollIntoView());
  await page.waitForTimeout(800);
  const more = page.getByRole('button', { name: /Show more/ }).first();
  if (await more.count()) {
    await more.click();
    await settle(page);
    await shot();
  }
});

await eachView('entry-pages', first, async (page, shot) => {
  await settle(page);
  await page.evaluate(() => document.getElementById('bd-pages')?.scrollIntoView());
  await page.getByRole('tab', { name: 'Entry pages' }).click();
  await settle(page);
  await shot();
});

await eachView('definitions', first, async (page, shot) => {
  await settle(page);
  await page.locator('.definitions summary').click();
  await page.locator('.definitions').scrollIntoViewIfNeeded();
  await shot();
});

// The site selector with several sites, one named by the address bar.
await eachView('site-selector', root + '/', async (page, shot) => {
  await settle(page);
  await page.locator('.toolbar select').focus();
  await shot();
});

const extra = [
  ['empty', flag('empty')],
  ['no-behavioral', flag('nobeh')],
  ['gdpr', flag('gdpr')],
];
for (const [label, url] of extra) {
  if (!url) continue;
  await eachView(label, url, async (page, shot) => {
    await settle(page);
    await loadAll(page);
    await shot();
  }, { full: true });
}

if (flag('auth') && flag('password')) {
  await eachView('sign-in', flag('auth'), async (page, shot) => {
    await settle(page);
    await shot();
    await page.fill('input[type=password]', 'not-the-password');
    await page.click('button[type=submit]');
    await page.waitForTimeout(600);
    await shot('-error');
    await page.fill('input[type=password]', flag('password'));
    await page.click('button[type=submit]');
    await settle(page);
    await shot('-signed-in');
    await page.getByRole('button', { name: 'API keys' }).click();
    await shot('-api-keys');
  });
}

// Saved segments and share links, as an admin sees them and as a viewer sees
// them. The viewer states open a real link made by the admin states' session.
if (flag('auth') && flag('password')) {
  const authUrl = flag('auth');
  const signIn = async (page) => {
    await page.fill('input[type=password]', flag('password'));
    await page.click('button[type=submit]');
    await page.waitForSelector('#main');
    await settle(page);
  };
  /** Create a link from inside a signed-in page. */
  const mint = (page, body) => page.evaluate(async (b) => {
    const r = await fetch('/api/share-links', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(b) });
    return r.json();
  }, body);
  const site = await (async () => {
    const { ctx, page } = await open(authUrl);
    await page.waitForSelector('input[type=password]');
    await signIn(page);
    const chosen = await page.locator('.toolbar select').first().inputValue().catch(() => '');
    await ctx.close();
    return chosen;
  })();

  await eachView('segments-empty', authUrl, async (page, shot) => {
    await signIn(page);
    await page.getByRole('button', { name: 'Segments' }).click();
    await shot();
  }, { setup: (p) => p.route('**/api/segments?**', jsonReply(200, [])) });

  await eachView('segments-saved', authUrl, async (page, shot) => {
    await signIn(page);
    await loadAll(page);
    await page.locator('.bar-link').first().click();
    await settle(page);
    await page.getByRole('button', { name: 'Segments' }).click();
    await page.getByLabel('Name', { exact: true }).fill('Chrome on desktop');
    await page.getByRole('button', { name: 'Save current filters' }).click();
    await settle(page);
    await shot('-panel');
    await page.locator('#segments tbody').scrollIntoViewIfNeeded();
    await shot('-panel-table');
    await page.getByLabel('Name', { exact: true }).fill('chrome on DESKTOP');
    await page.getByRole('button', { name: 'Save current filters' }).click();
    await page.waitForSelector('#segments [role=alert]');
    await shot('-duplicate-name');
    await page.getByRole('button', { name: 'Close' }).first().click();
    await shot('-menu');
    // Leave nothing behind for the next view.
    await page.getByRole('button', { name: 'Segments' }).click();
    page.once('dialog', (d) => d.accept());
    await page.getByRole('button', { name: /^Delete/ }).first().click();
    await settle(page);
  });

  await eachView('share-empty', authUrl, async (page, shot) => {
    await signIn(page);
    await page.getByRole('button', { name: 'Share links' }).click();
    await shot();
  }, { setup: (p) => p.route('**/api/share-links', (r) => (r.request().method() === 'GET' ? jsonReply(200, [])(r) : r.continue())) });

  const days = (n) => new Date(Date.now() + n * 86400000).toISOString().replace(/\.\d+Z$/, 'Z');
  const row = (id, label, segment, status, extra = {}) => ({ id, label, site_id: site || 'demo.example.com', segment, filters: segment ? 'browsers==Chrome' : '', created_at: days(-12), expires_at: days(18), last_used_at: days(-1), status, ...extra });
  await eachView('share-list', authUrl, async (page, shot) => {
    await signIn(page);
    await page.getByRole('button', { name: 'Share links' }).click();
    await settle(page);
    await shot();
  }, {
    setup: (p) => p.route('**/api/share-links', (r) => (r.request().method() === 'GET'
      ? jsonReply(200, [
          row('a1', 'Q4 board pack', 'Chrome users', 'active'),
          row('b2', 'Agency read-only access with a rather long label that must truncate', null, 'active', { expires_at: null, last_used_at: null }),
          row('c3', 'Old partner link', null, 'expired', { expires_at: days(-3), created_at: days(-40) }),
          row('d4', 'Revoked after the audit', 'Chrome users', 'revoked', { created_at: days(-60), expires_at: days(5) }),
        ])(r)
      : r.continue())),
  });

  await eachView('share-created', authUrl, async (page, shot) => {
    await signIn(page);
    await page.getByRole('button', { name: 'Share links' }).click();
    await page.getByLabel('Label (private)').fill('Q4 board pack');
    await page.getByRole('button', { name: 'Create link' }).click();
    await page.waitForSelector('.banner-key .key-value');
    await shot();
    await page.getByRole('button', { name: 'Done' }).click();
    // The link is real; revoke it so the instance is left as it was found.
    page.once('dialog', (d) => d.accept());
    await page.getByRole('button', { name: /^Revoke/ }).first().click();
    await settle(page);
    await shot('-revoked');
  });

  await eachView('share-error', authUrl, async (page, shot) => {
    await signIn(page);
    await page.getByRole('button', { name: 'Share links' }).click();
    await page.getByLabel('Label (private)').fill('One too many');
    await page.getByRole('button', { name: 'Create link' }).click();
    await page.waitForSelector('#share [role=alert]');
    await shot();
  }, { setup: (p) => p.route('**/api/share-links', (r) => (r.request().method() === 'POST'
    ? jsonReply(409, { code: 'conflict', error: 'This instance already has 100 active share links; revoke one first' })(r)
    : r.continue())) });

  // The viewer. One link per view, made by a throwaway signed-in context.
  const viewerLink = async (segment) => {
    const { ctx, page } = await open(authUrl);
    await page.waitForSelector('input[type=password]');
    await signIn(page);
    let segmentId;
    if (segment) {
      segmentId = await page.evaluate(async ([siteId, name]) => {
        const r = await fetch('/api/segments', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ site_id: siteId, name, filters: 'browsers==Chrome' }) });
        return (await r.json()).id;
      }, [site, segment]);
    }
    const made = await mint(page, { label: 'screenshots', site_id: site, ...(segmentId ? { segment_id: segmentId } : {}), expires_in_days: 30 });
    await ctx.close();
    return { url: `${authUrl}/#share=${made.token}`, id: made.id, segmentId };
  };
  const dispose = async (made) => {
    const { ctx, page } = await open(authUrl);
    await page.waitForSelector('input[type=password]');
    await signIn(page);
    await page.evaluate(async ([id, segmentId]) => {
      await fetch(`/api/share-links/${id}`, { method: 'DELETE' });
      if (segmentId) await fetch(`/api/segments/${segmentId}`, { method: 'DELETE' });
    }, [made.id, made.segmentId]);
    await ctx.close();
  };

  const plain = await viewerLink(null);
  await eachView('shared-populated', plain.url, async (page, shot) => {
    await settle(page);
    await loadAll(page);
    await shot();
  }, { full: true });
  await eachView('shared-filter', plain.url, async (page, shot) => {
    await settle(page);
    await loadAll(page);
    if (await page.locator('.bar-link').count()) {
      await page.locator('.bar-link').first().click();
      await settle(page);
      await shot();
    }
  });

  const limited = await viewerLink('Chrome only');
  await eachView('shared-segment', limited.url, async (page, shot) => {
    await settle(page);
    await loadAll(page);
    await shot();
  }, { full: true });
  await dispose(limited);

  // The same links after they stop working, and a mangled one.
  await eachView('shared-ended', limited.url, async (page, shot) => {
    await page.waitForSelector('.share-ended');
    await settle(page);
    await shot();
  });
  await eachView('shared-broken', `${authUrl}/#share=ms_truncated`, async (page, shot) => {
    await page.waitForSelector('.share-ended');
    await shot();
  });
  await eachView('shared-busy', plain.url, async (page, shot) => {
    // The client retries a 429 with backoff, so the network never goes idle; wait for it to give up.
    await page.waitForFunction(() => /busy right now/i.test(document.body.innerText), null, { timeout: 60000 }).catch(() => {});
    await page.waitForTimeout(500);
    await shot();
  }, { setup: (p) => p.route('**/api/shared/stats/**', jsonReply(429, { code: 'too_many_requests', error: 'This share link is being used too heavily. Try again in a moment.' }, { 'retry-after': '0' })) });
  await dispose(plain);
}

await browser.close();
console.log(`screenshots written to ${OUT}`);
