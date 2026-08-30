/*!
 * Mallard Metrics tracking script.
 *
 * Privacy-first: no cookies, no localStorage writes, no device fingerprinting,
 * no cross-site identifiers. The only state read is an explicit opt-out flag.
 *
 * Usage:
 *   <script defer src="https://analytics.example.com/mallard.js"
 *           data-domain="example.com"></script>
 *
 * Attributes (all optional except data-domain):
 *   data-api            Full ingest endpoint URL. Default: <script origin>/api/event
 *   data-exclude        Comma-separated path patterns to skip. "*" is a wildcard,
 *                       e.g. "/admin/*,/preview/*"
 *   data-include-local  "true" to also send from localhost / file: / private IPs.
 *                       Off by default so local development does not pollute data.
 *   data-honor-dnt      "true" to skip tracking when the browser sends
 *                       Do Not Track / Global Privacy Control.
 *   data-hash           "true" to treat hash changes as pageviews (hash routers).
 *   data-outbound       "true" to auto-track clicks on links to other origins
 *                       as an "Outbound Link: Click" event.
 *   data-downloads      "true" to auto-track clicks on file links as a
 *                       "File Download" event. Extensions can be overridden with
 *                       data-download-ext="pdf,zip,csv".
 *
 * Manual events:
 *   mallard('signup')
 *   mallard('purchase', { revenue: 49.99, currency: 'USD', props: { plan: 'pro' } })
 *   mallard('signup', { callback: fn })
 *
 * Opt out on a single browser (e.g. for site owners):
 *   localStorage.setItem('mallard_ignore', 'true')
 */
