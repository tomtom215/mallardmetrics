# Authentication API

Two kinds of credential:

1. **Session cookie** (`mm_session`) for the dashboard.
2. **API keys** (`mm_` prefix) for scripts, sent as `Authorization: Bearer <key>`
   or `X-API-Key: <key>`.

Handler errors on this page are `{"error": "..."}` **without** a `code` field
(see [Errors](index.md#errors)).

---

## Dashboard authentication

### `POST /api/auth/setup`

Sets the admin password for the first time. Available only while no password
exists; afterwards it returns `409`.

```bash
curl -i -X POST http://localhost:8000/api/auth/setup \
  -H 'Content-Type: application/json' \
  -d '{"password": "a-long-enough-password"}'
```

```
HTTP/1.1 200 OK
set-cookie: mm_session=<64 hex chars>; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400
content-type: application/json

{"status":"ok"}
```

The session token travels **only** in the cookie; the body does not repeat it.

| Status | Body | When |
|---|---|---|
| 200 | `{"status":"ok"}` | Password stored; you are logged in. |
| 400 | `{"error":"Password must be at least 12 characters"}` | Too short (counted in characters). |
| 400 | `{"error":"Password is too common; choose something unpredictable"}` | A 12+ character value from a short built-in list (`mallardmetrics`, `123456789012`, ...). |
| 403 | `{"error":"Cross-origin request refused"}` | `dashboard_origin` is set and `Origin`/`Referer` differs. |
| 409 | `{"error":"Admin password already configured"}` | A password exists (from the environment or an earlier setup). |
| 429 | `{"error":"Too many failed attempts. Try again later."}` + `Retry-After` | The caller's address is locked out (shares the login attempt counter). |
| 500 | `{"error":"Could not store the admin password. ..."}` | `data_dir/admin.json` could not be written. Nothing was accepted. |

The Argon2id hash is written to `data_dir/admin.json` (mode 0600) and loaded on
every start. `MALLARD_ADMIN_PASSWORD`, when set, takes precedence, passes the
same policy at startup (a rejected value stops the server), and is never written
to disk.

### `POST /api/auth/login`

```bash
curl -i -c cookies.txt -X POST http://localhost:8000/api/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"password": "your-password"}'
```

| Status | Body | When |
|---|---|---|
| 200 | `{"status":"ok"}` + `Set-Cookie: mm_session=...` | Success. |
| 400 | `{"error":"No admin password configured. Use /api/auth/setup first."}` | Before setup. |
| 401 | `{"error":"Invalid password"}` | Wrong password. |
| 403 | `{"error":"Cross-origin request refused"}` | CSRF origin check failed. |
| 429 | `{"error":"Too many failed attempts. Try again later."}` | Locked out. `Retry-After` is the remaining lockout in seconds. |
| 503 | `{"error":"Server is busy. Try again shortly."}` + `Retry-After: 1` | Four password checks were already running and none finished within 10 s. The attempt is not counted. |

The cookie has `HttpOnly; SameSite=Strict; Path=/; Max-Age=<session_ttl_secs>`
and gains `Secure` when `secure_cookies` is true or `dashboard_origin` starts
with `https://`. Sessions live in memory and are lost on restart.

**Lockout.** Attempts are counted per client address (IPv4 address, or IPv6
/64), *when the attempt is admitted*, not after the check. After
`max_login_attempts` (default 5) consecutive failures the address is locked for
`login_lockout_secs` (default 300). A success clears the count. At most four
Argon2 operations run at once, process-wide.

### `POST /api/auth/logout`

Invalidates the session in the cookie, if any. Always `200`:

```json
{"status":"logged_out"}
```

with a `Set-Cookie` that expires `mm_session` (same attributes, including
`Secure`, as the cookie it replaces). `403` if the CSRF origin check fails.

### `GET /api/auth/status`

```json
{"setup_required": false, "authenticated": false}
```

| Field | Notes |
|---|---|
| `setup_required` | `true` while no admin password exists. |
| `authenticated` | `true` for a valid session or API key. **Also `true` while `setup_required` is true**, because reads are open in that state. |

Open access covers **reads only**. Key management and `DELETE /api/gdpr/erase`
answer `401` until a password exists, so a key cannot be minted in the
unconfigured window. Run setup before exposing an instance to a network.

---

## API key management

All three endpoints need **admin** authentication (a session, or an `Admin`
key). Keys are SHA-256 hashed in `data_dir/api_keys.json`; the plaintext is
returned once. The dashboard's **API keys** button wraps these endpoints.

### `POST /api/keys`

```bash
curl -X POST http://localhost:8000/api/keys \
  -H 'Authorization: Bearer mm_<admin key>' -H 'Content-Type: application/json' \
  -d '{"name": "ci-pipeline", "scope": "ReadOnly"}'
```

```json
{"key":"mm_e09d2a6e...","key_hash":"426db04a...","name":"ci-pipeline","scope":"ReadOnly"}
```

Status `201`. `name` is 1–128 characters (`400` otherwise). `scope` is exactly
`ReadOnly` or `Admin` (case-sensitive; `readonly` is a `422`). A `500` means the
key store could not be written; the change was rolled back.

| Scope | Access |
|---|---|
| `ReadOnly` | All read routes. |
| `Admin` | Everything, including key management and erasure. |

### `GET /api/keys`

```json
[{"key_hash":"426db04a...","name":"ci-pipeline","scope":"ReadOnly","created_at":"2026-10-21T00:19:25Z","revoked":false}]
```

Revoked keys stay listed with `"revoked": true` until the periodic cleanup
removes them.

### `DELETE /api/keys/{key_hash}`

Revokes the key with that SHA-256 hex hash.

```json
{"status":"revoked"}
```

`404` with `{"code":"not_found","error":"Key not found"}` for an unknown hash.

---

## Share tokens are not API keys

A [share link](../share-links.md) carries a token (`ms_...`) in the
`X-Share-Token` header. It opens only the read-only `/api/shared/` routes for
one site. Offered as a key, a bearer token, a cookie or on any other route, it is
refused (`401`), and an API key is likewise refused on the share routes. Share
links are created and revoked with the admin endpoints in
[Share Links](../share-links.md#api).

---

## `POST /api/admin/backup`

Admin scope. Snapshots both storage tiers while serving; see
[Backup and Restore](../backup-restore.md#online-backup-no-downtime) for what is
in it and how to restore.

| Parameter | Default | Description |
|---|---|---|
| `name` | `mallard-<UTC timestamp>` | Directory name under `<data_dir>/backups`: 1-128 characters of letters, digits, `-`, `_` or `.`, not starting with a dot. |

```bash
curl -X POST "https://your-instance.com/api/admin/backup?name=before-upgrade" \
  -H "Authorization: Bearer mm_admin..."
```

```json
{
  "status": "created",
  "name": "before-upgrade",
  "path": "/data/backups/before-upgrade",
  "parquet_files": 212,
  "parquet_bytes": 48123456,
  "hot_rows": 318,
  "state_files": 3
}
```

`201` on success. `400` for an unsafe `name`, `409` (`code: conflict`) when a backup
of that name exists (nothing is overwritten), `401`/`403` without an admin
credential (including before setup), `500` if the copy fails (the partial
directory is removed).

---

## Using API keys

```bash
curl "https://your-instance.com/api/stats/main?site_id=example.com&period=30d" \
  -H "Authorization: Bearer mm_abc123..."

curl "https://your-instance.com/api/stats/main?site_id=example.com&period=30d" \
  -H "X-API-Key: mm_abc123..."
```

A session cookie is checked first, then `Authorization`, then `X-API-Key`.
Verified: a `ReadOnly` key reads stats (`200`) and gets `403` from
`GET /api/keys`; an `Admin` key lists keys.
