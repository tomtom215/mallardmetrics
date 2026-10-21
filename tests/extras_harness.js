// Drives tracking/script.js and tracking/extras.js together against a fake
// browser with a deterministic clock and timers, and checks what is sent.
// Usage: node extras_harness.js <script.js> <extras.js>
'use strict';
const fs = require('fs');
const vm = require('vm');
const assert = require('assert');

const mainSource = fs.readFileSync(process.argv[2], 'utf8');
const extrasSource = fs.readFileSync(process.argv[3], 'utf8');

/**
 * A fake page running both scripts. opts: url, main (attrs of the main tag,
 * default {data-domain}), extras (attrs of the extras tag; null = not loaded),
 * noMain, title, scrollHeight, viewport, matches (selectors present),
 * ignore (mallard_ignore), dnt, throwOnStorageWrite.
 */
function page(opts) {
  const mainAttrs = Object.assign({ 'data-domain': 'site.test' }, opts.main || {});
  const extrasAttrs = opts.extras === undefined ? {} : opts.extras;
  const sent = [];
  const endpoints = [];
  const listeners = { window: {}, document: {} };
  const add = (who) => (type, fn) => { (listeners[who][type] = listeners[who][type] || []).push(fn); };
  const remove = (who) => (type, fn) => {
    listeners[who][type] = (listeners[who][type] || []).filter((f) => f !== fn);
  };
  let clock = 1000000;
  const timers = [];
  let url = new URL(opts.url || 'https://site.test/');
  const location = {};
  const sync = () => {
    Object.assign(location, {
      href: url.href, origin: url.origin, protocol: url.protocol, host: url.host,
      hostname: url.hostname, pathname: url.pathname, search: url.search, hash: url.hash,
    });
  };
  sync();
  const history = {
    pushState(_s, _t, to) { url = new URL(to, url); sync(); },
    replaceState(_s, _t, to) { url = new URL(to, url); sync(); },
  };
  class FakeBlob { constructor(parts) { this.text = parts.join(''); } }
  const tag = (attrs, src) => ({ getAttribute: (n) => (n in attrs ? attrs[n] : null), src });
  const mainTag = tag(mainAttrs, 'https://stats.test/mallard.js');
  const extrasTag = tag(extrasAttrs || {}, 'https://stats.test/mallard-extras.js');
  const present = new Set(opts.matches || []);
  const doc = {
    currentScript: mainTag,
    baseURI: url.href,
    referrer: '',
    visibilityState: opts.hidden ? 'hidden' : 'visible',
    prerendering: false,
    title: opts.title || 'Home',
    focused: !opts.unfocused,
    hasFocus() { return doc.focused; },
    addEventListener: add('document'),
    removeEventListener: remove('document'),
    querySelector(sel) {
      if (sel === 'script[data-domain]') return opts.noMain ? null : mainTag;
      if (sel === 'script[src*="mallard-extras"]') return extrasTag;
      if (sel === '!!invalid') throw new SyntaxError('invalid selector');
      return present.has(sel) ? {} : null;
    },
    documentElement: { scrollHeight: opts.scrollHeight || 4000 },
  };
  const writes = [];
  const win = {
    location, history, document: doc,
    navigator: {
      sendBeacon(endpoint, blob) { endpoints.push(endpoint); sent.push(JSON.parse(blob.text)); return true; },
      doNotTrack: opts.dnt ? '1' : null,
    },
    innerWidth: 1280,
    innerHeight: opts.viewport || 800,
    pageYOffset: 0,
    localStorage: {
      getItem: (k) => (k === 'mallard_ignore' && opts.ignore ? 'true' : null),
      setItem() { writes.push('setItem'); throw new Error('storage write'); },
    },
    addEventListener: add('window'),
    removeEventListener: remove('window'),
  };
  win.window = win;
  Object.defineProperty(doc, 'cookie', { get: () => '', set() { writes.push('cookie'); throw new Error('cookie write'); } });
  const ctx = vm.createContext({
    window: win, document: doc, navigator: win.navigator, URL, Blob: FakeBlob, JSON, RegExp,
    Math, String, Date: { now: () => clock }, Error, SyntaxError,
    XMLHttpRequest: function () { throw new Error('XHR not expected'); },
    setTimeout(fn, ms) { timers.push({ fn, due: clock + ms }); return timers.length; },
  });
  if (!opts.noMain) vm.runInContext(mainSource, ctx);
  if (opts.stub) {
    win.mallard = function () { (win.mallard.q = win.mallard.q || []).push(arguments); };
  }
  let loadError = null;
  if (opts.extras !== null) {
    doc.currentScript = opts.noCurrentScript ? null : extrasTag;
    try { vm.runInContext(extrasSource, ctx); } catch (e) { loadError = e; }
  }
  const fire = (who, type, ev) => (listeners[who][type] || []).slice().forEach((f) => f(ev || {}));
  const p = {
    sent, endpoints, win, doc, fire, writes, loadError, listeners,
    /** Events other than the main tracker's pageviews. */
    extra() { return sent.filter((e) => e.n !== 'pageview'); },
    advance(ms) {
      clock += ms;
      timers.slice().sort((a, b) => a.due - b.due).forEach((t) => {
        if (t.due <= clock && !t.done) { t.done = true; t.fn(); }
      });
    },
    scroll(y) { win.pageYOffset = y; fire('window', 'scroll'); },
    hide() { doc.visibilityState = 'hidden'; fire('document', 'visibilitychange'); },
    show() { doc.visibilityState = 'visible'; fire('document', 'visibilitychange'); },
    blur() { doc.focused = false; fire('window', 'blur'); },
    focus() { doc.focused = true; fire('window', 'focus'); },
    go(to) { history.pushState(null, '', to); },
    submit(form) { fire('document', 'submit', { target: form }); },
  };
  return p;
}

