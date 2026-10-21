# Tracker Extras

`/mallard-extras.js` is a second, optional script. It adds four opt-in
behaviours that did not fit in the main tracker's 4,096-byte gzip budget. It is
served exactly like the main tracker (compiled in from
[`tracking/extras.js`](https://github.com/tomtom215/mallardmetrics/blob/main/tracking/extras.js),
`Cache-Control: public, max-age=3600`, CORS open to every origin) and has its own
budget of **2,048 bytes gzipped** (1,830 as built; 4,826 bytes raw), enforced by a
test and by CI.

Outbound-link and file-download tracking already exist in the main tracker
(`data-outbound`, `data-downloads`); the extras do not duplicate them.

## Embed

Load it **after** the main tracker, and enable only what you want:

```html
<script defer src="https://your-instance.com/mallard.js" data-domain="your-site.com"></script>
<script
  defer
  src="https://your-instance.com/mallard-extras.js"
  data-scroll="true"
  data-engagement="true"
  data-forms="true"
  data-404-selector="#not-found"
  data-404-title="page not found">
</script>
```

With no attribute set, the script does nothing.

| Attribute | Event | Properties |
|---|---|---|
| `data-scroll="true"` | `Scroll Depth` | `depth`: `25`, `50`, `75` or `100` |
| `data-engagement="true"` | `Engagement` | `engaged_ms`: milliseconds |
| `data-forms="true"` | `Form Submit` | `form`: the form's `id`, else its `name` (absent if it has neither) |
| `data-404-selector`, `data-404-title` | `404` | none (the path is on the event) |

## How it sends

The extras call the main tracker's `window.mallard()`, which posts to the normal
`/api/event` endpoint. Nothing about them is special to the server: the same
property limits, validation, rate limits and bot filtering apply. It also means
the extras take their configuration from the main tag and **need it on the
page**:

- `data-domain`, `data-api`, `data-exclude`, `data-honor-dnt`, `data-include-local`
  and the `mallard_ignore` opt-out all apply unchanged, because the main tracker
  makes the decision. Set them on the main tag, not on the extras tag.
- `data-hash` on the main tag also tells the extras to treat hash routes as pages.
- Without the main tracker, the extras send nothing (and throw nothing). Calls
  queued by the `window.mallard` stub snippet are replayed when it loads.

The extras write nothing: no cookies, no `localStorage`, no `sessionStorage`. They
send nothing a visitor typed and swallow every error, so they cannot break your
page.

## Scroll depth

One event per page view, carrying the deepest of 25/50/75/100 % reached, where
the percentage is `(scroll offset + viewport height) / document height`. No
event is sent if the visitor never reached 25 %. A page that fits in the viewport
counts as 100 % at load. The document height is read when the visitor scrolls, so
content that loads later raises the bar rather than being ignored.

## Engagement time

`engaged_ms` is the time the page was **visible and the window focused**. There
are no timers: the script adds up the intervals between the page becoming active
and becoming inactive, so nothing runs while a tab is hidden. It is capped at 30
minutes (a tab left open overnight is not 8 hours of engagement) and is not sent
below 1 second.

## 404 detection

Client-side JavaScript cannot read the HTTP status of the page it is running on,
so this is a heuristic you configure, not a measurement. The `404` event is sent
once per page view when either marker matches, checked 300 ms after load (and
after each single-page-app route change) to let the not-found view render:

- `data-404-selector`: a CSS selector that exists only on your not-found page,
  such as `#not-found` or `body.error-404`. An invalid selector never matches.
- `data-404-title`: text looked for, case-insensitively, anywhere in
  `document.title`, such as `page not found`.

Pick markers that are specific to your error page. A server that answers 404
with a page lacking both markers, or that serves a soft 404 with a 200 status and
the markers, is reported according to the markers, not the status. Because the
event carries the path, `/api/stats/property-values` and the goals list show
which missing URLs are hit.

## Form submits

`Form Submit` is sent when a `<form>` is submitted and passes the browser's
validation. The only thing sent is the form's `id` attribute (or `name`), cut to
64 characters; field names, values and the form action are never read. A form
that contains an `input[type=password]` is never reported. Forms submitted by
calling `form.submit()` from script fire no `submit` event and are not seen.

## Single-page apps, back/forward cache and tab switches

- A route change (the same `history` signals the main tracker listens to:
  `pushState`, `replaceState`, `popstate`, plus `hashchange` with `data-hash`)
  ends the previous page view: its scroll depth and engagement are sent under the
  **previous** URL, and a fresh page view begins.
- Scroll depth and engagement are sent when the page first becomes hidden
  (`visibilitychange`, which is reliable on mobile) or on `pagehide`, whichever
  comes first, with `sendBeacon` so it survives unload. They are sent **once**
  per page view: a visitor who switches tabs and comes back to keep reading is
  reported as of the first hide.
- A page restored from the back/forward cache is a new page view, as it is for
  the main tracker.

## What they do to your numbers

- **Bounce rate.** A visit with one pageview and any other event is not a bounce
  (see [Metric Definitions](metric-definitions.md)), and the extras' events are
  ordinary events. With `data-engagement` on, almost every visit that lasts a
  second sends one, so **bounce rate will fall sharply** and no longer be
  comparable with earlier data. `data-scroll` has the same effect on every visit
  that reaches 25 %. Decide whether you want that before enabling them.
- **Visit duration** improves for the same reason: the extra event is timestamped
  when the visitor leaves.
- **Goals.** Every event name other than `pageview` appears in the goals list, so
  `Scroll Depth`, `Engagement`, `404` and `Form Submit` will show up there with
  a conversion rate. That is expected.
- **Event counts** rise: expect up to two extra events per page view with both
  `data-scroll` and `data-engagement`, which counts toward `rate_limit_per_ip`.
