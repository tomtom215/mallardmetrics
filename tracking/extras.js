/*!
 * Mallard Metrics optional extras (load after mallard.js; see docs/src/extras.md).
 * Opt-in by attribute: data-scroll, data-engagement, data-forms,
 * data-404-selector, data-404-title. Events go through the main tracker's
 * window.mallard(), so its opt-outs apply. Writes no storage or cookies; sends
 * nothing a visitor typed; swallows every error.
 */
(function (window, document) {
  'use strict';

  var script =
    document.currentScript || document.querySelector('script[src*="mallard-extras"]');
  if (!script) return;

  var main = document.querySelector('script[data-domain]');
  var hashRoutes = !!main && main.getAttribute('data-hash') === 'true';
  var wantScroll = script.getAttribute('data-scroll') === 'true';
  var wantTime = script.getAttribute('data-engagement') === 'true';
  var wantForms = script.getAttribute('data-forms') === 'true';
  var sel404 = script.getAttribute('data-404-selector');
  var title404 = script.getAttribute('data-404-title');
  var CAP = 1800000;
  var loc = window.location;
  var page;

  function guard(fn) {
    return function () {
      try {
        return fn.apply(this, arguments);
      } catch (e) {
        // never break the host page
      }
    };
  }

  function send(name, props, url) {
    if (typeof window.mallard === 'function') {
      window.mallard(name, { props: props, url: url, referrer: null });
    }
  }

  // The route the main tracker reports.
  function pageUrl() {
    var h = loc.hash;
    if (!hashRoutes || !/^#\//.test(h)) return loc.href;
    return (
      loc.origin + loc.pathname.replace(/\/$/, '') + h.slice(1) +
      (loc.search && h.indexOf('?') < 0 ? loc.search : '')
    );
  }

  function pageKey() {
    return loc.pathname + loc.search + (hashRoutes ? loc.hash : '');
  }

  function active() {
    return document.visibilityState === 'visible' && document.hasFocus();
  }

  function pause() {
    if (page.at) {
      page.ms += Date.now() - page.at;
      page.at = 0;
    }
  }

  function resume() {
    if (!page.at && !page.sent && active()) page.at = Date.now();
  }

  function measure() {
    if (!wantScroll || page.sent || page.depth === 100) return;
      var pct =
      ((window.pageYOffset + window.innerHeight + 2) /
        (document.documentElement.scrollHeight || 1)) * 100;
    var b = pct >= 100 ? 100 : pct >= 75 ? 75 : pct >= 50 ? 50 : pct >= 25 ? 25 : 0;
    if (b > page.depth) page.depth = b;
  }

  function notFound() {
    try {
      if (sel404 && document.querySelector(sel404)) return true;
    } catch (e) {
      // an invalid selector simply never matches
    }
    return !!title404 && document.title.toLowerCase().indexOf(title404.toLowerCase()) >= 0;
  }

  function flush() {
    if (page.sent) return;
    pause();
    page.sent = true;
    if (page.depth) send('Scroll Depth', { depth: page.depth }, page.url);
    var ms = Math.min(page.ms, CAP);
    if (wantTime && ms >= 1000) send('Engagement', { engaged_ms: ms }, page.url);
  }

  function start() {
    var p = (page = { key: pageKey(), url: pageUrl(), depth: 0, ms: 0, at: 0, sent: false });
    resume();
    measure();
    if (sel404 || title404) {
      setTimeout(guard(function () {
        if (p === page && notFound()) send('404', undefined, p.url);
      }), 300);
    }
  }

  var changed = guard(function () {
    if (pageKey() === page.key) return;
    flush();
    start();
  });

  ['pushState', 'replaceState'].forEach(function (m) {
    var original = window.history && window.history[m];
    if (typeof original !== 'function') return;
    window.history[m] = function () {
      var result = original.apply(this, arguments);
      changed();
      return result;
    };
  });
  window.addEventListener('popstate', changed);
  if (hashRoutes) window.addEventListener('hashchange', changed);

  window.addEventListener('scroll', guard(measure), { passive: true });
  window.addEventListener('focus', guard(resume));
  window.addEventListener('blur', guard(function () { pause(); }));
  document.addEventListener('visibilitychange', guard(function () {
    if (document.visibilityState === 'hidden') flush();
    else resume();
  }));
  window.addEventListener('pagehide', guard(flush));
  // A page restored from the back/forward cache is a new page view.
  window.addEventListener('pageshow', guard(function (e) {
    if (e.persisted) start();
  }));

  if (wantForms) {
    document.addEventListener('submit', guard(function (e) {
      var f = e.target;
      if (!f || f.tagName !== 'FORM' || f.querySelector('input[type=password]')) return;
      // getAttribute: `f.id` can be a field named "id".
      var id = f.getAttribute('id') || f.getAttribute('name');
      send('Form Submit', id ? { form: String(id).slice(0, 64) } : undefined);
    }), true);
  }

  guard(start)();
})(window, document);
