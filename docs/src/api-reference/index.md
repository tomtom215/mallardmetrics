# API Reference

Mallard Metrics exposes an HTTP API from the same process that serves the
dashboard. Every route below was checked against `src/server.rs`, and every
example against a running binary (0.2.0).

## Base URL

```
http://your-instance.com
```

## Route table

| Method | Path | Auth | Page |
|---|---|---|---|
| `POST` | `/api/event` | none (Origin allowlist) | [Ingestion](ingestion.md) |
| `GET` | `/api/event` | none | [Ingestion](ingestion.md#get-apievent) |
| `HEAD` | `/api/event` | none | [Ingestion](ingestion.md#head-apievent) |
| `POST` | `/api/auth/setup` | none | [Authentication](auth.md) |
| `POST` | `/api/auth/login` | none | [Authentication](auth.md) |
| `POST` | `/api/auth/logout` | none | [Authentication](auth.md) |
| `GET` | `/api/auth/status` | none | [Authentication](auth.md) |
| `GET` | `/api/sites` | read | [Stats](stats.md#get-apisites) |
| `GET` | `/api/stats/main` | read | [Stats](stats.md#get-apistatsmain) |
| `GET` | `/api/stats/timeseries` | read | [Stats](stats.md#get-apistatstimeseries) |
| `GET` | `/api/stats/sessions` | read | [Behavioral](../behavioral-analytics.md#sessions) |
| `GET` | `/api/stats/realtime` | read | [Stats](stats.md#get-apistatsrealtime) |
| `GET` | `/api/stats/revenue` | read | [Stats](stats.md#get-apistatsrevenue) |
| `GET` | `/api/stats/goals` | read | [Stats](stats.md#get-apistatsgoals) |
| `GET` | `/api/stats/properties` | read | [Stats](stats.md#get-apistatsproperties-and-get-apistatsproperty-values) |
| `GET` | `/api/stats/property-values` | read | [Stats](stats.md#get-apistatsproperties-and-get-apistatsproperty-values) |
| `GET` | `/api/stats/breakdown/{dimension}` | read | [Stats](stats.md#get-apistatsbreakdowndimension) |
| `GET` | `/api/stats/export` | read | [Stats](stats.md#get-apistatsexport) |
| `GET` | `/api/stats/funnel` | read | [Behavioral](../behavioral-analytics.md#funnels) |
| `GET` | `/api/stats/retention` | read | [Behavioral](../behavioral-analytics.md#retention-cohorts) |
| `GET` | `/api/stats/sequences` | read | [Behavioral](../behavioral-analytics.md#sequence-matching) |
| `GET` | `/api/stats/flow` | read | [Behavioral](../behavioral-analytics.md#flow) |
| `GET` | `/api/segments` | read | [Share Links](../share-links.md#segments) |
| `POST` | `/api/segments` | admin | [Share Links](../share-links.md#segments) |
| `POST` | `/api/segments/{id}` | admin | [Share Links](../share-links.md#segments) |
| `DELETE` | `/api/segments/{id}` | admin | [Share Links](../share-links.md#segments) |
| `POST` | `/api/share-links` | admin | [Share Links](../share-links.md#post-apishare-links) |
| `GET` | `/api/share-links` | admin | [Share Links](../share-links.md#get-apishare-links) |
| `DELETE` | `/api/share-links/{id}` | admin | [Share Links](../share-links.md#delete-apishare-linksid) |
| `GET` | `/api/shared/session`, `/api/shared/stats/*` | share token | [Share Links](../share-links.md#how-a-request-is-confined) |
| `POST` | `/api/keys` | admin | [Authentication](auth.md#post-apikeys) |
| `GET` | `/api/keys` | admin | [Authentication](auth.md#get-apikeys) |
| `DELETE` | `/api/keys/{key_hash}` | admin | [Authentication](auth.md#delete-apikeyskey_hash) |
| `DELETE` | `/api/gdpr/erase` | admin | [Stats](stats.md#delete-apigdprerase) |
| `POST` | `/api/admin/backup` | admin | [Auth](auth.md#post-apiadminbackup) |
| `GET` | `/health`, `/health/ready`, `/health/detailed` | none | [Health](health.md) |
| `GET` | `/metrics` | none, or bearer token | [Health & Metrics](health.md#get-metrics) |
| `GET` | `/mallard.js`, `/js/script.js` | none | [Tracking Script](../tracking-script.md) |
| `GET` | `/robots.txt`, `/.well-known/security.txt` | none | |
| `GET` | `/` and `/{path}` | none | the embedded dashboard |

## Authentication levels

- **none** — reachable by anyone.
- **read** — a session cookie (`mm_session`), `Authorization: Bearer mm_...`, or
  `X-API-Key: mm_...`. A `ReadOnly` or `Admin` key works. **Before an admin
  password exists, read routes are open** (a deliberate "public dashboard"
  mode); the dashboard shows a warning banner in that state.
- **share token** — an `X-Share-Token: ms_...` header, accepted **only** by the
  `/api/shared/` routes (read-only reports for one site). It is not an API key:
  presented anywhere else it is a `401`. See [Share Links](../share-links.md).
- **admin** — a session, or an `Admin`-scoped key. A `ReadOnly` key gets `403`.
  Before an admin password exists these routes answer `401` to everyone.
  Session-authenticated admin requests are also checked against
  `dashboard_origin` when it is set (`403` on mismatch); API-key requests are not.

## Content type

Request bodies are `application/json`. Responses are `application/json` unless
stated (ingest answers with an empty body, `/health` and `/health/ready` with
plain text, `/metrics` with `text/plain; version=0.0.4`, exports with CSV).
JSON responses carry `Cache-Control: no-store, no-cache, private`. Every
response carries `X-Request-ID` (an incoming value of up to 128 visible-ASCII
characters is reused) and the security headers described in
[Security](../security.md#security-headers).

## Errors

The `/api/stats/*`, `/api/sites` and `/api/gdpr/erase` handlers return

```json
{"code": "bad_request", "error": "human-readable description"}
```

`code` is a stable machine-readable string:

| `code` | Status | Meaning |
|---|---|---|
| `bad_request` | 400 | Invalid parameter. `error` names it. This includes a missing or mistyped query parameter and a malformed, mistyped or wrongly-typed JSON body on every route except `POST /api/event`; all are `400` (never `415` or `422`). |
| `unauthorized` | 401 | Reserved. The auth middleware itself answers with an empty body. |
| `not_found` | 404 | For example, revoking an unknown key. |
| `internal_error` | 500 | A server-side fault. The body never contains internals; the cause is in the server log. |
| `too_many_requests` | 429 | No query slot became free within 10 s. Carries `Retry-After: 1`. |
| `behavioral_unavailable` | 503 | The `behavioral` extension is not loaded. Fix: the install (see [Troubleshooting](../troubleshooting.md#behavioral-endpoints-answer-503)). |
| `session_data_unavailable` | 503 | `gdpr_mode`/`round_timestamps` stores timestamps rounded to the hour, so sessions, funnels, sequences, flow and entry/exit pages cannot be computed. Not an install problem: the fix is the privacy setting. |
| `conflict` | 409 | The request conflicts with existing state, for example a backup of that name exists, a segment name is taken or a cap (active share links, segments) is reached. |
| `share_link_invalid` | 401 | `/api/shared/` routes only: no token, or an unknown, malformed, revoked or expired one. All look alike on purpose. |
| `share_site_forbidden` | 403 | `/api/shared/` routes only: `site_id` names a site other than the link's. |
| `query_timeout` | 504 | The statement overran its 20 s deadline and was interrupted. Use a shorter range or fewer filters. |

Not every error body is JSON. These are produced before or outside the handlers
(each was observed against the running binary):

| Situation | Status | Body |
|---|---|---|
| No credentials, wrong key or no session on a read or admin route | 401 | empty |
| `ReadOnly` key on an admin route; CSRF origin mismatch on an admin route | 403 | empty |
| Malformed body, wrong type, missing field or wrong `Content-Type` on `POST /api/event` | 400 | plain text (the browser beacon never reads it) |
| Body over 64 KB on `/api/event` | 413 | plain text |
| Request not finished in 30 s | 408 | empty |
| `/api/auth/*` handler errors | 4xx/5xx | `{"error": "..."}` **without** `code` (a malformed request body there *is* `bad_request` with a `code`) |
| Ingest rejections (400, 403, 429) | | empty |

Branch on the status first and use `code` when it is present.

## Status codes

| Code | Where |
|---|---|
| 200 | Success. |
| 201 | API key created. |
| 202 | Event accepted (or silently dropped as bot traffic). |
| 400 | Invalid input. |
| 401 | Not authenticated; any admin route before setup; an invalid share token. |
| 403 | Origin not allowed (ingest), read-only key on an admin route, CSRF mismatch, a share link asked for another site. |
| 404 | Unknown key hash. |
| 408 | 30-second request timeout. |
| 409 | `POST /api/auth/setup` when a password already exists; a taken segment name; a cap on share links or segments. |
| 413 | Ingest body over 64 KB. |
| 415 | JSON body without a JSON content type. |
| 422 | JSON parsed but a field is missing or the wrong type. |
| 429 | Ingest rate limit (per site or per IP), login lockout, or query queue overflow. Always with `Retry-After`. |
| 500 | Internal error; GDPR erasure that left data behind (`status: "partial"`). |
| 503 | Behavioral analytics unavailable; `/health/ready` not ready; `/health/detailed` degraded; login or setup while four password checks are already running; ingest while the server is shutting down or its buffer is full. |
| 504 | Analytics query exceeded its deadline. |

## Sections

- [Event Ingestion](ingestion.md)
- [Analytics Stats](stats.md)
- [Authentication](auth.md)
- [Health & Metrics](health.md)
