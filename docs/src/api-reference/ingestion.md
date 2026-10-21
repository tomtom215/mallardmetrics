# Event Ingestion

## `POST /api/event`

Records one analytics event. The tracking script calls it automatically; it can
also be called directly for server-side events.

**Authentication:** none. When `site_ids` is configured, the `Origin` header
**and** the payload's `d` field are checked against it (see below).

**CORS:** `Access-Control-Allow-Origin: *`, methods `POST, GET`, header
`Content-Type`, and preflight responses are cached for 24 hours
(`Access-Control-Max-Age: 86400`) so a `sendBeacon` costs one request, not two.

### Request body

```json
{
  "d": "example.com",
  "n": "pageview",
  "u": "https://example.com/pricing",
  "r": "https://google.com/",
  "w": 1920,
  "p": "{\"plan\": \"pro\"}",
  "ra": 99.00,
  "rc": "USD"
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `d` | string | yes | Site ID. 1–256 characters from `A-Z a-z 0-9 . - _ :`. |
| `n` | string | yes | Event name, 1–256 characters (`pageview`, `signup`, ...). |
| `u` | string | yes | Page URL, at most 2048 characters. Only the path is stored; query string and fragment are dropped, but `utm_*` parameters are extracted first. |
| `r` | string | no | Referrer URL, at most 2048 characters. The source name (`Google`, ...) is derived from its host. |
| `w` | integer | no | Viewport width in CSS pixels (the tracker sends `window.innerWidth`). Stored as `screen_size`; also gives `device_type` when the browser sends no Client Hints. A non-integer is a `422`. |
| `p` | string | no | Custom properties as a **JSON-encoded object in a string**, at most 4096 characters. Anything that is not a JSON object is dropped (the event is still recorded). |
| `ra` | number | no | Revenue amount. Dropped if not finite or outside ±9,999,999,999.99. |
| `rc` | string | no | ISO 4217 code. Validated and upper-cased; an invalid code is dropped. |

### Response

```
HTTP/1.1 202 Accepted
Content-Length: 0
```

`202` means the event is in the in-memory buffer. **It is not queryable yet**:
the buffer is flushed every `flush_interval_secs` (default 60) or when it holds
`flush_event_count` (default 1000) events, whichever comes first. See
[Data Management](../data-management.md#buffer-and-flush-lifecycle).

### Errors

Checked in this order; the body is empty unless stated.

| Condition | Status |
|---|---|
| Body over 64 KB | 413 (plain text) |
| Not valid JSON | 400 (plain text) |
| `Content-Type` is not `application/json` | 415 (plain text) |
| A required field missing, or a field of the wrong type | 422 (plain text naming the field) |
| `Origin` present and not in `site_ids` | 403 |
| `d`, `n` or `u` empty; `n` > 256, `u` > 2048, `r` > 2048 or `p` > 4096 characters | 400 |
| `d` has invalid characters | 400 |
| `site_ids` set and `d` not in it | 403 |
| Per-site (`rate_limit_per_site`) or per-IP (`rate_limit_per_ip`) limit exceeded | 429 with `Retry-After: 1` |
| Bot User-Agent while `filter_bots = true` | **202**, event discarded, not counted |
| Buffer at `max_buffered_events` (flushes are failing) or server shutting down | 503 |

Note the order: bot filtering runs **after** rate limiting, so bot traffic
consumes rate-limit budget. An unlisted `Origin` is checked before the payload
is validated; a request with **no** `Origin` (curl, server-side code) passes the
Origin check and is held to the `d` allowlist instead.

Origin matching is by host: `https://example.com` and `http://example.com:8080`
match the entry `example.com`; `https://example.com.evil.com` does not.

### Privacy processing

Before storage the server:

1. Resolves the client address — the peer socket, or `X-Forwarded-For` entry
   `trusted_proxy_hops` from the right when `trust_proxy_headers` is on.
2. Computes `visitor_id = HMAC-SHA256(salt, site_id | IP | User-Agent)`, where
   the salt is derived from `MALLARD_SECRET` and the current salt period
   (`visitor_salt_rotation_days`, 1 by default). See
   [Privacy and GDPR](../privacy-gdpr.md).
3. Looks the address up in GeoIP if a database is loaded.
4. Discards the address. It is never written anywhere.

### Server-side example

```bash
curl -X POST https://your-instance.com/api/event \
  -H 'Content-Type: application/json' \
  -H 'User-Agent: Mozilla/5.0 (X11; Linux x86_64) Chrome/131.0 Safari/537.36' \
  -d '{"d":"example.com","n":"server_signup","u":"https://example.com/signup"}'
```

Send a browser-like `User-Agent`: curl's default is classed as a bot and the
event would be accepted (`202`) and silently dropped.

---

## `GET /api/event`

A 1×1 transparent GIF tracker for places without JavaScript (HTML email, RSS).

| Query parameter | Required | Description |
|---|---|---|
| `d` | yes | Site ID. |
| `u` | yes | Page URL. |
| `n` | no | Event name, default `pageview`. |
| `r` | no | Referrer. |
| `w` | no | Viewport width. A non-number is ignored. |

Revenue and custom properties are not available on this route.

**The response is always the GIF**, `200 OK`, `Content-Type: image/gif`,
`Content-Length: 43`, `Cache-Control: no-store, no-cache, must-revalidate`,
whatever the query says: a missing `d` or `u`, a disallowed site, a rate-limited
or bot request all return the GIF and simply record nothing (rate-limit
rejections still count in `mallard_rate_limit_rejections_total`). An error
status would render as a broken image in a mail client.

```html
<img src="https://analytics.example.com/api/event?d=example.com&u=https%3A%2F%2Fexample.com%2Fpage" width="1" height="1" alt="">
```

## `HEAD /api/event`

Answers `200` with `Content-Type: image/gif` and records nothing, so link
checkers and mail-client prefetchers do not register pageviews. (Without this
route HEAD would reach the GET handler.)
