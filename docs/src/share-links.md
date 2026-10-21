# Share Links and Saved Segments

A **share link** gives someone a read-only view of one site's dashboard without
a login. An admin creates it, can limit it to a [saved segment](#saved-segments)
and an expiry date, and can revoke it at any time. This page is the design and
its threat model: what a link lets a holder do, what it cannot, and which leaks
it does not stop.

A share link is a bearer secret. Whoever holds the link sees what it shows.
Everything below follows from taking that sentence seriously.

## Contents

- [Using share links](#using-share-links)
- [Threat model](#threat-model)
- [How the token travels](#how-the-token-travels)
- [How a request is confined](#how-a-request-is-confined)
- [Lifetime and storage](#lifetime-and-storage)
- [Limits](#limits)
- [What is not protected](#what-is-not-protected)
- [Saved segments](#saved-segments)
- [API](#api)

## Using share links

In the dashboard, an admin opens **Share links**, gives the link a private
label, optionally picks a segment and an expiry, and chooses **Create link**.
The link appears once:

```
https://analytics.example.com/#share=ms_1f0c...  (64 hex digits after ms_)
```

Copy it then. The server keeps only a hash, so it cannot show the link again;
if it is lost, revoke it and make another. The same panel lists every link with
its label, site, segment, expiry, last use and status, and revokes with one click.

Someone who opens the link sees the normal dashboard for that site, marked
**Shared view**, with these things absent: the site menu, the install snippet,
API keys, share links, segments, backup, sign-out and every export. They can
change the date range, compare periods, click values to add filters and open
every report panel. Anything they add narrows the view further; nothing they do
widens it.

## Threat model

| Who | Can | Cannot |
|---|---|---|
| **Anyone on the internet** with no link | Reach the public dashboard shell (static files) and get `401` from every share route. Guessing tokens is the only attack, and it is limited to 5 refused credentials per second per client network against a 256-bit space | Learn whether a link exists, was revoked, or expired: every failure is the same `401` |
| **Holder of a valid link** | Read every report for the link's site (and segment) over any range up to 366 days, including history from before the link existed, custom event names, **custom property values**, referrers, pages, revenue and the realtime panel. Narrow the view with filters. Spend query capacity, bounded by the [rate limits](#limits) | Name another site (`403`); remove or widen the segment; export anything; list sites, segments or other links; reach keys, erasure, backup, auth or any write; use the token as an API key |
| **Former holder** (link revoked or expired) | Keep whatever they already saw or saved | Make one more request: both take effect on the next request |
| **Admin** | Create, list and revoke links and segments | Read a token back after creation |
| **Someone who reads the data directory** (backup, disk image) | Read `share_links.json`: labels, sites, segments, SHA-256 hashes | Recover a token from it (256 bits of randomness leave nothing to search) |
| **Operator, reverse proxy, log reader** | See requests and, if configured to, headers | Find the token in a URL, in Mallard's logs or in `/metrics` ([below](#how-the-token-travels)) |
| **A page the viewer navigates to** | See the origin as a `Referer` only if the browser sends one | Receive the token: it is never in a URL the server sees, and share responses say `Referrer-Policy: no-referrer` |

**A holder can drill down.** The dashboard's filters apply to every report, so
a viewer inside a segment can combine filters until a count reaches one visitor
and read that visitor's page, browser, operating system, referrer and country.
That is the same power any dashboard user has; a share link is not an
aggregates-only view. Restricting a link to a segment narrows what exists to be
drilled into, which is the control you have. Do not give a link to someone you
would not give the dashboard to for that segment.

## How the token travels

Three ways to carry a bearer token in a link were compared.

| | Where the token is | Reaches server access logs and proxies | In `Referer` | Server state | Survives reload |
|---|---|---|---|---|---|
| **A. Path or query token** (`/s/<token>`) | the request URL | **yes**, every request, in Mallard's request log and any proxy's | unless `no-referrer` is set everywhere it matters | none | yes |
| **B. Server bootstrap**: `GET /s/<token>` sets a cookie and redirects | URL once, then a cookie | **yes**, the first request | the redirect target only | a session per viewer | yes |
| **C. Fragment, then header** (chosen): `/#share=<token>` | the URL fragment, then an `X-Share-Token` header on same-origin API calls | **no**, the fragment is never sent; the header is not logged | **never**, fragments are not part of a `Referer` | none | yes, while the fragment is in the address bar |

**Chosen: C.** The decisive property is that the token never appears in a URL
that the server, a CDN or a proxy sees, so there is no log, trace or metric
label to redact it from. A and B put it in the one place the request logging of
every layer in front of the dashboard records by default, and the defence is
then a list of places that must all remember to redact. C has no such list.

What C costs, stated plainly:

- The viewer's browser needs JavaScript. The dashboard already does.
- The token is part of the link as shared, so it lives in the sender's and the
  recipient's browser history, bookmarks, synced devices, clipboard managers and
  anything that records the address bar. Every URL-borne secret has this
  property; the fragment does not remove it. The dashboard does not rewrite the
  address bar, so that a reload and a bookmark keep working.
- The token is held in page memory only. It is never written to `localStorage`
  or `sessionStorage`. A script running on the dashboard's own origin (an XSS
  bug) could read it, as it could act as a logged-in admin; `script-src 'self'`
  and the absence of any user-controlled markup are what stand in the way.
- A client without JavaScript can still use the API directly:
  `curl -H 'X-Share-Token: ms_...' https://host/api/shared/stats/main?period=7d`.

Defence in depth on top of C, each with a test:

- **Responses.** Every response from a share route, errors included, carries
  `Referrer-Policy: no-referrer`, `Cache-Control: no-store` and
  `X-Robots-Tag: noindex, nofollow`. The server-wide `Referrer-Policy` no longer
  overwrites a policy a response set for itself.
- **Logs.** Mallard's request log records the URI of each request in a span.
  That span now passes the URI through a redaction that replaces anything shaped
  like a share token with `ms_[redacted]`, for the day someone pastes a token
  into a URL anyway. Request headers are not logged. The messages about
  links (created, revoked, refused) carry the link's `id`, never the token.
- **Metrics.** `/metrics` has no label derived from a request; there is nothing
  to leak. A share request is counted in the same status-class totals as any other.
- **Errors.** Error bodies never echo a credential; the tests present tokens in
  every place a credential could be read from and search every response.
- **CORS.** Share routes send no `Access-Control-Allow-*` headers, so a page on
  another origin can neither send the token header (the preflight fails) nor read
  a response. `X-Frame-Options: DENY` already stops embedding.
- **Credentials.** The browser sends the token with `credentials: 'omit'` and
  `referrerPolicy: 'no-referrer'`.

The reverse proxy is yours. Mallard cannot stop a proxy that logs request
*headers* from logging `X-Share-Token`. The bundled Caddy configuration logs no
headers; check any other proxy.

## How a request is confined

A share token is **not an API key**. The routes it opens are mounted in their
own router, outside the layers that check sessions and keys, and the credential
checks of those layers do not recognise it. A route is reachable with a token
only if it is listed in `view_routes` in `src/api/share.rs`:

| Share route | Same report as |
|---|---|
| `GET /api/shared/session` | what the viewer's dashboard needs: the site, the segment's name, the expiry |
| `GET /api/shared/stats/main` | `/api/stats/main` |
| `GET /api/shared/stats/timeseries` | `/api/stats/timeseries` |
| `GET /api/shared/stats/sessions` | `/api/stats/sessions` |
| `GET /api/shared/stats/realtime` | `/api/stats/realtime` |
| `GET /api/shared/stats/revenue` | `/api/stats/revenue` |
| `GET /api/shared/stats/goals` | `/api/stats/goals` |
| `GET /api/shared/stats/properties` | `/api/stats/properties` |
| `GET /api/shared/stats/property-values` | `/api/stats/property-values` |
| `GET /api/shared/stats/breakdown/{dimension}` | `/api/stats/breakdown/{dimension}` |
| `GET /api/shared/stats/funnel`, `retention`, `sequences`, `flow` | the same names under `/api/stats/` |

Absent on purpose: `export` (daily, raw, CSV, JSON), `/api/sites`, `/api/keys`,
`/api/gdpr/erase`, `/api/admin/backup`, `/api/auth/*`, `/api/segments` and
`/api/share-links`. Each answers `404` on the share prefix and `401` when the
token is offered to the ordinary route.

Before a handler runs, the gate does four things, in this order:

1. **Authenticate.** The `X-Share-Token` header is checked for shape, hashed
   with SHA-256 and compared with every stored hash in constant time without
   stopping at a match. A missing, malformed, unknown, revoked or expired token
   is `401 share_link_invalid`. Only refused credentials spend the per-client
   failure budget, so someone guessing tokens cannot use up what viewers need.
2. **Rate-limit.** Per link, then across all links ([Limits](#limits)).
3. **Confine the query.** `site_id` must be absent or exactly the link's site;
   naming any other site (any spelling, any capitalisation, a trailing dot) is
   `403 share_site_forbidden`, **not** silently rewritten. `site_id` and
   `filters` may each appear once: a repeated parameter means whichever parser
   wins decides, so it is `400`. The link's segment filters are put in front of
   the viewer's own, joined with `;`. The server ANDs every condition, so a
   viewer can add conditions but cannot remove the segment's, blank it, contradict
   it into showing other data, or widen it with a negation: `browsers!=Chrome`
   inside a Chrome-only link returns nothing, not Firefox.
4. **Rebuild the query string** from the values just checked, percent-encoded
   afresh, so the handler's parser sees exactly what the gate verified and no
   difference between two parsers can be used to slip a value past it.

The handlers are the ordinary ones, so every report honours the segment because
every report already honours `filters`; `tests/share_test.rs` checks the data a
viewer receives from each report type against rows deliberately outside the
segment, rather than trusting the status code.

The query cache is keyed by site and by the parsed filters, so a link with a
segment, a link without one and an admin's own view never share an entry.

A segment is **copied into the link when it is made**. Editing or deleting the
segment afterwards changes nothing about links already issued; a later edit
cannot widen an existing link. The link's listing shows the filters it holds.

## Lifetime and storage

- **Token.** `ms_` followed by 32 bytes (256 bits) read directly from the
  operating system's CSPRNG (`getrandom`), hex encoded. A failure to read
  entropy is an error, never a weaker token.
- **Stored.** Only `SHA-256("share-link/v1\0" + token)`. SHA-256 is enough
  because the input has 256 bits of entropy: there is nothing to brute-force,
  so a deliberately slow hash would only slow every request.
- **Where.** `<data_dir>/share_links.json` (saved segments in `segments.json`),
  mode 0600, written to a temporary file in the same directory, flushed and
  renamed into place, with the directory flushed too. Leftover temporary files
  from a crash are removed at start.
- **Corrupt file.** A file that cannot be parsed is moved to
  `share_links.json.corrupt-<unix time>` and the store starts empty. Nothing
  overwrites the copy, and an empty store **authorises nothing**: it fails
  closed. Recreate the links you need.
- **Expiry** is optional, in whole days from 1 to 730, and is checked against
  the clock on every request. A link is refused at the instant it expires, with
  no grace period.
- **Revocation** takes effect on the next request. The revocation is applied in
  memory *before* the file is written, and it stays in force if the write fails;
  in that case the API answers `500` and says the link may work again after a
  restart, so you know to retry. Revoked and expired links stay listed for 30
  days and are then dropped.
- **Last used** is kept in memory and written to disk at most every ten
  minutes (and with any other change), so a crash can lose up to ten minutes of
  it. It is a property of the link, not of a person.

## Limits

| What | Limit |
|---|---|
| Active links (not revoked, not expired) per instance | 100 (`409` beyond) |
| Link records kept, revoked and expired included | 500 (the oldest inactive ones are dropped) |
| Link label | 1-128 characters, no control characters |
| Link lifetime | 1-730 days, or none |
| Requests per link | 30 per second (a dashboard load is about 15 requests) |
| Requests across all links | 120 per second |
| Refused credentials per client network | 5 per second (IPv6 clients by /64) |
| Saved segments | 50 per site, 500 per instance |
| Segment name | 1-64 characters, unique per site, ignoring case |
| Segment filters | 1-8 conditions, at most 4096 bytes in total |

A limited request gets `429` with `Retry-After: 1`. The per-link and global
limits count only valid tokens; the refused-credential limit counts only invalid
ones. These are constants in `src/api/share.rs`; they are not configuration
settings. Analytics queries from share links also share the instance-wide
`max_concurrent_queries` slots with everyone else.

## What is not protected

Be clear-eyed about these.

- **A leaked link is a leaked dashboard** for that site or segment until you
  revoke it. Forwarding, a screenshot of the address bar, a synced browser
  history, a shoulder-surfer, a link in a chat that is later exported: all work.
  There is no way to bind a link to a person, and no per-viewer audit; "last
  used" tells you only that someone used it.
- **Transport security is yours.** Over plain HTTP the header is readable by
  anyone on the path. Run behind TLS (see [Deployment](deployment.md)). The
  failure-budget key follows `trust_proxy_headers`; behind a proxy that is not
  trusted every viewer shares the proxy's address, which only affects clients
  presenting *invalid* tokens.
- **A compromised admin session can mint links** that outlive the session. After
  a suspected compromise, list the links and revoke every one you did not make.
  There is no password-change endpoint to invalidate them as a side effect.
- **Backups restore old links.** `share_links.json` is part of an online backup.
  Restoring a backup taken before you revoked a link makes that link valid
  again. After a restore, list the links and revoke what should be dead.
- **The file is read at start.** Editing `share_links.json` while the server runs
  has no effect, and is overwritten by the next change. Use the API.
- **Rate limiting is per process and in memory.** A restart resets it. One heavy
  viewer of a link can use up that link's budget for its other viewers.
- **A holder can use query capacity.** Reports are bounded by the 20 second
  query deadline, the 366 day range cap, the filter caps and the limits above,
  but a determined holder can still slow the dashboard for you. Revoke the link.
- **No recall.** Nothing un-shows a figure that was already displayed.
- **Open-access mode.** Before an admin password exists the share *admin* routes
  answer `401` to everyone, so no link can be made. Existing links keep working.
- **XSS on the dashboard origin** defeats the fragment design as it defeats
  every other secret held in page memory.

## Saved segments

A saved segment is a name and a set of filters for one site, in the same syntax
as the `filters` query parameter (`browsers==Chrome;countries!=US`). They exist so
that a view can be recalled and so that a share link can be limited to one.

- They are validated by the same parser as the stats endpoints: whatever it
  rejects, a segment rejects (unknown dimension, entry or exit pages, an empty
  value, a value over 512 characters, a malformed condition). What is stored is
  the parser's canonical rendering, not the caller's spelling, and it is checked to
  read back as the same filters.
- A segment needs at least one filter, and at most 8, so a share link limited to
  one still leaves the viewer room under the server's limit of 12 conditions per
  request.
- The dashboard's **Segment** menu applies one as the active filters. **Save
  current filters** in the Segments panel creates one from what is on screen.
- Listing is open to anyone who can read the site's stats (a session or any API
  key); creating, changing and deleting are admin-only. Share viewers cannot list
  segments.
- Deleting or editing a segment never affects share links already made from it.

## API

Admin routes need admin scope (a session or an `Admin` key); a session is also
checked against `dashboard_origin` like the other admin routes. They answer `401`
until an admin password exists and `403` to a `ReadOnly` key. Errors use the
shape `{"code": "...", "error": "..."}` described in the
[API overview](api-reference/index.md#errors).

### `POST /api/share-links`

```bash
curl -X POST https://analytics.example.com/api/share-links \
  -H "Authorization: Bearer mm_admin..." -H 'Content-Type: application/json' \
  -d '{"label":"Q4 board pack","site_id":"example.com","segment_id":"3f9c...","expires_in_days":30}'
```

| Field | Type | |
|---|---|---|
| `label` | string | Required, 1-128 characters. Private: viewers never see it. |
| `site_id` | string | Required. |
| `segment_id` | string | Optional. A segment of the **same site**; its filters are copied now. |
| `expires_in_days` | integer | Optional, 1-730. Omit for no expiry. |

`201`:

```json
{
  "id": "a1b2c3d4e5f6a1b2c3d4e5f6",
  "token": "ms_<64 hex digits>",
  "label": "Q4 board pack",
  "site_id": "example.com",
  "segment": "Chrome users",
  "filters": "browsers==Chrome",
  "created_at": "2026-10-21T12:00:00Z",
  "expires_at": "2026-11-20T12:00:00Z",
  "last_used_at": null,
  "status": "active"
}
```

`token` is in this response and nowhere else. The link to hand out is
`https://<your host>/#share=<token>`. Errors: `400` (invalid field, segment of
another site), `404` (unknown `segment_id`), `409` (100 active links),
`500` (the data directory is not writable; nothing was created).

### `GET /api/share-links`

A JSON array, newest first, with the fields above except `token`. `status` is
`active`, `expired` or `revoked`. Neither the token nor its hash is ever listed.

### `DELETE /api/share-links/{id}`

`200 {"status":"revoked"}`; repeating it is harmless. `404` for an unknown id.
`500` means the link **is** revoked in memory but the change could not be saved.

### Reading with a share token

```bash
curl -H 'X-Share-Token: ms_...' \
  'https://analytics.example.com/api/shared/stats/breakdown/pages?period=7d'
```

`site_id` may be omitted (it is the link's) and must otherwise match. Responses
are those of the ordinary endpoints. Failures:

| Status | `code` | |
|---|---|---|
| 401 | `share_link_invalid` | No token, or an unknown, malformed, revoked or expired one |
| 403 | `share_site_forbidden` | `site_id` names another site |
| 400 | `bad_request` | `site_id` or `filters` repeated; anything the ordinary endpoint rejects |
| 429 | `too_many_requests` | Per-link, global or refused-credential limit; `Retry-After: 1` |

### Segments

| Method | Path | Auth | |
|---|---|---|---|
| `GET` | `/api/segments?site_id=` | read | List, ordered by name. `site_id` optional. |
| `POST` | `/api/segments` | admin | `{"site_id","name","filters"}`, `201` |
| `POST` | `/api/segments/{id}` | admin | `{"name"?,"filters"?}` (at least one), `200` |
| `DELETE` | `/api/segments/{id}` | admin | `200 {"status":"deleted"}` |

A segment is
`{"id","site_id","name","filters","created_at","updated_at"}`. Errors: `400`
(anything the filter parser rejects, a bad name or an empty edit), `404`
(unknown id), `409` (name taken on that site, or the per-site / per-instance cap).
Editing uses `POST` rather than `PUT` because the dashboard CORS policy allows
`GET`, `POST` and `DELETE`.
