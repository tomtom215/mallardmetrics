# Tracking Script

The Mallard Metrics tracking script (`mallard.js`) is served by the server at `GET /mallard.js`. It is under 1 KB, sets no cookies, and loads asynchronously.

## Basic Embed

```html
<script
  async
  defer
  src="https://your-instance.com/mallard.js"
  data-domain="your-site.com">
</script>
```

**Attributes:**

| Attribute | Required | Description |
|---|---|---|
| `data-domain` | Yes | The site ID to record events under. Must match an entry in `site_ids` if that config option is set. |

## Automatic Tracking

Once embedded, the script automatically fires a `pageview` event on every page load with the following data:

| Field | Source |
|---|---|
| `pathname` | `window.location.pathname + search + hash` |
| `referrer` | `document.referrer` |
| `screen_size` | `window.innerWidth + 'x' + window.innerHeight` |
| User-Agent | Sent in request header, parsed server-side |
| UTM parameters | Extracted from URL query string |

## Custom Events

Use `window.mallard(eventName, options)` to track custom actions:

```javascript
// Simple event
window.mallard('signup');

// Event with custom properties
window.mallard('purchase', {
  props: { plan: 'pro', coupon: 'SAVE20' }
});

// Revenue event
window.mallard('checkout', {
  revenue: 99.00,
  currency: 'USD'
});

// Event with callback
window.mallard('form_submit', {
  props: { form: 'contact' },
  callback: function() {
    console.log('Event recorded');
  }
});
```

### Options

| Option | Type | Description |
|---|---|---|
| `props` | `object` | Custom properties stored as JSON in the `props` column. Queryable via `json_extract`. |
| `revenue` | `number` | Revenue amount (stored as `DECIMAL(12,2)`). |
| `currency` | `string` | ISO 4217 currency code (3 characters, e.g. `"USD"`). |
| `callback` | `function` | Called after the event is successfully recorded. |

## Outbound Link Tracking

To track outbound link clicks, call `window.mallard` before navigating:

```javascript
document.querySelectorAll('a[href^="http"]').forEach(function(link) {
  link.addEventListener('click', function(e) {
    window.mallard('outbound_link', {
      props: { url: link.href },
      callback: function() { window.location = link.href; }
    });
    e.preventDefault();
  });
});
```

## Single-Page App Support

For SPAs, call `window.mallard('pageview')` manually after each route change:

```javascript
// Example with a router
router.afterEach(function(to) {
  window.mallard('pageview');
});
```

## Server-Side Events (No Script)

You can also send events directly to the API without the browser script. This is useful for server-rendered pages or background jobs:

```bash
curl -X POST https://your-instance.com/api/event \
  -H 'Content-Type: application/json' \
  -d '{
    "d": "your-site.com",
    "n": "signup",
    "u": "https://your-site.com/signup"
  }'
```

See [Event Ingestion API](api-reference/ingestion.md) for the full request schema.