const form = (attrs, hasPassword) => ({
  tagName: 'FORM',
  getAttribute: (n) => (n in attrs ? attrs[n] : null),
  querySelector: (sel) => (hasPassword && sel === 'input[type=password]' ? {} : null),
});

const tests = [];
const test = (name, fn) => tests.push([name, fn]);
const scrollOn = { 'data-scroll': 'true' };
const timeOn = { 'data-engagement': 'true' };

// ── scroll depth ────────────────────────────────────────────────────────

test('scroll: one event at the deepest of 25/50/75/100, sent when the page is hidden', () => {
  const p = page({ extras: scrollOn });
  p.scroll(300);   // (300+800)/4000 = 27.5%
  p.scroll(1300);  // 52.5%
  p.scroll(500);   // scrolling back up never lowers it
  assert.strictEqual(p.extra().length, 0, 'nothing is sent while the page is in use');
  p.hide();
  assert.strictEqual(p.extra().length, 1);
  const e = p.extra()[0];
  assert.strictEqual(e.n, 'Scroll Depth');
  assert.deepStrictEqual(JSON.parse(e.p), { depth: 50 });
  assert.strictEqual(e.d, 'site.test', 'domain comes from the main tracker');
  assert.strictEqual(e.u, 'https://site.test/');
  assert.strictEqual(e.r, null, 'no referrer on a derived event');
});

test('scroll: each threshold, including the bottom within sub-pixel slack', () => {
  const cases = [[0, null], [100, null], [200, 25], [1200, 50], [2200, 75], [3198, 100], [3200, 100]];
  for (const [y, want] of cases) {
    const p = page({ extras: scrollOn });
    p.scroll(y);
    p.hide();
    if (want === null) assert.strictEqual(p.extra().length, 0, `y=${y}`);
    else assert.strictEqual(JSON.parse(p.extra()[0].p).depth, want, `y=${y}`);
  }
});

