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
| GDPR/CCPA compliant by design | No personal data is stored. No consent banner required |

### Data Protection

- **Storage format** -- Event data is stored in Parquet files with ZSTD compression, organized by `site_id` and `date` for partition pruning
- **Encryption at rest** -- Not provided by Mallard Metrics itself. Use filesystem-level encryption (e.g., LUKS, dm-crypt) if required
- **Data retention** -- Configurable automatic deletion of old partitions via `MALLARD_RETENTION_DAYS`

---

## Authentication and Access Control

### Dashboard Authentication

- **Password hashing** -- Argon2id with default parameters (memory-hard, GPU-resistant)
- **Session tokens** -- 256-bit cryptographic random tokens, stored as HttpOnly cookies
- **Cookie attributes** -- HttpOnly, Secure (when behind TLS), SameSite=Lax
- **Session expiration** -- Configurable TTL (default: 24 hours) via `MALLARD_SESSION_TTL`

### API Key Management

- **Key format** -- `mm_` prefix followed by a cryptographic random string
- **Storage** -- Keys are SHA-256 hashed at rest. Plaintext keys are shown only once at creation time
- **Operations** -- Create, list, and revoke via `/api/keys` endpoints
- **Scope** -- API keys provide read access to analytics endpoints

### Route Protection

| Route Group | Authentication Required |
|---|---|
| `POST /api/event` | No (tracking script must work without auth) |
| `GET /health`, `GET /health/detailed` | No |
| `GET /metrics` | No |
| `/auth/*` | No (these are the auth endpoints themselves) |
| `GET /api/stats/*` | Yes (session cookie or API key) |
| `/api/keys/*` | Yes (session cookie only) |
| `GET /api/stats/export` | Yes (session cookie or API key) |

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

### Rate Limiting

Per-site token-bucket rate limiting is available on the ingestion endpoint. Configure via `MALLARD_RATE_LIMIT` (max events/sec per site, 0 = unlimited). For additional protection, a reverse proxy (nginx, Caddy) can provide IP-based rate limiting.

### Bot Filtering

Known bot User-Agents are automatically filtered from analytics when `MALLARD_FILTER_BOTS=true` (default). This prevents automated crawlers and scrapers from inflating visitor counts.

---

## Threat Model

| Threat | Mitigation |
|---|---|
| SQL injection | Parameterized queries for all user input. Safe format parsing for funnel/sequence conditions |
| XSS | Input sanitization, control character removal, length limits |
| Data exfiltration | No external network calls, embedded database, authenticated API access |
| PII leakage | IP addresses never stored. Daily hash rotation. No cookies |
| Brute force (login) | Argon2id hashing (inherently slow), configurable rate limiting |
| Brute force (API) | Per-site token-bucket rate limiting on ingestion |
| Session hijacking | HttpOnly cookies, Secure flag with TLS, SameSite=Lax, 256-bit random tokens |
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
