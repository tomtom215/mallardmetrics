#!/usr/bin/env node
//
// Catch `this.someMethod()` calls in the dashboard that no class defines.
//
// The dashboard has no build step and no test harness, so nothing else notices:
// `node --check` validates syntax and says nothing about whether a method
// exists, and the failure only appears when a user clicks the thing. A filter
// button calling `this.load()` when the method is named `refresh()` shipped
// exactly this way.
//
// Deliberately simple: it collects every method name defined anywhere in the
// file and checks every `this.NAME(` call site against that set plus Preact's
// own members. Cross-class calls would be a false negative; a typo — the case
// that actually happens — is caught.

import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import assert from 'node:assert/strict';

const FILE = process.argv[2] ?? 'src/dashboard/assets/app.js';
const source = readFileSync(FILE, 'utf8');

// Members Preact's Component provides, plus fields assigned in constructors.
const INHERITED = new Set([
  'setState',
  'forceUpdate',
  'render',
  'props',
  'state',
  'context',
  'base',
  'componentDidMount',
  'componentWillUnmount',
  'componentDidUpdate',
  'shouldComponentUpdate',
  'componentDidCatch',
]);

// `  name(args) {` or `  async name(args) {` at a class-body indent.
const defined = new Set(
  [...source.matchAll(/^\s{2}(?:async\s+)?([A-Za-z_$][\w$]*)\s*\([^)]*\)\s*\{/gm)].map(
    (m) => m[1],
  ),
);

// Fields assigned as `this.name = ...`, which are callable if they hold a function.
for (const m of source.matchAll(/\bthis\.([A-Za-z_$][\w$]*)\s*=/g)) {
  defined.add(m[1]);
}

const missing = new Map();
for (const m of source.matchAll(/\bthis\.([A-Za-z_$][\w$]*)\s*\(/g)) {
  const name = m[1];
  if (defined.has(name) || INHERITED.has(name)) continue;
  const line = source.slice(0, m.index).split('\n').length;
  if (!missing.has(name)) missing.set(name, line);
}

let failed = false;
for (const [name, line] of missing) {
  console.error(`${FILE}:${line}: this.${name}() is called but never defined`);
  failed = true;
}
if (!failed) console.log(`${FILE}: every this.<method>() call resolves`);

// ── Imports between the dashboard's own modules ────────────────────────────
//
// `import { x } from '/format.js'` where format.js has no `x` is a link error:
// the browser refuses to run app.js at all, and nothing but a browser notices.
const dir = dirname(resolve(FILE));
const modules = {};
for (const name of ['format', 'api']) {
  modules[name] = await import(pathToFileURL(resolve(dir, `${name}.js`)).href);
}
for (const m of source.matchAll(/import\s*\{([^}]*)\}\s*from\s*'\/(format|api)\.js'/g)) {
  for (const raw of m[1].split(',')) {
    const imported = raw.trim().split(/\s+as\s+/)[0];
    if (imported && !(imported in modules[m[2]])) {
      console.error(`${FILE}: imports ${imported} from /${m[2]}.js, which does not export it`);
      failed = true;
    }
  }
}

// ── Pure helpers ───────────────────────────────────────────────────────────
//
// format.js and api.js touch neither the DOM nor Preact, so the rules that
// decide what a user is told are asserted here instead of eyeballed.
const F = modules.format;
const A = modules.api;
const checks = [];
const check = (name, fn) => checks.push([name, fn]);

check('numbers never render undefined, NaN or null', () => {
  for (const bad of [undefined, null, NaN, Infinity, 'x']) {
    assert.equal(F.num(bad), F.DASH);
    assert.equal(F.compact(bad), F.DASH);
    assert.equal(F.pct(bad), F.DASH);
    assert.equal(F.fixed(bad), F.DASH);
    assert.equal(F.duration(bad), F.DASH);
    assert.equal(F.money(bad, 'USD'), F.DASH);
  }
  assert.equal(F.num(0), '0');
  assert.equal(F.pct(0.1234), '12.3%');
  assert.equal(F.duration(187), '3m 07s');
  assert.equal(F.duration(42), '42s');
});

check('very large numbers are compacted for cards, full elsewhere', () => {
  assert.equal(F.compact(999999), F.num(999999));
  assert.match(F.compact(1500000), /1\.5\s?M/i);
  assert.match(F.tick(25000), /25\s?K/i);
});

