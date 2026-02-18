# API Reference

Mallard Metrics exposes a JSON HTTP API. All endpoints are served by the same process as the dashboard.

## Base URL

```
http://your-instance.com
```

## Authentication

Most `/api/stats/*` and `/api/keys/*` endpoints require authentication. Provide one of:

1. **Session cookie** — Set after `POST /api/auth/login`. Sent automatically by browsers.
2. **Bearer token** — An API key in the `Authorization: Bearer mm_...` header.

Endpoints that do not require authentication:
- `POST /api/event` — Event ingestion (uses `Origin` allowlist instead).
- `POST /api/auth/login`, `POST /api/auth/setup`, `GET /api/auth/status`, `POST /api/auth/logout`
- `GET /health`, `GET /health/detailed`, `GET /metrics`
- `GET /` (dashboard)

## Content Type

All request bodies are `application/json`. All responses are `application/json` unless otherwise noted.

## Error Responses

Errors are returned as JSON objects:

```json
{
  "error": "human-readable description",
  "code": "MACHINE_CODE"
}
```

Common HTTP status codes:

| Code | Meaning |
|---|---|
| 202 | Event accepted (ingestion only) |
| 400 | Bad request — missing or invalid parameters |
| 401 | Unauthenticated — no valid session or API key |
| 403 | Forbidden — origin not in allowlist |
| 422 | Unprocessable — JSON validation failed |
| 429 | Rate limited |
| 500 | Internal server error |

## Sections

- [Event Ingestion](ingestion.md) — `POST /api/event`
- [Analytics Stats](stats.md) — `GET /api/stats/*`
- [Authentication](auth.md) — `POST /api/auth/*`, `POST /api/keys/*`
- [Health & Metrics](health.md) — `GET /health`, `GET /metrics`