test('scroll: a page that fits the viewport has reached 100% at load', () => {
  const p = page({ extras: scrollOn, scrollHeight: 700 });
  p.hide();
  assert.strictEqual(JSON.parse(p.extra()[0].p).depth, 100);
});

test('scroll: sent once per page view even if hidden again; nothing without the attribute', () => {
  const p = page({ extras: scrollOn });
  p.scroll(1300);
  p.hide(); p.show(); p.scroll(3300); p.hide();
  p.fire('window', 'pagehide');
  assert.strictEqual(p.extra().length, 1);
  const q = page({ extras: {} });
  q.scroll(3300); q.hide();
  assert.strictEqual(q.extra().length, 0);
});

test('scroll: pagehide alone is enough (browsers that skip visibilitychange)', () => {
  const p = page({ extras: scrollOn });
  p.scroll(1300);
  p.fire('window', 'pagehide', { persisted: false });
  assert.strictEqual(p.extra().length, 1);
});

// ── engagement time ─────────────────────────────────────────────────────

test('engagement: counts only visible-and-focused time', () => {
  const p = page({ extras: timeOn });
  p.advance(4000);          // 4s engaged
  p.blur(); p.advance(60000); // unfocused: not counted
  p.focus(); p.advance(3000); // 3s engaged
  p.hide(); p.advance(120000); // hidden: nothing runs, nothing counted
  assert.strictEqual(p.extra().length, 1);
  assert.strictEqual(p.extra()[0].n, 'Engagement');
  assert.deepStrictEqual(JSON.parse(p.extra()[0].p), { engaged_ms: 7000 });
});

test('engagement: a page opened in a background tab starts counting when first shown', () => {
  const p = page({ extras: timeOn, hidden: true });
  p.advance(600000); // hidden for ten minutes: not engaged
  p.show(); p.advance(4000); p.hide();
  assert.deepStrictEqual(JSON.parse(p.extra()[0].p), { engaged_ms: 4000 });
});

test('engagement: a page that loads without window focus counts only once focused', () => {
  const p = page({ extras: timeOn, unfocused: true });
  p.advance(30000); p.focus(); p.advance(2000); p.hide();
  assert.deepStrictEqual(JSON.parse(p.extra()[0].p), { engaged_ms: 2000 });
});

test('engagement: under one second is not reported, and it is capped at 30 minutes', () => {
  const a = page({ extras: timeOn });
  a.advance(900); a.hide();
  assert.strictEqual(a.extra().length, 0);
  const b = page({ extras: timeOn });
  b.advance(10 * 3600 * 1000); b.hide();
  assert.deepStrictEqual(JSON.parse(b.extra()[0].p), { engaged_ms: 1800000 });
});

test('engagement: sent once; later activity on the same page view is not re-sent', () => {
  const p = page({ extras: timeOn });
  p.advance(2000); p.hide(); p.show(); p.advance(5000); p.hide();
  p.fire('window', 'pagehide');
  assert.strictEqual(p.extra().length, 1);
  assert.strictEqual(JSON.parse(p.extra()[0].p).engaged_ms, 2000);
});

test('engagement and scroll together send two events, depth first', () => {
  const p = page({ extras: Object.assign({}, scrollOn, timeOn) });
  p.advance(3000); p.scroll(2200); p.hide();
  assert.deepStrictEqual(p.extra().map((e) => e.n), ['Scroll Depth', 'Engagement']);
});

// ── bfcache ─────────────────────────────────────────────────────────────