check('dates parse strictly and format in UTC', () => {
  assert.equal(F.parseDate('2026-02-31'), null);
  assert.equal(F.parseDate('garbage'), null);
  assert.equal(F.fmtDate('2026-10-03'), F.fmtDate('2026-10-03T00:00:00'));
  assert.match(F.fmtDate('2026-10-03'), /Oct/);
  assert.match(F.fmtDate('2026-10-03'), /2026/);
  assert.equal(F.fmtDate(undefined), F.DASH);
  assert.equal(F.hasTime('2026-10-03 14:00'), true);
  assert.equal(F.hasTime('2026-10-03'), false);
  assert.equal(F.shiftDay('2026-03-01', 1), '2026-02-28');
});

check('range presets are inclusive UTC dates', () => {
  const now = new Date('2026-10-20T03:00:00Z');
  assert.deepEqual(F.rangePreset('yesterday', now), { start: '2026-10-19', end: '2026-10-19' });
  assert.deepEqual(F.rangePreset('this-month', now), { start: '2026-10-01', end: '2026-10-20' });
  assert.deepEqual(F.rangePreset('last-month', now), { start: '2026-09-01', end: '2026-09-30' });
  assert.deepEqual(F.rangePreset('last-month', new Date('2026-01-05T00:00:00Z')), { start: '2025-12-01', end: '2025-12-31' });
});

check('custom range validation explains each rejection', () => {
  const today = '2026-10-20';
  assert.equal(F.validateRange('2026-10-01', '2026-10-20', today), null);
  assert.match(F.validateRange('', '2026-10-20', today), /both/);
  assert.match(F.validateRange('2026-10-21', '2026-10-20', today), /on or before/);
  assert.match(F.validateRange('2026-10-01', '2026-10-21', today), /future/);
  assert.match(F.validateRange('2025-01-01', '2026-10-20', today), /at most 366/);
  assert.equal(F.validateRange('2025-10-20', '2026-10-20', today), null); // exactly 366 inclusive days
  assert.match(F.validateRange('2026-13-01', '2026-10-20', today), /YYYY-MM-DD/);
});

check('filters round-trip and hostile input is dropped', () => {
  const allowed = new Set(['browsers', 'countries', 'utm-campaigns']);
  const filters = [
    { dimension: 'browsers', negated: false, value: 'Chrome' },
    { dimension: 'utm-campaigns', negated: true, value: 'spring,sale-2024' },
  ];
  assert.deepEqual(F.parseFilters(F.serializeFilters(filters), allowed), filters);
  assert.deepEqual(F.parseFilters('nope==x;browsers==Chrome;browsers==Firefox;countries!=', allowed), [filters[0]]);
  assert.deepEqual(F.parseFilters('entry-pages==/x', allowed), []);
  assert.equal(F.parseFilters('countries==' + 'x'.repeat(513), allowed).length, 0);
});

check('URL state round-trips, defaults are omitted, junk is ignored', () => {
  const allowed = new Set(['browsers']);
  const view = { siteId: 'a.example', period: '7d', startDate: '', endDate: '', compare: 'previous_period', filters: [{ dimension: 'browsers', negated: false, value: 'Chrome' }] };
  const qs = F.writeViewState(view);
  assert.deepEqual(F.readViewState(qs, allowed), { siteId: 'a.example', period: '7d', compare: 'previous_period', filters: view.filters });
  assert.equal(F.writeViewState({ siteId: 's', period: '30d', compare: 'none', filters: [] }), 'site=s');
  assert.deepEqual(F.readViewState('period=bogus&compare=sideways&start=2026-13-45&end=x', allowed), {});
  assert.equal(F.readViewState('period=today', allowed).period, 'day');
  const custom = F.readViewState('start=2026-09-01&end=2026-09-30', allowed);
  assert.equal(custom.period, 'custom');
  assert.deepEqual(F.readViewState('start=2026-09-30&end=2026-09-01', allowed), {});
});

check('chart axes use whole, de-duplicated ticks', () => {
  assert.equal(F.niceMax(0), 1);
  assert.equal(F.niceMax(47), 50);
  assert.equal(F.niceMax(1234), 2000);
  for (const top of [1, 2, 5, 25, 50, 2000]) {
    const t = F.axisTicks(top);
    assert.equal(t[0], 0);
    assert.equal(t.at(-1), top);
    assert.ok(t.every(Number.isInteger), `non-integer tick in ${t}`);
  }
});

