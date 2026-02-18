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

To distinguish unique visitors without storing PII, Mallard Metrics computes:

```
visitor_id = HMAC-SHA256(
    key   = MALLARD_SECRET,
    input = IP_address + User-Agent + today_UTC_date
)
```

Properties of this approach:

- **Deterministic within a day** — The same visitor from the same browser produces the same ID throughout the day, enabling accurate unique-visitor counting.
- **Rotates daily** — The UTC date is included in the input, so the ID changes every 24 hours. A visitor cannot be tracked across days.
- **Not reversible** — Without `MALLARD_SECRET`, the IP address cannot be recovered from the stored hash.
- **No IP storage** — The input to the HMAC is discarded after hashing.

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

If `dashboard_origin` is not configured, the dashboard routes also use a permissive policy (same-origin browsers work without any extra configuration).

### TLS

Mallard Metrics does not handle TLS directly. In production, place it behind a TLS-terminating reverse proxy (nginx, Caddy, Traefik, etc.).

---

## Supply Chain

- All Rust dependencies are audited with `cargo-deny` in CI.
- GitHub Actions steps are pinned to exact commit SHAs (not floating tags).
- The `bundled` DuckDB feature compiles DuckDB from source as part of the build; no pre-built DuckDB binaries are downloaded at runtime.
