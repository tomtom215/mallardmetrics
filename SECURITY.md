# Security Policy

---

## Table of Contents

- [Reporting Vulnerabilities](#reporting-vulnerabilities)
- [Security Model](#security-model)
- [Authentication and Access Control](#authentication-and-access-control)
- [Input Validation](#input-validation)
- [Threat Model](#threat-model)
- [Dependency Security](#dependency-security)
- [Known limitations](#known-limitations)

---

## Reporting Vulnerabilities

If you discover a security vulnerability in Mallard Metrics, please report it responsibly by opening a **private security advisory** on GitHub.

**Do NOT open a public issue for security vulnerabilities.**

We will acknowledge receipt within 48 hours and provide a timeline for a fix.

---

## Security Model

### Privacy Guarantees

| Guarantee | Implementation |
|---|---|
| No cookies | The visitor ID is an HMAC-SHA256 hash of site + IP + User-Agent under a salt that rotates every `visitor_salt_rotation_days` (default 1). The tracking script sets no cookies |
| No IP storage | The client IP is used for the hash and the GeoIP lookup, then dropped. It is not written to the event store. Authentication events log a *truncated* address (IPv4 `a.b.c.x`, IPv6 first three groups; `anonymize_ip` in `src/api/auth.rs`) |
| Salt rotation | Visitor IDs change when the salt rotates, which limits long-term tracking; raising the period weakens that, by design and by operator choice |
| Outbound network | Analytics data is not sent anywhere. The only outbound connection Mallard makes is the DuckDB `INSTALL behavioral FROM community` download (first start, or `--install-extension`). Integrity of that download is delegated to DuckDB's extension mechanism; Mallard checks only the installed version (>= 0.10.0) and does not pin a hash |
| Reverse-proxy logs | Not controlled by Mallard. The bundled Caddy configuration (`deploy/Caddyfile`) deletes remote IP, headers and the query string from its access and error logs; check any other proxy you use. See [PRIVACY.md](PRIVACY.md) |

### Data Protection

- **Storage format** -- Event data is stored in Parquet files with ZSTD compression, organized by `site_id` and `date` for partition pruning
- **Atomic writes** -- Parquet files, `api_keys.json`, `share_links.json`, `segments.json` and the visitor-ID secret are written to a temporary file and renamed into place, so an interrupted write leaves either the old content or the new, never a truncated file
- **File permissions** -- `api_keys.json`, `admin.json`, `share_links.json`, `segments.json` and `.secret` are created mode 0600. Anyone able to read the secret can re-derive every visitor ID from an IP and User-Agent, which is the whole pseudonymisation guarantee
- **Encryption at rest** -- Not provided by Mallard Metrics itself. Use filesystem-level encryption (e.g., LUKS, dm-crypt) if required
- **Data retention** -- Configurable automatic deletion of old partitions via `MALLARD_RETENTION_DAYS`
- **Container hardening** -- The image is a distroless glibc image (no shell or package manager) running as uid 65532 and writing only to `/data`; the bundled compose files add `read_only`, `cap_drop: ALL` and `no-new-privileges`. The production compose file also puts Mallard on an `internal` network with no route to the internet, which is why the extension must be seeded with `--install-extension` there

---

## Authentication and Access Control

### Dashboard Authentication

- **Password hashing** -- Argon2id with the `argon2` crate's default parameters. Minimum 12 characters and a short list of obvious values refused, for both `MALLARD_ADMIN_PASSWORD` (startup fails if it does not pass) and the setup endpoint. A password set through setup is stored as an Argon2id hash in `data_dir/admin.json`; `MALLARD_ADMIN_PASSWORD` takes precedence and is not written back
- **Session tokens** -- 256-bit random tokens in an HttpOnly cookie (`mm_session`); the token is not repeated in the response body. Sessions are in memory, capped by `MALLARD_MAX_SESSIONS`, and cleared on restart
- **Cookie attributes** -- HttpOnly, Secure (when `MALLARD_SECURE_COOKIES=true` or `dashboard_origin` starts with `https://`), SameSite=Strict
- **Session expiration** -- Configurable TTL (default: 24 hours) via `MALLARD_SESSION_TTL`

### API Key Management

- **Key format and transport** -- `mm_` prefix followed by a random string, sent as `Authorization: Bearer mm_...` or `X-API-Key: mm_...`
- **Storage** -- Keys are SHA-256 hashed at rest. Plaintext keys are shown only once at creation time. Keys are persisted to `data/api_keys.json` and survive server restarts.
- **Operations** -- Create, list, and revoke via `/api/keys` endpoints
- **Scopes** -- Two scopes available:
  - `ReadOnly` — access to all `GET /api/stats/*` and export endpoints
  - `Admin` — full access including creating, listing, and revoking API keys
- **Comparison** -- Constant-time SHA-256 comparison prevents timing side-channel attacks

### Route Protection

| Route Group | Authentication Required |
|---|---|
| `POST /api/event` | No (tracking script must work without auth; Origin allowlist applies) |
| `GET /api/event` | No (pixel tracking; same Origin allowlist applies) |
| `GET /health`, `GET /health/ready`, `GET /health/detailed` | No. `/health/detailed` discloses the version, extension state and counters, so do not expose it publicly if that matters to you |
| `GET /metrics` | Optional — bearer token required when `MALLARD_METRICS_TOKEN` is set |
| `GET /robots.txt`, `GET /.well-known/security.txt` | No |
| `/api/auth/*` | No (these are the auth endpoints themselves) |
| `GET /api/stats/*` | Yes — session cookie or any API key (ReadOnly or Admin) |
| `GET /api/stats/export` | Yes — session cookie or any API key (ReadOnly or Admin) |
| `GET /api/keys`, `POST /api/keys`, `DELETE /api/keys/*`, `DELETE /api/gdpr/erase` | Yes — session cookie or Admin-scoped API key; refused (`401`) until an admin password exists |
| `POST/GET /api/share-links`, `DELETE /api/share-links/*`, `POST /api/segments`, `POST/DELETE /api/segments/*` | Yes — session cookie or Admin-scoped API key; refused (`401`) until an admin password exists |
| `GET /api/segments` | Yes — session cookie or any API key (ReadOnly or Admin) |
| `GET /api/shared/session`, `GET /api/shared/stats/*` | A share token in `X-Share-Token` (and nothing else): read-only reports for the link's one site. See [docs/src/share-links.md](docs/src/share-links.md) |

Before an admin password exists, the read routes are open. That is a deliberate mode for a public dashboard; the dashboard warns about it on every load.

### CSRF Protection

When `dashboard_origin` is set, the login, setup and logout endpoints and the session-authenticated admin routes (key management, erasure) compare the request's `Origin` header, or failing that its `Referer`, with it; a mismatch is `403 Forbidden`. Limits: a request carrying **neither** header is allowed (non-browser clients), API-key requests are not checked, and nothing is checked when `dashboard_origin` is unset. Session cookies are `SameSite=Strict`, which is the primary defence; set `MALLARD_DASHBOARD_ORIGIN` in production for the second one.

### CORS Policy

- **Ingestion** (`POST /api/event`) -- Permissive CORS. The tracking script must be able to POST from any customer domain.
- **Dashboard and API** (`/api/stats/*`, `/api/keys/*`) -- When `MALLARD_DASHBOARD_ORIGIN` is set, only that origin is allowed; when unset these routes answer `Access-Control-Allow-Origin: *` (verified on a running binary), so set it unless you rely on API keys only.

---

## Input Validation

### SQL Injection Prevention

All DuckDB queries use parameterized statements (`$1`, `?`). User input is never interpolated into SQL strings. Funnel steps and sequence conditions use a safe `page:/path` and `event:name` format that is parsed and validated before being incorporated into queries.

### XSS Prevention

All user-provided data (page names, referrers, UTM parameters, custom properties) is sanitized before storage. Control characters are stripped and strings are truncated to maximum lengths.

### Input Length Limits

| Field | Maximum Length |
|---|---|
| Domain / site_id | 256 characters |
| Event name | 256 characters |
| URL / pathname | 2048 characters |
| Referrer | 2048 characters |
| Custom properties (JSON) | 4096 bytes |
| Request body | 65,536 bytes (`413` beyond) |
| `X-Request-ID` (echoed back) | 128 printable ASCII characters |

Beyond length, two fields are validated by shape:

- **Custom properties** must parse as a JSON *object*. Anything else is dropped
  rather than stored, because a non-object value would break every JSON query on
  that site's `props` column.
- **`revenue_currency`** must be three ASCII letters. It was previously
  truncated to three characters, which turned `DOLLARS` into `DOL`.

### Rate Limiting

Two independent token-bucket limiters guard the ingestion endpoint:

| Setting | Keyed by | Protects |
|---|---|---|
| `MALLARD_RATE_LIMIT` | `site_id` | The server, from one site's traffic |
| `MALLARD_RATE_LIMIT_PER_IP` | Client IP | A site's real visitors, from one abusive client |

Both default to 0 (unlimited). The per-site limit alone is not sufficient: a
single client can consume the whole site budget and deny service to everyone
else on that site.

Both bucket maps are capped by `MALLARD_MAX_TRACKED_KEYS`, because their keys
come from attacker-influenced values. At the cap idle buckets are reclaimed and
then the least recently used are evicted in batches; a new key is never refused,
so a flood of distinct keys cannot turn real visitors away. Evictions are
counted in `mallard_rate_limiter_evictions_total`. IPv6 clients are keyed by
their /64.

The per-IP limiter depends on the client address being accurate — see
`MALLARD_TRUST_PROXY_HEADERS` under Threat Model.

### Bot Filtering

Requests whose User-Agent matches a built-in list of crawler and HTTP-client
markers (`src/ingest/useragent.rs`) are dropped when `MALLARD_FILTER_BOTS=true`
(default). This is a substring list: it keeps well-behaved crawlers out of the
numbers and does nothing against a bot that sends a browser User-Agent. There is
no data-centre/ASN filtering or referrer-spam list.

---

## Threat Model

| Threat | Mitigation |
|---|---|
| SQL injection | Parameterized queries for all user input. Safe format parsing for funnel/sequence conditions |
| XSS | Input sanitization, control character removal, length limits |
| Data exfiltration | No analytics data is sent to third parties; embedded database; authenticated API. The one outbound call is the extension download (see Privacy Guarantees) |
| PII leakage | IP addresses not stored (authentication logs keep a truncated prefix). Salt rotation (daily by default). Per-site hash scoping. No cookies |
| Cross-site correlation | The site ID is part of the visitor-ID HMAC input, so one person on two sites of an instance produces unrelated identifiers |
| Spoofed client identity | `X-Forwarded-For` and `X-Real-IP` are ignored unless `MALLARD_TRUST_PROXY_HEADERS` is set; otherwise the peer socket address is used. When trusted, the client is entry `MALLARD_TRUSTED_PROXY_HOPS` (default 1, range 1 to 8) counted from the **right** of `X-Forwarded-For`, because each proxy you operate appends the address it saw and anything further left was supplied by the client. The value must parse as an IP address. A proxy that appends (nginx's `$proxy_add_x_forwarded_for`, Caddy) is correct at 1 hop; with a CDN in front of nginx use 2. A hop count larger than the number of proxies you run lets a client choose its address, and Mallard cannot detect that. Direct access to the port, bypassing the proxy, defeats the scheme: bind to loopback or an internal network |
| Unauthorised event injection | When `site_ids` is configured it is enforced against the event payload as well as the `Origin` header, so a client that omits `Origin` cannot write to an unlisted site |
| Secret disclosure at rest | `api_keys.json` and the visitor-ID secret are written mode 0600, atomically (temp file plus rename) |
| Memory exhaustion | The event buffer, session store, rate-limit buckets and login-attempt records are all capped; dropped events are counted in `mallard_events_dropped_total` |
| Corrupt data blocking reads | Parquet files are written to a temporary name and renamed into place, so an interrupted write cannot leave a truncated file in the read glob |
| Brute force (login) | Argon2id hashing, run off the async runtime. Attempts are counted per client network (IPv4 address or IPv6 /64) when they are admitted, before the password is verified, so parallel requests cannot slip past the limit. At most 4 Argon2 operations run at once; a request that cannot get a slot within 10 s receives `503`. Lockout length and attempt count are configurable (`MALLARD_LOGIN_LOCKOUT`, `MALLARD_MAX_LOGIN_ATTEMPTS`; `0` attempts disables the protection and logs a warning). `/api/auth/setup` shares the protection |
| Brute force (API) | Per-site and per-IP token-bucket rate limiting on ingestion |
| Weak admin password | Minimum 12 characters, and a short list of obvious values is refused |
| Instance takeover after a restart | The Argon2id hash of a password set through the setup endpoint is persisted to `data_dir/admin.json` (mode 0600), so a restart does not reopen first-run setup for whoever reaches the instance first |
| Dashboard lockout as denial of service | Brute-force records are keyed by the resolved client address rather than a shared placeholder, so one attacker's failures cannot lock out every other operator |
| Privilege escalation before setup | Key management and GDPR erasure return `401` until an admin password exists, so an unconfigured instance cannot be used to mint an admin key that would outlive setup |
| Leaked, guessed or over-reaching share link | 256-bit OS-CSPRNG token shown once and stored only as a hash; read from a URL fragment and sent in a header, so it is in no URL, log or `Referer`; confined to the read-only `/api/shared/` routes with `site_id` forced and the link's segment AND-ed in; revocation and expiry on the next request; per-link, global and per-client rate limits. A leaked link stays valid until revoked, and restoring an old backup revives revoked links. See [docs/src/share-links.md](docs/src/share-links.md) |
| Session hijacking | HttpOnly cookies, Secure flag with TLS, SameSite=Strict, 256-bit random tokens |
| CSRF | `SameSite=Strict` cookies, plus Origin/Referer validation when `dashboard_origin` is set (see CSRF Protection for its limits) |
| Clickjacking | `X-Frame-Options: DENY` and `Content-Security-Policy` headers |
| Protocol downgrade | `Strict-Transport-Security` (HSTS) with 1-year `max-age`, `includeSubDomains`, and `preload` (eligible for browser preload lists via hstspreload.org) |
| Unauthorized dashboard access | Argon2id password authentication, session-based access control |
| Unauthorized API access | API key authentication with SHA-256 hashed storage |
| Data tampering through the API | `ReadOnly` keys cannot change data; only an Admin credential can erase. Anyone with filesystem access to the data directory can alter it; Mallard does not sign or checksum stored events |
| Dependency vulnerabilities | `cargo deny` licence/ban/source checks on every CI run; RustSec advisories audited weekly and on any change to `Cargo.toml`, `Cargo.lock` or `deny.toml` (`.github/workflows/security.yml`). GitHub Actions pinned to commit SHAs |

---

## Dependency Security

- **`cargo deny`** runs in CI (`policy` job) for licences, bans and sources, and weekly for RustSec advisories (`security.yml`)
- **GitHub Actions** are pinned to commit SHAs for reproducible, tamper-resistant builds
- **Runtime image** -- distroless `cc-debian12` with the compiled binary: no shell or package manager. DuckDB is compiled from source into the binary, and the `behavioral` extension is third-party code (a community extension by the same author as this project) loaded into the process at runtime

---

## Known limitations

- No encryption at rest, no audit log, and one shared admin credential.
- Keys and sessions have no per-site scoping: any valid credential reads every site.
- `/health/detailed` and, without `MALLARD_METRICS_TOKEN`, `/metrics` are unauthenticated. `/metrics` reports whether an admin password is set (`mallard_auth_configured`) and the live session count.
- Bot filtering is a User-Agent substring list.
- The `behavioral` extension is fetched at runtime and not hash-pinned.
