# Security & Privacy

## Privacy Model

Mallard Metrics is built with privacy as a hard constraint, not an afterthought.

### No Cookies

The tracking script sets no cookies. There is no cookie-based session tracking.

### No PII Storage

The client IP address is the only piece of potentially identifying information that arrives at the server. It is:

1. Used to compute the visitor ID (see below).
2. Used for a GeoIP lookup (if configured).
3. **Discarded immediately.** It is never written to the database, log files, or Parquet files.

No names, email addresses, or device fingerprints are collected or stored.

### Privacy-Safe Visitor ID

To distinguish unique visitors without storing PII, Mallard Metrics computes a two-step HMAC-SHA256 derivation:

```
daily_salt   = HMAC-SHA256(key = "mallard-metrics-salt",
                           input = MALLARD_SECRET + ":" + today_UTC_date)

visitor_id   = HMAC-SHA256(key = daily_salt,
                           input = IP_address + "|" + User-Agent)
```

The intermediate `daily_salt` binds the secret and the current date together, rotating the effective key every 24 hours while keeping the outer HMAC's message short.

Properties of this approach:

- **Deterministic within a day** — The same visitor from the same browser produces the same ID throughout the day, enabling accurate unique-visitor counting.
- **Rotates daily** — The UTC date changes the salt each day, so IDs cannot be correlated across days.
- **Not reversible** — Without `MALLARD_SECRET`, the IP address cannot be recovered from the stored hash.
- **No IP storage** — The IP address is discarded immediately after hashing.

### GDPR/CCPA Compliance

Because Mallard Metrics stores no personal data:
- No cookie consent banner is required.
- No data subject access/deletion requests need to be processed.
- No data processor agreements are needed with third parties (there are none).

---

## Authentication Security

### Dashboard Password

Passwords are hashed with **Argon2id** using PHC default parameters before any comparison. The plaintext password is never stored. The hash is held in memory and set from the `MALLARD_ADMIN_PASSWORD` environment variable at startup.

### Session Tokens

Dashboard sessions use **256-bit cryptographically random tokens** generated with the OS CSPRNG. Tokens are delivered as `HttpOnly; SameSite=Strict` cookies to prevent JavaScript access and CSRF.

Sessions are stored in an in-memory `HashMap` with TTL expiry (default 24 hours, configurable via `session_ttl_secs`). Sessions are cleared on server restart.

### API Keys

- Generated with 128 bits of randomness.
- Prefixed with `mm_` for easy identification in logs and secret scanners.
- **SHA-256 hashed** before storage. The plaintext key is returned only once at creation time.
- Compared using **constant-time equality** to prevent timing side-channel attacks.

---

## Input Validation & SQL Injection Prevention

### Parameterized Queries

All user-supplied values (site IDs, date ranges, event names) are bound to SQL statements as parameters using DuckDB's prepared statement API. Raw string interpolation is used only where DuckDB's API does not support parameters (e.g., `COPY TO` file paths), and those values are explicitly validated and escaped before use.

### Path Traversal Prevention

The `site_id` value is validated by `is_safe_path_component()` before being used in any filesystem path. The following are rejected:
- Empty strings
- Strings containing `..` (directory traversal)
- Strings containing `/` or `\` (path separators)
- Strings containing null bytes (`\0`)
- Strings longer than 256 characters

### Funnel and Sequence Step Validation

User-supplied funnel and sequence steps (from `?steps=` query parameters) are parsed from a safe `page:/path` or `event:name` format. Raw SQL expressions are never accepted from the API. Single quotes in path values are escaped by doubling.

### Origin Validation

When `site_ids` is configured, the `Origin` header is validated with **exact host matching**:

- `https://example.com` → passes (if `"example.com"` is in `site_ids`).
- `http://example.com:8080` → passes (explicit port suffix allowed).
- `https://example.com.evil.com` → **rejected** (prefix match is explicitly disallowed).

### CSV Injection Prevention

The CSV export endpoint (`GET /api/stats/export?format=csv`) escapes fields that start with formula-triggering characters (`=`, `+`, `-`, `@`) by prefixing them with a single quote, preventing formula injection when the CSV is opened in spreadsheet software.

---

## Brute-Force Protection

Login attempts are tracked per client IP address. After `max_login_attempts` consecutive failures (default 5), the IP is locked out for `login_lockout_secs` seconds (default 300). The server returns `429 Too Many Requests` with a `Retry-After` header containing the remaining lockout duration in seconds.

A successful login clears the failure count for that IP. Failure counts are stored in memory and reset on server restart.

Configure using TOML fields `max_login_attempts` and `login_lockout_secs`, or the environment variables `MALLARD_MAX_LOGIN_ATTEMPTS` and `MALLARD_LOGIN_LOCKOUT`. Set `max_login_attempts = 0` to disable.

---

## Security Headers

All HTTP responses include these OWASP-recommended headers:

| Header | Value |
|---|---|
| `X-Content-Type-Options` | `nosniff` — prevents MIME-type sniffing |
| `X-Frame-Options` | `DENY` — prevents clickjacking via iframe embedding |
| `Referrer-Policy` | `strict-origin-when-cross-origin` — limits referrer leakage |
| `Content-Security-Policy` | HTML responses only — restricts scripts and resources to same origin |

---

## HTTP Timeout

All requests have a 30-second server-side timeout. Connections that do not complete within this window are closed with `408 Request Timeout`. This prevents Slowloris-style attacks that hold connections open indefinitely.

---

## CSRF Protection

State-mutating endpoints authenticated via session cookie (login, logout, setup, key creation, key revocation) validate the `Origin` or `Referer` header against the configured `dashboard_origin`. Requests with a mismatched or missing origin receive `403 Forbidden`.

When `dashboard_origin` is not set, CSRF checks are bypassed (all origins allowed). Set `dashboard_origin` in production to enable CSRF protection.

---

## Network Security

### CORS Policy

Mallard Metrics uses separate CORS policies for ingestion and dashboard routes:

**Ingestion** (`POST /api/event`):
```
Access-Control-Allow-Origin: *
Access-Control-Allow-Methods: POST
```

**Dashboard / Stats / Admin** (when `dashboard_origin` is set):
```
Access-Control-Allow-Origin: <configured origin>
Access-Control-Allow-Methods: GET, POST, DELETE
Access-Control-Allow-Credentials: true
```

If `dashboard_origin` is not configured, the dashboard routes use a permissive policy that allows any origin. Set `dashboard_origin` in production to restrict cross-origin access to your dashboard domain.

### TLS

Mallard Metrics does not handle TLS directly. In production, place it behind a TLS-terminating reverse proxy (nginx, Caddy, Traefik, etc.).

---

## Supply Chain

- All Rust dependencies are audited with `cargo-deny` in CI.
- GitHub Actions steps are pinned to exact commit SHAs (not floating tags).
- The `bundled` DuckDB feature compiles DuckDB from source as part of the build; no pre-built DuckDB binaries are downloaded at runtime.
