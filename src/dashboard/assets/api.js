/*
 * The dashboard's only door to the server.
 *
 * Two jobs that used to be absent:
 *
 *  1. Concurrency. A cold dashboard load fired ~31 requests at once against a
 *     server that answers 429 beyond `max_concurrent_queries` (default 10), so
 *     the first thing a visitor saw was a wall of "HTTP 429". Requests now go
 *     through a small queue, a handful in flight at a time.
 *  2. Honest failure. A 429 is retried with backoff (honouring Retry-After); a
 *     refused connection, an HTML error page from a proxy, or a body that is not
 *     JSON each become a short sentence a person can act on, never raw JSON and
 *     never "undefined".
 *
 * Failures are told apart by the `code` the server puts in every error body
 * (`bad_request`, `unauthorized`, `not_found`, `internal_error`,
 * `too_many_requests`, `behavioral_unavailable`, `query_timeout`), never by
 * matching the wording of its message, which is written for people and free to
 * change. A response with no code at all (a proxy's HTML page, an older server)
 * is classified by its status alone.
 *
 * Every call resolves; none rejects. Callers switch on `result.kind`:
 *   ok           - `data` holds the parsed body
 *   unauthorized - the session ended (401)
 *   unavailable  - `behavioral_unavailable` (503): the report needs data this
 *                  server cannot provide, either because the `behavioral`
 *                  extension is not loaded or because gdpr_mode rounds
 *                  timestamps to the hour. A state of the deployment, not a fault.
 *   timeout      - `query_timeout` (504): valid request, too much work
 *   busy         - 429 that outlasted the retries
 *   error        - anything else the server refused or failed; `message` is displayable
 *   network      - the server could not be reached at all
 *   aborted      - the caller cancelled it; ignore the result
 */

export const MAX_IN_FLIGHT = 4;
const MAX_ATTEMPTS = 5;
const BASE_DELAY_MS = 400;
const MAX_DELAY_MS = 8000;

/** Seconds from a Retry-After header, or null. HTTP dates are ignored: the server sends seconds. */
export function retryAfterSeconds(value) {
  if (value == null) return null;
  const n = Number(value);
  return Number.isFinite(n) && n >= 0 ? n : null;
}

/** How long to wait before attempt number `attempt` (0-based retry index). */
export function backoffMs(attempt, retryAfter, random = Math.random) {
  const exponential = BASE_DELAY_MS * 2 ** attempt;
  const asked = retryAfter == null ? 0 : retryAfter * 1000;
  return Math.min(MAX_DELAY_MS, Math.max(exponential, asked)) + Math.floor(random() * 250);
}

/** The stable error code from a response body, or null when it has none the dashboard knows. */
export const ERROR_CODES = [
  'bad_request',
  'unauthorized',
  'not_found',
  'internal_error',
  'too_many_requests',
  'behavioral_unavailable',
  'query_timeout',
];

export function errorCode(body) {
  return body && typeof body.code === 'string' && ERROR_CODES.includes(body.code) ? body.code : null;
}

/**
 * What kind of failure a non-OK response is, from its `code` and, only when
 * there is no code, its status.
 */
export function classifyFailure(status, body) {
  const code = errorCode(body);
  if (code === 'behavioral_unavailable') return 'unavailable';
  if (code === 'query_timeout' || (code === null && status === 504)) return 'timeout';
  if (code === 'too_many_requests' || (code === null && status === 429)) return 'busy';
  return 'error';
}

/** A sentence for a failed response. Prefers the server's own message when it is short and plain. */
export function describeFailure(status, body) {
  const said = body && typeof body.error === 'string' ? body.error.trim() : '';
  if (said && said.length <= 300) return said;
  if (status === 429) return 'The server is busy. Try again in a moment.';
  if (status === 403) return 'You do not have permission to view this.';
  if (status === 404) return 'This report is not available on this server.';
  if (status >= 500) return `The server reported an error (HTTP ${status}).`;
  return `The server rejected the request (HTTP ${status}).`;
}

/** The sentence for a failure of a given kind. Timeouts and a busy server get the dashboard's own wording. */
export function failureMessage(kind, status, body) {
  if (kind === 'timeout') return 'This report took too long to compute.';
  if (kind === 'busy') return 'The server is busy right now.';
  return describeFailure(status, body);
}

const sleep = (ms, signal) =>
  new Promise((resolve, reject) => {
    const timer = setTimeout(resolve, ms);
    signal?.addEventListener(
      'abort',
      () => {
        clearTimeout(timer);
        reject(new DOMException('aborted', 'AbortError'));
      },
      { once: true },
    );
  });

