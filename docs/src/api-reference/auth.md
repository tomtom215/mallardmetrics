# Authentication API

Mallard Metrics supports two forms of authentication:

1. **Session cookies** — For human dashboard users.
2. **API keys** — For programmatic access (CI/CD, integrations, monitoring).

---

## Dashboard Authentication

### `POST /api/auth/setup`

Sets the admin password for the first time. Returns `409 Conflict` if a password is already configured.

The Argon2id hash is written to `data_dir/admin.json` at mode 0600 and loaded on
every subsequent start, so the password survives a restart. Before this it lived
only in memory: a restart put the instance back into first-run mode, and whoever
reached it first — not necessarily the operator — could claim it. If the hash
cannot be written the request fails with `500` rather than accepting a password
it would silently lose.

`MALLARD_ADMIN_PASSWORD`, when set, takes precedence over the stored hash and is
deliberately not written to disk, so removing the variable does not leave a
stale credential behind.

**No authentication required.**

```json
// Request — password must be at least 12 characters
{"password": "your-secure-password"}

// Response 200 — also sets HttpOnly, SameSite=Strict cookie mm_session
{"token": "<session-token>"}

// Response 400 — password too short
{"error": "Password must be at least 12 characters"}

// Response 409 — password already configured
{"error": "Admin password already configured"}
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

// Response 400 — no password configured yet
{"error": "No admin password configured. Use /api/auth/setup first."}

// Response 401 — wrong password
{"error": "Invalid password"}

// Response 429 — Too Many Requests (IP locked out after max failed attempts)
// Retry-After header contains the remaining lockout seconds
{"error": "Too many failed login attempts. Try again later."}
```

Sessions are stored in memory and expire after `session_ttl_secs` (default 24 hours). Sessions are cleared on server restart.

**Brute-force protection:** After `max_login_attempts` (default 5) consecutive failures from the same IP, the IP is locked out for `login_lockout_secs` (default 300 seconds). A successful login clears the failure count. Configure via `MALLARD_MAX_LOGIN_ATTEMPTS` and `MALLARD_LOGIN_LOCKOUT` environment variables, or the corresponding TOML fields. Set `max_login_attempts = 0` to disable.

---

### `POST /api/auth/logout`

Invalidates the current session.

**No authentication required.** If a valid session cookie is present, it is invalidated. Otherwise the endpoint is a no-op (always returns 200).

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
| `setup_required` | boolean | `true` when no admin password has been set. Analytics reads are open in this state. |
| `authenticated` | boolean | `true` when the request carries a valid session or API key, or when `setup_required` is `true`. |

Open access covers **reads only**. Key management and `DELETE /api/gdpr/erase`
return `401` until an admin password exists — before setup there is no admin to
authorise, and a key minted in that window would keep working afterwards. Run
`POST /api/auth/setup` before exposing an instance to a network.

---

## API Key Management

API keys are prefixed with `mm_` and are SHA-256 hashed before storage. The plaintext key is only returned once at creation time.

All key management endpoints require **admin** authentication: a session, or an
API key with the `admin` scope. A read-only key gets `403`, and every one of
them gets `401` before setup has run.

These endpoints are also reachable from the dashboard — the **API keys** button
beside the period selector — which lists, creates and revokes keys. It appears
once an admin password exists, because there is nothing to authorise before
then.

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

Both headers are accepted on all stats and admin endpoints. `ReadOnly` keys can access stats endpoints; all key management endpoints (`GET /api/keys`, `POST /api/keys`, `DELETE /api/keys/{hash}`) require an `Admin`-scoped key.
