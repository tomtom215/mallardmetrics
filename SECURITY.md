# Security Policy

---

## Table of Contents

- [Reporting Vulnerabilities](#reporting-vulnerabilities)
- [Security Model](#security-model)
- [Authentication and Access Control](#authentication-and-access-control)
- [Input Validation](#input-validation)
- [Threat Model](#threat-model)
- [Dependency Security](#dependency-security)

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
| No cookies | Visitor ID is a daily-rotating HMAC-SHA256 hash of IP + User-Agent + daily salt |
| No PII storage | IP addresses are used only for hashing and GeoIP lookup, then immediately discarded. They are never written to disk, database, or logs |
| Daily salt rotation | Visitor IDs change every day, preventing long-term tracking |
| No external network calls | DuckDB is embedded. No analytics data leaves the server except via the authenticated dashboard API |
| Privacy-preserving design | Pseudonymous visitor IDs only; no cookies; no raw IP storage. See [PRIVACY.md](PRIVACY.md) for the full GDPR/CCPA analysis |

### Data Protection

- **Storage format** -- Event data is stored in Parquet files with ZSTD compression, organized by `site_id` and `date` for partition pruning
- **Atomic writes** -- Parquet files, `api_keys.json` and the visitor-ID secret are written to a temporary file and renamed into place, so an interrupted write leaves either the old content or the new, never a truncated file
- **File permissions** -- `api_keys.json` and `.secret` are created mode 0600. Anyone able to read the secret can re-derive every visitor ID from an IP and User-Agent, which is the whole pseudonymisation guarantee
- **Encryption at rest** -- Not provided by Mallard Metrics itself. Use filesystem-level encryption (e.g., LUKS, dm-crypt) if required
- **Data retention** -- Configurable automatic deletion of old partitions via `MALLARD_RETENTION_DAYS`
- **Container hardening** -- The image runs as uid 65532 and writes only to `/data`; the bundled compose file adds `read_only`, `cap_drop: ALL` and `no-new-privileges`

---

## Authentication and Access Control

### Dashboard Authentication

- **Password hashing** -- Argon2id with default parameters (memory-hard, GPU-resistant)
- **Session tokens** -- 256-bit cryptographic random tokens, stored as HttpOnly cookies
- **Cookie attributes** -- HttpOnly, Secure (when `MALLARD_SECURE_COOKIES=true` or `dashboard_origin` starts with `https://`), SameSite=Strict
- **Session expiration** -- Configurable TTL (default: 24 hours) via `MALLARD_SESSION_TTL`

### API Key Management

- **Key format** -- `mm_` prefix followed by a cryptographic random string
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
| `GET /health`, `GET /health/ready`, `GET /health/detailed` | No |
| `GET /metrics` | Optional — bearer token required when `MALLARD_METRICS_TOKEN` is set |
| `GET /robots.txt`, `GET /.well-known/security.txt` | No |
| `/api/auth/*` | No (these are the auth endpoints themselves) |
| `GET /api/stats/*` | Yes — session cookie or any API key (ReadOnly or Admin) |
| `GET /api/stats/export` | Yes — session cookie or any API key (ReadOnly or Admin) |
| `GET /api/keys`, `POST /api/keys`, `DELETE /api/keys/*` | Yes — session cookie or Admin-scoped API key |

### CSRF Protection

State-mutating endpoints authenticated via session cookie validate the `Origin` or `Referer` header against `dashboard_origin`. Requests with a mismatched or absent origin receive `403 Forbidden`. Set `MALLARD_DASHBOARD_ORIGIN` in production to enable CSRF protection.

### CORS Policy

- **Ingestion** (`POST /api/event`) -- Permissive CORS. The tracking script must be able to POST from any customer domain.
- **Dashboard and API** (`/api/stats/*`, `/api/keys/*`) -- Restrictive CORS. When `MALLARD_DASHBOARD_ORIGIN` is set, only that origin is allowed.

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
| Custom properties (JSON) | 4096 characters |
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
come from attacker-influenced values and the cleanup sweep only runs every 15
minutes. Idle buckets are reclaimed before a new key is refused.

The per-IP limiter depends on the client address being accurate — see
`MALLARD_TRUST_PROXY_HEADERS` under Threat Model.

### Bot Filtering

Known bot User-Agents are automatically filtered from analytics when `MALLARD_FILTER_BOTS=true` (default). This prevents automated crawlers and scrapers from inflating visitor counts.

---

## Threat Model

| Threat | Mitigation |
|---|---|
| SQL injection | Parameterized queries for all user input. Safe format parsing for funnel/sequence conditions |
| XSS | Input sanitization, control character removal, length limits |
| Data exfiltration | No external network calls, embedded database, authenticated API access |
| PII leakage | IP addresses never stored. Salt rotation (daily by default). Per-site hash scoping. No cookies |
| Cross-site correlation | The site ID is part of the visitor-ID HMAC input, so one person on two sites of an instance produces unrelated identifiers |
| Spoofed client identity | `X-Forwarded-For` and `X-Real-IP` are ignored unless `MALLARD_TRUST_PROXY_HEADERS` is set; otherwise the peer socket address is used. Even when trusted, the value must parse as an IP address, so a request that bypasses the proxy cannot inject arbitrary text into the rate-limit key, the GeoIP lookup or the visitor-ID input |
| Unauthorised event injection | When `site_ids` is configured it is enforced against the event payload as well as the `Origin` header, so a client that omits `Origin` cannot write to an unlisted site |
| Secret disclosure at rest | `api_keys.json` and the visitor-ID secret are written mode 0600, atomically (temp file plus rename) |
| Memory exhaustion | The event buffer, session store, rate-limit buckets and login-attempt records are all capped; dropped events are counted in `mallard_events_dropped_total` |
| Corrupt data blocking reads | Parquet files are written to a temporary name and renamed into place, so an interrupted write cannot leave a truncated file in the read glob |
| Brute force (login) | Argon2id hashing (inherently slow, run off the async runtime), per-IP attempt counting with configurable lockout. `/api/auth/setup` shares the same protection, so the window before first configuration cannot be probed freely |
| Brute force (API) | Per-site and per-IP token-bucket rate limiting on ingestion |
| Weak admin password | Minimum 12 characters, and a short list of obvious values is refused |
| Instance takeover after a restart | The Argon2id hash of a password set through the setup endpoint is persisted to `data_dir/admin.json` (mode 0600), so a restart does not reopen first-run setup for whoever reaches the instance first |
| Dashboard lockout as denial of service | Brute-force records are keyed by the resolved client address rather than a shared placeholder, so one attacker's failures cannot lock out every other operator |
| Privilege escalation before setup | Key management and GDPR erasure return `401` until an admin password exists, so an unconfigured instance cannot be used to mint an admin key that would outlive setup |
| Session hijacking | HttpOnly cookies, Secure flag with TLS, SameSite=Strict, 256-bit random tokens |
| CSRF | Origin/Referer header validation on all state-mutating session-authenticated endpoints |
| Clickjacking | `X-Frame-Options: DENY` and `Content-Security-Policy` headers |
| Protocol downgrade | `Strict-Transport-Security` (HSTS) with 1-year `max-age`, `includeSubDomains`, and `preload` (eligible for browser preload lists via hstspreload.org) |
| Unauthorized dashboard access | Argon2id password authentication, session-based access control |
| Unauthorized API access | API key authentication with SHA-256 hashed storage |
| Data tampering | Parquet files are append-only per partition. Dashboard access is read-only for API keys |
| Dependency vulnerabilities | `cargo deny check` in CI pipeline. All GitHub Actions pinned to commit SHAs |

---

## Dependency Security

- **`cargo deny`** runs in CI to check for known vulnerabilities, license issues, and duplicate dependencies
- **GitHub Actions** are pinned to commit SHAs for reproducible, tamper-resistant builds
- **Minimal dependency surface** -- the project avoids unnecessary dependencies to reduce attack surface
- **Static binary** -- the `FROM scratch` Docker image contains only the compiled binary, with no shell, package manager, or other tools that could be exploited
