# Authentication API

Mallard Metrics supports two forms of authentication:

1. **Session cookies** — For human dashboard users.
2. **API keys** — For programmatic access (CI/CD, integrations, monitoring).

---

## Dashboard Authentication

### `POST /api/auth/setup`

Sets the admin password for the first time. Returns `409 Conflict` if a password is already configured.

**No authentication required.**

```json
// Request
{"password": "your-secure-password"}

// Response 200 — also sets HttpOnly, SameSite=Strict cookie mm_session
{"token": "<session-token>"}
```

Passwords are hashed with Argon2id before storage. The plaintext password is never persisted.

---

### `POST /api/auth/login`

Authenticates with the admin password and creates a session.

**No authentication required.**

```json
// Request
{"password": "your-secure-password"}

// Response 200 — sets HttpOnly, SameSite=Strict cookie mm_session
{"token": "<session-token>"}

// Response 429 — Too Many Requests (IP locked out after max failed attempts)
// Retry-After header contains the remaining lockout seconds
{"error": "Too many login attempts. Try again later."}
```

Sessions are stored in memory and expire after `session_ttl_secs` (default 24 hours). Sessions are cleared on server restart.

**Brute-force protection:** After `max_login_attempts` (default 5) consecutive failures from the same IP, the IP is locked out for `login_lockout_secs` (default 300 seconds). A successful login clears the failure count. Configure via `MALLARD_MAX_LOGIN_ATTEMPTS` and `MALLARD_LOGIN_LOCKOUT` environment variables, or the corresponding TOML fields. Set `max_login_attempts = 0` to disable.

---

### `POST /api/auth/logout`

Invalidates the current session.

**Session cookie required.**

```json
// Response 200 — clears mm_session cookie
{"status": "logged_out"}
```

---

### `GET /api/auth/status`

Returns the current authentication state.

```json
// No password configured (open access mode)
{"setup_required": true, "authenticated": true}

// Password configured, not logged in
{"setup_required": false, "authenticated": false}

// Password configured, logged in
{"setup_required": false, "authenticated": true}
```

| Field | Type | Notes |
|---|---|---|
| `setup_required` | boolean | `true` when no admin password has been set. System is in open-access mode. |
| `authenticated` | boolean | `true` when the request carries a valid session or API key, or when `setup_required` is `true`. |

---

## API Key Management

API keys are prefixed with `mm_` and are SHA-256 hashed before storage. The plaintext key is only returned once at creation time.

All key management endpoints require authentication.

### `POST /api/keys`

Creates a new API key.

```json
// Request
{"name": "ci-pipeline", "scope": "ReadOnly"}

// Response 201
{
  "key": "mm_abc123...",
  "key_hash": "a1b2c3...",
  "name": "ci-pipeline",
  "scope": "ReadOnly"
}
```

The `key` field is the only time the plaintext key is returned. Store it securely.

**Scopes:**

| Value | Access |
|---|---|
| `ReadOnly` | Read-only access to stats queries. |
| `Admin` | Full admin access (key management, config). |

---

### `GET /api/keys`

Lists all API keys (without plaintext values).

```json
[
  {
    "key_hash": "a1b2c3...",
    "name": "ci-pipeline",
    "scope": "ReadOnly",
    "created_at": "2024-01-15T10:00:00Z",
    "revoked": false
  }
]
```

---

### `DELETE /api/keys/{key_hash}`

Revokes an API key by its SHA-256 hex hash.

```json
// Response 200
{"status": "revoked"}

// Response 404 if hash not found
{"error": "Key not found"}
```

---

## Using API Keys

API keys can be passed in two ways:

**Authorization header (Bearer token):**

```bash
curl "https://your-instance.com/api/stats/main?site_id=example.com&period=30d" \
  -H "Authorization: Bearer mm_abc123..."
```

**X-API-Key header:**

```bash
curl "https://your-instance.com/api/stats/main?site_id=example.com&period=30d" \
  -H "X-API-Key: mm_abc123..."
```

Both headers are accepted on all stats and admin endpoints. `ReadOnly` keys can access stats endpoints; key management endpoints (`POST /api/keys`, `DELETE /api/keys/{hash}`) require an `Admin`-scoped key.
