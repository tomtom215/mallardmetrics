# Event Ingestion

## `POST /api/event`

Records a single analytics event. This endpoint is called by the tracking script automatically and can also be called directly for server-side event recording.

**Authentication:** None required. The `Origin` header is validated against `site_ids` if that config option is set.

**CORS:** Fully permissive (`Access-Control-Allow-Origin: *`) to allow cross-origin calls from the tracking script.

### Request Body

```json
{
  "d": "example.com",
  "n": "pageview",
  "u": "https://example.com/pricing",
  "r": "https://google.com/",
  "w": 1920,
  "p": "{\"plan\": \"pro\"}",
  "ra": 99.00,
  "rc": "USD"
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `d` | string | Yes | Domain / site identifier. Must be non-empty. |
| `n` | string | Yes | Event name (e.g. `"pageview"`, `"signup"`, `"purchase"`). |
| `u` | string | Yes | Full URL of the page where the event occurred. |
| `r` | string | No | Referrer URL. |
| `w` | number | No | Screen width in pixels (for device-type detection). |
| `p` | string | No | Custom properties as a JSON-encoded string. Stored in the `props` column and queryable via `json_extract`. |
| `ra` | number | No | Revenue amount (stored as `DECIMAL(12,2)`). |
| `rc` | string | No | ISO 4217 currency code (e.g. `"USD"`, `"EUR"`). Maximum 3 characters. |

### Response

```
HTTP/1.1 202 Accepted
```

The response body is empty. `202` means the event was accepted into the buffer. It will be flushed to Parquet on the next flush cycle or when the buffer threshold is reached.

### Validation Errors

| Condition | Status |
|---|---|
| Missing required field (`d`, `n`, or `u`) | 422 Unprocessable |
| Empty `d` (site ID) | 400 Bad Request |
| `Origin` header does not match `site_ids` | 403 Forbidden |
| Rate limit exceeded for this `site_id` | 429 Too Many Requests |

### Bot Filtering

When `filter_bots = true` (default), the server inspects the `User-Agent` header and discards the event if it matches known bot patterns. A `202` is still returned — the event is silently dropped rather than returning an error.

### Privacy Processing

Before the event is stored:

1. The client IP address is extracted from the request.
2. A daily-rotating HMAC-SHA256 `visitor_id` is computed from `IP + User-Agent + today's UTC date + MALLARD_SECRET`.
3. The IP address is discarded. It is never written to disk or the database.

### Server-Side Example

```bash
curl -X POST https://your-instance.com/api/event \
  -H 'Content-Type: application/json' \
  -d '{
    "d": "example.com",
    "n": "server_signup",
    "u": "https://example.com/signup"
  }'
```