test('bfcache: pagehide reports, and a persisted pageshow starts a fresh page view', () => {
  const p = page({ extras: Object.assign({}, scrollOn, timeOn) });
  p.advance(2000); p.scroll(1300);
  p.fire('window', 'pagehide', { persisted: true });
  assert.strictEqual(p.extra().length, 2);
  p.fire('window', 'pageshow', { persisted: false });   // initial load: ignored
  p.advance(9000);
  p.hide();
  assert.strictEqual(p.extra().length, 2, 'a non-persisted pageshow starts nothing');

  p.show();
  p.fire('window', 'pageshow', { persisted: true });     // restored: new page view
  p.win.pageYOffset = 0;
  p.advance(3000); p.scroll(2200);
  p.hide();
  const later = p.extra().slice(2);
  assert.deepStrictEqual(later.map((e) => e.n), ['Scroll Depth', 'Engagement']);
  assert.strictEqual(JSON.parse(later[0].p).depth, 75);
  assert.strictEqual(JSON.parse(later[1].p).engaged_ms, 3000, 'the clock restarted');
});

// ── SPA navigation ──────────────────────────────────────────────────────

test('SPA: a route change reports the previous page under its own URL', () => {
  const p = page({ extras: Object.assign({}, scrollOn, timeOn) });
  p.advance(2500); p.scroll(1300);
  p.go('/pricing');
  const prev = p.extra();
  assert.deepStrictEqual(prev.map((e) => e.n), ['Scroll Depth', 'Engagement']);
  assert.ok(prev.every((e) => e.u === 'https://site.test/'), 'old URL, not the new route');
  assert.strictEqual(p.sent.filter((e) => e.n === 'pageview').length, 2, 'the main tracker still counts it');

  p.win.pageYOffset = 0; p.advance(1500); p.scroll(2200); p.hide();
  const next = p.extra().slice(2);
  assert.ok(next.every((e) => e.u === 'https://site.test/pricing'));
  assert.strictEqual(JSON.parse(next[0].p).depth, 75);
  assert.strictEqual(JSON.parse(next[1].p).engaged_ms, 1500);
});

test('SPA: replaceState to the same page, or a bare fragment, is not a navigation', () => {
  const p = page({ extras: scrollOn });
  p.scroll(1300);
  p.win.history.replaceState(null, '', '/');
  p.win.history.pushState(null, '', '/#section');
  assert.strictEqual(p.extra().length, 0);
  p.win.history.replaceState(null, '', '/?tab=2'); // main counts a query change; so do we
  assert.strictEqual(p.extra().length, 1);
});

test('SPA: popstate (back/forward) ends the page too', () => {
  const p = page({ extras: scrollOn });
  p.go('/a'); p.scroll(1300);
  p.win.history.pushState(null, '', '/');
  p.fire('window', 'popstate');
  assert.strictEqual(p.extra().length, 1);
  assert.strictEqual(p.extra()[0].u, 'https://site.test/a');
});

test('SPA: hash routes follow the main tracker setting (data-hash)', () => {
  const p = page({ url: 'https://site.test/app/#/a', main: { 'data-hash': 'true' }, extras: scrollOn });
  p.scroll(1300);
  p.win.history.pushState(null, '', '#/b');
  p.fire('window', 'hashchange');
  assert.strictEqual(p.extra().length, 1);
  assert.strictEqual(p.extra()[0].u, 'https://site.test/app/a', 'folded into a path like the pageview');
  const q = page({ url: 'https://site.test/app/#/a', extras: scrollOn });
  q.win.history.pushState(null, '', '#/b');
  q.fire('window', 'hashchange');
  assert.strictEqual(q.extra().length, 0, 'without data-hash a fragment is not a route');
});

// ── 404 ─────────────────────────────────────────────────────────────────

test('404: selector marker sends "404" once, with the page URL, no props', () => {
  const p = page({ url: 'https://site.test/missing', extras: { 'data-404-selector': '#nf' }, matches: ['#nf'] });
  assert.strictEqual(p.extra().length, 0, 'checked after the page has rendered');
  p.advance(300);
  p.advance(300);
  assert.strictEqual(p.extra().length, 1);
  assert.strictEqual(p.extra()[0].n, '404');
  assert.strictEqual(p.extra()[0].u, 'https://site.test/missing');
  assert.strictEqual(p.extra()[0].p, undefined);
});