check('failure messages are sentences, never raw bodies', () => {
  assert.equal(A.describeFailure(429, null), 'The server is busy. Try again in a moment.');
  assert.match(A.describeFailure(502, null), /HTTP 502/);
  assert.equal(A.describeFailure(400, { error: 'bad period' }), 'bad period');
  assert.doesNotMatch(A.describeFailure(500, { error: 'x'.repeat(400) }), /xxxx/);
  assert.doesNotMatch(A.describeFailure(500, { sql: 'SELECT 1' }), /SELECT/);
  assert.equal(A.retryAfterSeconds('3'), 3);
  assert.equal(A.retryAfterSeconds('Wed, 21 Oct 2026 07:28:00 GMT'), null);
  assert.ok(A.backoffMs(0, null, () => 0) >= 400);
  assert.ok(A.backoffMs(10, 1000, () => 0.999) <= 8250, 'backoff is capped');
  assert.ok(A.backoffMs(0, 3, () => 0) >= 3000, 'Retry-After is honoured');
});

check('the client retries 429, caps concurrency and classifies results', async () => {
  const res = (status, body, headers = {}) => ({
    status, ok: status < 400, headers: { get: (k) => headers[k.toLowerCase()] ?? null },
    json: async () => { if (body === undefined) throw new Error('no json'); return body; },
  });
  let calls = 0;
  const api = new A.Api({
    wait: async () => {},
    fetchImpl: async () => { calls += 1; return calls < 3 ? res(429, { error: 'busy' }, { 'retry-after': '1' }) : res(200, { fine: true }); },
  });
  assert.deepEqual(await api.get('/x'), { kind: 'ok', data: { fine: true } });
  assert.equal(api.stats.retried, 2);

  const kinds = { 401: 'unauthorized', 400: 'error', 500: 'error' };
  for (const [status, kind] of Object.entries(kinds)) {
    const r = await new A.Api({ wait: async () => {}, fetchImpl: async () => res(Number(status), { error: 'e' }) }).get('/x');
    assert.equal(r.kind, kind, `status ${status}`);
  }
  const behavioral = await new A.Api({ fetchImpl: async () => res(503, { error: 'Funnel analysis requires the DuckDB `behavioral` community extension' }) }).get('/x');
  assert.equal(behavioral.kind, 'unavailable');
  const other503 = await new A.Api({ fetchImpl: async () => res(503, { error: 'shutting down' }) }).get('/x');
  assert.equal(other503.kind, 'error');
  const html = await new A.Api({ fetchImpl: async () => res(502, undefined) }).get('/x');
  assert.equal(html.kind, 'error');
  assert.match(html.message, /HTTP 502/);
  const badOk = await new A.Api({ fetchImpl: async () => res(200, undefined) }).get('/x');
  assert.equal(badOk.kind, 'error');
  const refused = await new A.Api({ fetchImpl: async () => { throw new TypeError('Failed to fetch'); } }).get('/x');
  assert.equal(refused.kind, 'network');
  assert.doesNotMatch(refused.message, /Failed to fetch|TypeError/);

  let live = 0;
  let peak = 0;
  const throttled = new A.Api({ limit: 3, fetchImpl: async () => { live += 1; peak = Math.max(peak, live); await new Promise((r) => setTimeout(r, 5)); live -= 1; return res(200, {}); } });
  await Promise.all(Array.from({ length: 20 }, () => throttled.get('/x')));
  assert.equal(peak, 3);
  assert.equal(throttled.stats.requests, 20);

  const ctl = new AbortController();
  const slow = new A.Api({ limit: 1, fetchImpl: async () => { await new Promise((r) => setTimeout(r, 20)); return res(200, {}); } });
  const first = slow.get('/a');
  const second = slow.get('/b', { signal: ctl.signal });
  const third = slow.get('/c');
  ctl.abort();
  assert.equal((await second).kind, 'aborted');
  assert.equal((await first).kind, 'ok');
  assert.equal((await third).kind, 'ok', 'a cancelled request must not stall the queue');
});

for (const [name, fn] of checks) {
  try {
    await fn();
    console.log(`ok: ${name}`);
  } catch (e) {
    failed = true;
    console.error(`FAIL: ${name}\n  ${String(e.message).split('\n').join('\n  ')}`);
  }
}

process.exit(failed ? 1 : 0);