export class Api {
  constructor({ fetchImpl, limit = MAX_IN_FLIGHT, wait = sleep } = {}) {
    this.fetch = fetchImpl ?? ((...args) => fetch(...args));
    this.limit = limit;
    this.wait = wait;
    this.active = 0;
    this.queue = [];
    // Counters the browser check reads to prove a cold load never surfaced a 429.
    this.stats = { requests: 0, retried: 0, peak: 0 };
  }

  /** Run `task` when a slot is free. */
  slot(task, signal) {
    return new Promise((resolve, reject) => {
      const start = () => {
        this.active += 1;
        this.stats.peak = Math.max(this.stats.peak, this.active);
        task().then(resolve, reject).finally(() => {
          this.active -= 1;
          const next = this.queue.shift();
          if (next) next();
        });
      };
      if (signal?.aborted) {
        reject(new DOMException('aborted', 'AbortError'));
        return;
      }
      if (this.active < this.limit) {
        start();
      } else {
        this.queue.push(() => {
          if (!signal?.aborted) return start();
          // Cancelled while waiting: hand the free slot to whoever is next,
          // or the queue stalls behind an entry that will never run.
          reject(new DOMException('aborted', 'AbortError'));
          const next = this.queue.shift();
          if (next) next();
        });
      }
    });
  }

  /** Fetch with 429 retry; resolves with a Response or throws. */
  async fetchWithRetry(url, options, signal) {
    for (let attempt = 0; ; attempt += 1) {
      this.stats.requests += 1;
      const res = await this.fetch(url, { ...options, signal });
      if (res.status !== 429 || attempt >= MAX_ATTEMPTS - 1) return res;
      this.stats.retried += 1;
      await this.wait(backoffMs(attempt, retryAfterSeconds(res.headers.get('retry-after'))), signal);
    }
  }

  /** GET a JSON endpoint. Never rejects. */
  async get(url, { signal } = {}) {
    try {
      return await this.slot(async () => {
        const res = await this.fetchWithRetry(url, { headers: { Accept: 'application/json' } }, signal);
        if (res.status === 401) return { kind: 'unauthorized' };

        let body = null;
        let parsed = false;
        try {
          body = await res.json();
          parsed = true;
        } catch (e) {
          /* not JSON: handled below */
        }

        if (res.ok) {
          return parsed
            ? { kind: 'ok', data: body }
            : { kind: 'error', status: res.status, message: 'The server sent a response the dashboard cannot read.' };
        }
        const failure = parsed ? body : null;
        const kind = classifyFailure(res.status, failure);
        const code = errorCode(failure);
        const message = failureMessage(kind, res.status, failure);
        return { kind, status: res.status, code, message, retryable: kind !== 'unavailable' && (res.status === 429 || res.status >= 500) };
      }, signal);
    } catch (e) {
      if (e && e.name === 'AbortError') return { kind: 'aborted' };
      return { kind: 'network', message: 'Cannot reach the server. Check that it is running, then retry.' };
    }
  }

  /** POST/DELETE JSON for the small mutating calls. Never rejects. */
  async send(method, url, payload) {
    try {
      const res = await this.fetch(url, {
        method,
        headers: payload === undefined ? {} : { 'Content-Type': 'application/json' },
        body: payload === undefined ? undefined : JSON.stringify(payload),
      });
      let body = null;
      try {
        body = await res.json();
      } catch (e) {
        /* an empty body is fine */
      }
      if (res.ok) return { kind: 'ok', data: body };
      if (res.status === 401) return { kind: 'unauthorized' };
      return { kind: 'error', status: res.status, message: failureMessage(classifyFailure(res.status, body), res.status, body) };
    } catch (e) {
      return { kind: 'network', message: 'Cannot reach the server. Check that it is running, then retry.' };
    }
  }

  /** GET a file (CSV/JSON export) as a Blob, with the same retry and error handling. */
  async download(url, { signal } = {}) {
    try {
      return await this.slot(async () => {
        const res = await this.fetchWithRetry(url, {}, signal);
        if (res.status === 401) return { kind: 'unauthorized' };
        if (!res.ok) {
          let body = null;
          try {
            body = await res.json();
          } catch (e) {
            /* keep the status-based sentence */
          }
          return { kind: 'error', status: res.status, message: failureMessage(classifyFailure(res.status, body), res.status, body) };
        }
        const disposition = res.headers.get('content-disposition') || '';
        const name = /filename="?([^";]+)"?/i.exec(disposition)?.[1] ?? null;
        return { kind: 'ok', blob: await res.blob(), filename: name };
      }, signal);
    } catch (e) {
      if (e && e.name === 'AbortError') return { kind: 'aborted' };
      return { kind: 'network', message: 'Cannot reach the server. Check that it is running, then retry.' };
    }
  }
}