test('404: title marker is a case-insensitive substring', () => {
  const p = page({ extras: { 'data-404-title': 'page not found' }, title: 'Oops - Page Not Found | Site' });
  p.advance(300);
  assert.strictEqual(p.extra().length, 1);
  const q = page({ extras: { 'data-404-title': 'page not found' }, title: 'Pricing' });
  q.advance(300);
  assert.strictEqual(q.extra().length, 0);
});

test('404: no marker configured, no marker matched, or an invalid selector sends nothing', () => {
  const none = page({ extras: {}, title: 'Not Found' });
  none.advance(300);
  assert.strictEqual(none.extra().length, 0);
  const miss = page({ extras: { 'data-404-selector': '#nf' } });
  miss.advance(300);
  assert.strictEqual(miss.extra().length, 0);
  const bad = page({ extras: { 'data-404-selector': '!!invalid' } });
  bad.advance(300);
  assert.strictEqual(bad.extra().length, 0);
  assert.strictEqual(bad.loadError, null);
});

test('404: re-checked on each SPA route, and not if the visitor already left', () => {
  const p = page({ extras: { 'data-404-title': 'not found' }, title: 'Home' });
  p.advance(300);
  p.go('/x');
  p.doc.title = 'Not Found';
  p.advance(300);
  assert.deepStrictEqual(p.extra().map((e) => e.u), ['https://site.test/x']);

  const q = page({ extras: { 'data-404-title': 'not found' }, title: 'Not Found' });
  q.go('/elsewhere'); // left before the check ran
  q.doc.title = 'Fine';
  q.advance(300);
  assert.strictEqual(q.extra().length, 0);
});

// ── form submit ─────────────────────────────────────────────────────────

test('forms: sends the form id (or name) only', () => {
  const p = page({ extras: { 'data-forms': 'true' } });
  p.submit(form({ id: 'newsletter' }));
  p.submit(form({ name: 'contact' }));
  p.submit(form({}));
  const sent = p.extra();
  assert.deepStrictEqual(sent.map((e) => e.n), ['Form Submit', 'Form Submit', 'Form Submit']);
  assert.deepStrictEqual(JSON.parse(sent[0].p), { form: 'newsletter' });
  assert.deepStrictEqual(JSON.parse(sent[1].p), { form: 'contact' });
  assert.strictEqual(sent[2].p, undefined);
});

test('forms: never on a form with a password field, never from a non-form, off by default', () => {
  const p = page({ extras: { 'data-forms': 'true' } });
  p.submit(form({ id: 'login' }, true));
  p.fire('document', 'submit', { target: { tagName: 'DIV', getAttribute: () => 'x' } });
  p.fire('document', 'submit', {});
  assert.strictEqual(p.extra().length, 0);
  const q = page({ extras: {} });
  q.submit(form({ id: 'newsletter' }));
  assert.strictEqual(q.extra().length, 0);
});

test('forms: the payload carries no field data (only a bounded, developer-chosen id)', () => {
  const f = form({ id: 'x'.repeat(500) });
  f.elements = [{ name: 'email', value: 'victim@example.com' }];
  f.action = '/signup?token=s3cret';
  const p = page({ extras: { 'data-forms': 'true' } });
  p.submit(f);
  const body = JSON.stringify(p.extra()[0]);
  for (const leak of ['victim', 'email', 's3cret', 'signup']) assert.ok(!body.includes(leak), leak);
  assert.strictEqual(JSON.parse(p.extra()[0].p).form.length, 64);
});

// ── opt-outs and failure modes ──────────────────────────────────────────

const allOn = { 'data-scroll': 'true', 'data-engagement': 'true', 'data-forms': 'true', 'data-404-title': 'nf' };
function exercise(p) {
  p.advance(5000); p.scroll(3300); p.submit(form({ id: 'f' })); p.advance(300); p.hide();
}

