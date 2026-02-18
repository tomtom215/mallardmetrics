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

// Response 200
{"message": "Admin password configured"}
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
{"message": "Logged in"}
```

Sessions are stored in memory and expire after `session_ttl_secs` (default 24 hours). Sessions are cleared on server restart.

---

### `POST /api/auth/logout`

Invalidates the current session.

**Session cookie required.**

```
// Response 200
{"message": "Logged out"}
```

---

### `GET /api/auth/status`

Returns the current authentication state.

```json
// Not logged in, no password configured
{"authenticated": false, "has_password": false}

// Not logged in, password configured
{"authenticated": false, "has_password": true}

// Logged in
{"authenticated": true, "has_password": true, "username": "admin"}
```

---

## API Key Management

API keys are prefixed with `mm_` and are SHA-256 hashed before storage. The plaintext key is only returned once at creation time.

All key management endpoints require authentication.

### `POST /api/keys`

Creates a new API key.

```json
// Request
{"name": "ci-pipeline", "scope": "read_only"}

// Response 201
{
  "key": "mm_abc123...",
  "name": "ci-pipeline",
  "key_hash": "sha256:abc..."
}
```

The `key` field is the only time the plaintext key is returned. Store it securely.

**Scopes:** `read_only` (currently the only supported scope).

---

### `GET /api/keys`

Lists all active API keys (without plaintext values).

```json
[
  {
    "name": "ci-pipeline",
    "key_hash": "sha256:abc...",
    "created_at": "2024-01-15T10:00:00Z"
  }
]
```

---

### `DELETE /api/keys/{key_hash}`

Revokes an API key by its hash.

```
// Response 204 No Content
```

---

## Using API Keys

Pass the key as a Bearer token in the `Authorization` header:

```bash
curl https://your-instance.com/api/stats/main?site_id=example.com&period=30d \
  -H "Authorization: Bearer mm_abc123..."
```
