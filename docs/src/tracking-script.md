# Tracking Script

The tracking script is served from `GET /mallard.js` (and `GET /js/script.js`,
an alias for people migrating from Plausible). It sets no cookies, writes nothing
to browser storage, and is about 3.8 KB over the wire once gzipped.

The script is compiled into the binary from a single source file,
[`tracking/script.js`](https://github.com/tomtom215/mallardmetrics/blob/main/tracking/script.js).
Reading that file is the fastest way to see exactly what runs on your visitors'
browsers — it is deliberately kept short and readable rather than minified.

## Basic embed

```html
<script
  defer
  src="https://your-instance.com/mallard.js"
  data-domain="your-site.com">
</script>
```

## Attributes

| Attribute | Default | Description |
|---|---|---|
| `data-domain` | (required) | The site ID events are recorded under. Must be listed in `site_ids` when that option is set. |
| `data-api` | `<script origin>/api/event` | Full ingest endpoint URL, for a proxied or custom path. |
| `data-exclude` | (none) | Comma-separated path patterns to skip. `*` is a wildcard, e.g. `/admin/*,/preview/*`. |
| `data-include-local` | `false` | Also send from `localhost`, `file:` and private addresses. Off by default so local development does not pollute production data. |
| `data-honor-dnt` | `false` | Skip tracking when the browser sends Do Not Track or Global Privacy Control. |
| `data-hash` | `false` | Treat `hashchange` as a pageview, for hash-based routers. |
| `data-outbound` | `false` | Record clicks on links to other origins as `Outbound Link: Click`. |
| `data-downloads` | `false` | Record clicks on file links as `File Download`. |
| `data-download-ext` | (common formats) | Override the extensions treated as downloads, e.g. `pdf,zip,csv`. |

## What is sent

A pageview carries only what an aggregate report needs:

| Field | Source |
|---|---|
| `d` | `data-domain` |
| `n` | Event name (`pageview`, or your own) |
| `u` | `window.location.href` — the server keeps only the path |
| `r` | `document.referrer`, **only when it is from another origin** |
| `w` | `window.innerWidth` |

Same-origin referrers are dropped in the browser. Sending them would make every
internal navigation look like an acquisition, and your own site would appear as
its own top traffic source.

The User-Agent is read from the request header server-side and parsed into
browser and OS names. Where the browser sends
[User-Agent Client Hints](https://developer.mozilla.org/en-US/docs/Web/HTTP/Client_hints),
the low-entropy ones (`Sec-CH-UA`, `Sec-CH-UA-Platform`, `Sec-CH-UA-Mobile`) are
preferred, because Chrome freezes its legacy UA string. The high-entropy hints
are never requested.

## Automatic pageviews

A pageview fires on load, and again whenever the path or query string changes
through `history.pushState`, `history.replaceState`, or the back and forward
buttons. `replaceState` matters: most routers use it for filter and query
changes, and those navigations were previously invisible.

Repeated navigations to the same path do not double-count.

A page the browser is *prerendering* is not counted while it is hidden — the
visitor may never look at it. If the prerender is activated, the pageview fires
then, once.

## Custom events

```javascript
mallard('signup');

mallard('purchase', {
  props: { plan: 'pro', coupon: 'SAVE20' },
  revenue: 99.0,
  currency: 'USD',
});

mallard('form_submit', {
  props: { form: 'contact' },
  callback: function (result) {
    console.log('recorded', result.status);
  },
});
```

| Option | Type | Description |
|---|---|---|
| `props` | object | Custom properties, stored as JSON. Must be an object; anything else is dropped server-side. Query them at `/api/stats/property-values`. |
| `revenue` | number | Revenue amount, stored as `DECIMAL(12,2)`. |
| `currency` | string | ISO 4217 alphabetic code, e.g. `"USD"`. Validated and uppercased. |
| `url` | string | Override the recorded URL. |
| `referrer` | string \| null | Override the referrer. |
| `callback` | function | Called with `{ status }` once the request completes. |

Custom events appear in `/api/stats/goals` with their conversion rate, and in the
`events` breakdown dimension.

### Calls before the script loads

Add the standard stub so nothing is lost while the script is still loading:

```html
<script>
  window.mallard = window.mallard || function () {
    (window.mallard.q = window.mallard.q || []).push(arguments);
  };
</script>
<script defer src="https://your-instance.com/mallard.js" data-domain="your-site.com"></script>
```

Queued calls are replayed once the real implementation takes over.

The script reads its configuration from its own `<script>` tag. It normally
finds that tag through `document.currentScript`; when the tag is injected
dynamically and runs asynchronously — as a tag manager does — it falls back to
the first `script[data-domain]` on the page.

## Outbound links and downloads

Set `data-outbound` and `data-downloads` rather than writing your own handlers:

```html
<script
  defer
  src="https://your-instance.com/mallard.js"
  data-domain="your-site.com"
  data-outbound="true"
  data-downloads="true">
</script>
```

Both use `navigator.sendBeacon`, which survives the page unload that follows the
click — so no navigation delay is needed and no clicks are lost.

## Opting out

Two mechanisms, both entirely client-side:

```javascript
// Stop this browser from being counted, e.g. your own visits.
localStorage.setItem('mallard_ignore', 'true');
```

```html
<!-- Honour Do Not Track / Global Privacy Control. -->
<script ... data-honor-dnt="true"></script>
```

Localhost, `file:` URLs and private network addresses are excluded by default;
set `data-include-local="true"` if you are deliberately testing against a local
instance.

## Transport

`navigator.sendBeacon` is used where available, so a pageview fired immediately
before navigating away is not cancelled by the navigation. Requests that need a
`callback` fall back to `XMLHttpRequest`, since `sendBeacon` reports no status.

Because the beacon survives unload, outbound-link and download clicks need no
navigation delay, and a modified click (open-in-new-tab, middle click) needs no
special handling — it is recorded like any other.

## Server-side events

The script is optional. Any client can post to the ingest endpoint:

```bash
curl -X POST https://your-instance.com/api/event \
  -H 'Content-Type: application/json' \
  -d '{
    "d": "your-site.com",
    "n": "signup",
    "u": "https://your-site.com/signup"
  }'
```

There is also a pixel endpoint for contexts without JavaScript, such as HTML
email:

```html
<img src="https://your-instance.com/api/event?d=your-site.com&n=email_open&u=https%3A%2F%2Fexample.com%2Fnewsletter" width="1" height="1" alt="">
```

Note that server-side and pixel requests carry no `Origin` header. When
`site_ids` is configured, the allowlist is enforced against the payload's
`d` field as well, so an unlisted site is rejected either way.

See [Event Ingestion API](api-reference/ingestion.md) for the full schema.