test('opt-out: the mallard_ignore flag silences every extra', () => {
  const p = page({ extras: allOn, ignore: true, title: 'nf' });
  exercise(p);
  assert.strictEqual(p.sent.length, 0);
});

test('opt-out: data-honor-dnt on the main tag applies to the extras', () => {
  const p = page({ main: { 'data-honor-dnt': 'true' }, extras: allOn, dnt: true, title: 'nf' });
  exercise(p);
  assert.strictEqual(p.sent.length, 0);
  const q = page({ extras: allOn, dnt: true, title: 'nf' }); // DNT ignored unless asked for
  exercise(q);
  assert.ok(q.extra().length > 0);
});

test('opt-out: data-exclude and local development apply to the extras', () => {
  const p = page({ url: 'https://site.test/admin/x', main: { 'data-exclude': '/admin/*' }, extras: allOn, title: 'nf' });
  exercise(p);
  assert.strictEqual(p.sent.length, 0);
  const q = page({ url: 'http://localhost:3000/', extras: allOn, title: 'nf' });
  exercise(q);
  assert.strictEqual(q.sent.length, 0);
});

test('data-domain and data-api are the main tracker\'s: extras send to its endpoint and site', () => {
  const p = page({ main: { 'data-domain': 'other.test', 'data-api': 'https://proxy.test/e' }, extras: scrollOn });
  p.scroll(3300); p.hide();
  assert.strictEqual(p.extra()[0].d, 'other.test');
  assert.strictEqual(p.endpoints[p.endpoints.length - 1], 'https://proxy.test/e');
  const q = page({ extras: scrollOn });
  q.scroll(3300); q.hide();
  assert.strictEqual(q.endpoints[q.endpoints.length - 1], 'https://stats.test/api/event');
});

test('without the main tracker nothing is sent and nothing throws', () => {
  const p = page({ noMain: true, extras: allOn, title: 'nf' });
  exercise(p);
  assert.strictEqual(p.loadError, null);
  assert.strictEqual(p.sent.length, 0);
});

test('a stub queue (mallard.q) before the main script loads still receives the events', () => {
  const p = page({ noMain: true, stub: true, extras: scrollOn });
  p.win.mallard = function () { (p.win.mallard.q = p.win.mallard.q || []).push(Array.from(arguments)); };
  p.scroll(3300); p.hide();
  assert.strictEqual(p.win.mallard.q.length, 1);
  assert.strictEqual(p.win.mallard.q[0][0], 'Scroll Depth');
});

test('nothing is written to storage or cookies, whatever the page does', () => {
  const p = page({ extras: allOn, title: 'nf' });
  exercise(p);
  p.go('/b'); p.fire('window', 'pagehide', { persisted: true }); p.fire('window', 'pageshow', { persisted: true });
  assert.deepStrictEqual(p.writes, []);
});

test('errors are swallowed: a throwing tracker never reaches the host page', () => {
  const p = page({ extras: allOn, title: 'nf' });
  p.win.mallard = () => { throw new Error('boom'); };
  assert.doesNotThrow(() => {
    exercise(p);
    p.go('/next');
    p.submit(form({ id: 'f' }));
    p.fire('window', 'pagehide');
  });
  assert.strictEqual(p.win.location.pathname, '/next', 'navigation still happened');
});

test('without a script tag it does nothing', () => {
  const p = page({ extras: {}, noCurrentScript: true });
  assert.strictEqual(p.loadError, null);
});

let failed = 0;
for (const [name, fn] of tests) {
  try { fn(); console.log('ok   ' + name); }
  catch (e) { failed++; console.log('FAIL ' + name + '\n     ' + String(e.stack || e.message).split('\n').slice(0, 3).join('\n     ')); }
}
console.log(`${tests.length - failed}/${tests.length} passed`);
process.exit(failed ? 1 : 0);