(function (window, document) {
  'use strict';

  // `currentScript` is null when the tag is injected dynamically and runs
  // asynchronously — a tag manager, for instance. Falling back to the tag's own
  // marker attribute keeps those deployments working instead of silently
  // disabling the tracker, queue drain included.
  var script =
    document.currentScript || document.querySelector('script[data-domain]');
  if (!script) return;

  var domain = script.getAttribute('data-domain');
  var endpoint =
    script.getAttribute('data-api') ||
    new URL(script.src, document.baseURI).origin + '/api/event';

  var DEFAULT_DOWNLOAD_EXT =
    '7z,avi,csv,dmg,doc,docx,exe,gz,key,mid,midi,mkv,mp3,mp4,mpeg,mpg,msi,' +
    'ogg,pdf,pkg,pps,ppt,pptx,rar,rtf,tar,tgz,txt,wav,wma,wmv,xls,xlsx,xml,zip';

  function attr(name) {
    return script.getAttribute(name);
  }
  function flag(name) {
    return attr(name) === 'true';
  }

  var excludePatterns = (attr('data-exclude') || '')
    .split(',')
    .map(function (p) { return p.trim(); })
    .filter(Boolean);

  var downloadExtensions = (attr('data-download-ext') || DEFAULT_DOWNLOAD_EXT)
    .split(',')
    .map(function (e) { return e.trim().toLowerCase().replace(/^\./, ''); })
    .filter(Boolean);

  /* --- Opt-out and environment checks ------------------------------------ */

  function optedOut() {
    try {
      return window.localStorage.getItem('mallard_ignore') === 'true';
    } catch (e) {
      // Storage can throw in private mode or when site data is blocked.
      return false;
    }
  }

  function doNotTrack() {
    if (!flag('data-honor-dnt')) return false;
    var n = window.navigator;
    return (
      n.globalPrivacyControl === true ||
      n.doNotTrack === '1' ||
      n.doNotTrack === 'yes' ||
      window.doNotTrack === '1'
    );
  }

  var LOCAL_HOST_RE =
    /^(localhost|127(\.\d+){3}|\[?::1\]?|10(\.\d+){3}|192\.168(\.\d+){2}|172\.(1[6-9]|2\d|3[01])(\.\d+){2}|.*\.local)$/i;

  function isLocalEnvironment() {
    if (flag('data-include-local')) return false;
    return (
      window.location.protocol === 'file:' ||
      LOCAL_HOST_RE.test(window.location.hostname)
    );
  }

  // Simple '*' glob match, anchored at both ends.
  function matchesPattern(path, pattern) {
    var parts = pattern.split('*').map(function (p) {
      return p.replace(/[.+?^${}()|[\]\\]/g, '\\$&');
    });
    return new RegExp('^' + parts.join('.*') + '$').test(path);
  }

  function isExcluded(path) {
    for (var i = 0; i < excludePatterns.length; i++) {
      if (matchesPattern(path, excludePatterns[i])) return true;
    }
    return false;
  }

  /* --- Referrer ----------------------------------------------------------- */

  // Same-origin referrers describe internal navigation, not an acquisition
  // source. Sending them would make the site itself the top traffic source.
  function externalReferrer() {
    var ref = document.referrer;
    if (!ref) return null;
    try {
      if (new URL(ref).host === window.location.host) return null;
    } catch (e) {
      return null;
    }
    return ref;
  }

  /* --- Transport ---------------------------------------------------------- */

  function send(body, callback) {
    var payload = JSON.stringify(body);

    // sendBeacon survives page unload, so a pageview fired immediately before
    // navigating away is not cancelled. It gives no status back, so requests
    // needing a callback fall through to fetch/XHR.
    if (!callback && navigator.sendBeacon) {
      try {
        if (navigator.sendBeacon(endpoint, new Blob([payload], { type: 'application/json' }))) {
          return;
        }
      } catch (e) {
        // fall through to XHR
      }
    }

    var xhr = new XMLHttpRequest();
    xhr.open('POST', endpoint, true);
    xhr.setRequestHeader('Content-Type', 'application/json');
    if (callback) {
      xhr.onreadystatechange = function () {
        if (xhr.readyState === 4) callback({ status: xhr.status });
      };
    }
    xhr.send(payload);
  }

  /* --- Event API ---------------------------------------------------------- */

  function track(name, options) {
    options = options || {};

    if (document.visibilityState === 'prerender') return;
    if (optedOut() || doNotTrack() || isLocalEnvironment()) return;
    if (!domain) return;

    var url = options.url || window.location.href;
    var path;
    try {
      path = new URL(url, document.baseURI).pathname;
    } catch (e) {
      path = window.location.pathname;
    }
    if (isExcluded(path)) return;

    var body = {
      d: domain,
      n: name,
      u: url,
      r: options.referrer !== undefined ? options.referrer : externalReferrer(),
      w: window.innerWidth
    };
    if (options.props) body.p = JSON.stringify(options.props);
    if (options.revenue != null) body.ra = options.revenue;
    if (options.currency) body.rc = options.currency;

    send(body, options.callback);
  }

  function pageview() {
    track('pageview');
  }

  // A prerendered page runs the script but must not be counted: the visitor may
  // never look at it. `track` drops the event, so re-fire once (and only once)
  // if the prerender is later activated, or the visit is lost entirely.
  function pageviewWhenVisible() {
    if (document.visibilityState !== 'prerender') {
      pageview();
      return;
    }
    var onVisible = function () {
      if (document.visibilityState === 'prerender') return;
      document.removeEventListener('visibilitychange', onVisible);
      pageview();
    };
    document.addEventListener('visibilitychange', onVisible);
  }

  /* --- Automatic pageviews ------------------------------------------------ */

  var lastPage = window.location.pathname + window.location.search;

  function pageviewIfChanged() {
    var current = window.location.pathname + window.location.search;
    if (current === lastPage) return;
    lastPage = current;
    pageview();
  }

  function wrapHistory(method) {
    var original = window.history[method];
    if (typeof original !== 'function') return;
    window.history[method] = function () {
      var result = original.apply(this, arguments);
      pageviewIfChanged();
      return result;
    };
  }

  if (window.history && window.history.pushState) {
    wrapHistory('pushState');
    // replaceState is what most routers use for filter/query changes; without
    // it those navigations were invisible.
    wrapHistory('replaceState');
    window.addEventListener('popstate', pageviewIfChanged);
  }

  if (flag('data-hash')) {
    window.addEventListener('hashchange', pageview);
  }

  /* --- Automatic outbound-link and download tracking ---------------------- */

  function fileExtension(pathname) {
    var match = /\.([a-z0-9]+)$/i.exec(pathname);
    return match ? match[1].toLowerCase() : null;
  }

  function handleClick(event) {
    // Modified clicks (open-in-new-tab, middle click) need no special handling:
    // `sendBeacon` survives the unload that follows a same-tab navigation, so
    // the click is never delayed and never lost either way.
    var link = event.target && event.target.closest && event.target.closest('a[href]');
    if (!link) return;

    var href = link.getAttribute('href');
    if (!href) return;

    var target;
    try {
      target = new URL(link.href, document.baseURI);
    } catch (e) {
      return;
    }
    if (target.protocol !== 'http:' && target.protocol !== 'https:') return;

    var isOutbound = target.host !== window.location.host;
    var ext = fileExtension(target.pathname);
    var isDownload = ext !== null && downloadExtensions.indexOf(ext) !== -1;

    if (flag('data-outbound') && isOutbound) {
      track('Outbound Link: Click', { props: { url: target.href } });
    }
    if (flag('data-downloads') && isDownload) {
      track('File Download', { props: { url: target.href } });
    }
  }

  if (flag('data-outbound') || flag('data-downloads')) {
    document.addEventListener('click', handleClick, true);
  }

  /* --- Public API --------------------------------------------------------- */

  // Drain any calls queued before this script finished loading, following the
  // window.mallard = window.mallard || function(){ (mallard.q = mallard.q || []).push(arguments) }
  // snippet convention.
  var queued = window.mallard && window.mallard.q;
  window.mallard = track;
  if (queued) {
    for (var i = 0; i < queued.length; i++) {
      track.apply(null, queued[i]);
    }
  }

  pageviewWhenVisible();
})(window, document);
