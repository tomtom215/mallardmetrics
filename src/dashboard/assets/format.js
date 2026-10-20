/*
 * Pure helpers for the dashboard: number and date formatting, the URL <-> state
 * mapping, and date-range validation.
 *
 * Nothing in here touches the DOM, `fetch` or Preact, which is deliberate:
 * `scripts/check-dashboard-methods.mjs` imports this file under plain Node and
 * asserts on it, so the rules that decide what a user is told (a number, a
 * date, why a range is rejected) are tested rather than eyeballed.
 */

export const DASH = '—';

/* ── Numbers ────────────────────────────────────────────────────────────── */

const NUMBER = new Intl.NumberFormat();
const COMPACT = new Intl.NumberFormat(undefined, {
  notation: 'compact',
  maximumFractionDigits: 1,
});

/** True for a finite number; everything else renders as an em dash. */
const isNum = (v) => typeof v === 'number' && Number.isFinite(v);

/** A count with thousands separators. Never "undefined", "NaN" or "null". */
export function num(value) {
  return isNum(value) ? NUMBER.format(value) : DASH;
}

/** Like `num`, but 1,500,000 becomes 1.5M so a headline figure cannot overflow its card. */
export function compact(value) {
  if (!isNum(value)) return DASH;
  return Math.abs(value) >= 1e6 ? COMPACT.format(value) : NUMBER.format(value);
}

/** Axis tick: always short. */
export function tick(value) {
  if (!isNum(value)) return DASH;
  return Math.abs(value) >= 10000 ? COMPACT.format(value) : NUMBER.format(value);
}

/** A 0-1 fraction as a percentage. */
export function pct(value, digits = 1) {
  return isNum(value) ? `${(value * 100).toFixed(digits)}%` : DASH;
}

/** A fixed-decimals figure, tolerant of null (the API sends null for "unavailable"). */
export function fixed(value, digits = 2) {
  return isNum(value) ? value.toFixed(digits) : DASH;
}

/** A duration in seconds as "3m 07s" or "42s". */
export function duration(seconds) {
  if (!isNum(seconds)) return DASH;
  const total = Math.round(seconds);
  const mins = Math.floor(total / 60);
  const secs = total % 60;
  return mins > 0 ? `${mins}m ${String(secs).padStart(2, '0')}s` : `${secs}s`;
}

export function money(amount, currency) {
  if (!isNum(amount)) return DASH;
  try {
    return new Intl.NumberFormat(undefined, { style: 'currency', currency }).format(amount);
  } catch (e) {
    return `${amount.toFixed(2)} ${currency ?? ''}`.trim();
  }
}

/* ── Dates ──────────────────────────────────────────────────────────────── */

// Events are stored as naive UTC, and the API's periods are UTC days. Formatting
// in the viewer's zone would show a bar for "Oct 3" under the label "Oct 2" for
// anyone west of Greenwich, so every date is formatted in UTC.
const DATE_LONG = new Intl.DateTimeFormat(undefined, {
  year: 'numeric',
  month: 'short',
  day: 'numeric',
  timeZone: 'UTC',
});
const DATE_SHORT = new Intl.DateTimeFormat(undefined, {
  month: 'short',
  day: 'numeric',
  timeZone: 'UTC',
});
const MONTH_YEAR = new Intl.DateTimeFormat(undefined, {
  month: 'short',
  year: 'numeric',
  timeZone: 'UTC',
});
const HOUR = new Intl.DateTimeFormat(undefined, {
  hour: 'numeric',
  minute: '2-digit',
  timeZone: 'UTC',
});

/** Parse "YYYY-MM-DD" (optionally followed by a time) as a UTC instant, or null. */
export function parseDate(text) {
  if (typeof text !== 'string') return null;
  const m = /^(\d{4})-(\d{2})-(\d{2})(?:[T ](\d{2}):(\d{2}))?/.exec(text);
  if (!m) return null;
  const d = new Date(Date.UTC(+m[1], +m[2] - 1, +m[3], +(m[4] ?? 0), +(m[5] ?? 0)));
  // Date.UTC rolls 2026-02-31 into March; reject rather than display a wrong day.
  return d.getUTCFullYear() === +m[1] && d.getUTCMonth() === +m[2] - 1 && d.getUTCDate() === +m[3]
    ? d
    : null;
}

/** "Oct 3, 2026" - the one date format used for prose and tables. */
export function fmtDate(text) {
  const d = parseDate(text);
  return d ? DATE_LONG.format(d) : String(text ?? DASH);
}

/** "Oct 3" - for chart axes where the range already implies the year. */
export function fmtDay(text) {
  const d = parseDate(text);
  return d ? DATE_SHORT.format(d) : String(text ?? DASH);
}

/** "Oct 2026" - for axes spanning many months, where "Oct 25" would read as a day. */
export function fmtMonth(text) {
  const d = parseDate(text);
  return d ? MONTH_YEAR.format(d) : String(text ?? DASH);
}

/** "2:00 PM" for hourly buckets. */
export function fmtHour(text) {
  const d = parseDate(text);
  return d ? HOUR.format(d) : String(text ?? DASH);
}

/** True when a series bucket carries a time of day (period=day returns hours). */
export function hasTime(text) {
  return typeof text === 'string' && /^\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}/.test(text);
}

const pad = (n) => String(n).padStart(2, '0');

/** YYYY-MM-DD for a Date, in UTC. */
export function isoDay(date) {
  return `${date.getUTCFullYear()}-${pad(date.getUTCMonth() + 1)}-${pad(date.getUTCDate())}`;
}

export function todayUTC(now = new Date()) {
  return isoDay(now);
}

/** `days` days before the given YYYY-MM-DD (negative for after). */
export function shiftDay(day, days) {
  const d = parseDate(day);
  d.setUTCDate(d.getUTCDate() - days);
  return isoDay(d);
}

/** Calendar presets for the custom-range editor, as inclusive UTC dates. */
export function rangePreset(name, now = new Date()) {
  const today = todayUTC(now);
  const y = now.getUTCFullYear();
  const m = now.getUTCMonth();
  switch (name) {
    case 'yesterday': {
      const day = shiftDay(today, 1);
      return { start: day, end: day };
    }
    case 'this-month':
      return { start: isoDay(new Date(Date.UTC(y, m, 1))), end: today };
    case 'last-month':
      return {
        start: isoDay(new Date(Date.UTC(y, m - 1, 1))),
        end: isoDay(new Date(Date.UTC(y, m, 0))),
      };
    default:
      return null;
  }
}

/** The inclusive span the API's `period` values cover, for display. */
export function periodDays(period) {
  return { day: 1, today: 1, '7d': 7, '30d': 30, '90d': 90, '12mo': 365 }[period] ?? null;
}

export const MAX_RANGE_DAYS = 366;

/**
 * Why a custom range would be rejected, or null when it is acceptable.
 *
 * Mirrors the server's rules (inclusive dates, both required, at most 366
 * days) so the message appears beside the inputs instead of as a 400 in every
 * panel at once.
 */
export function validateRange(start, end, today = todayUTC()) {
  if (!start || !end) return 'Choose both a start and an end date.';
  const s = parseDate(start);
  const e = parseDate(end);
  if (!s || !e) return 'Enter dates as YYYY-MM-DD.';
  if (s > e) return 'The start date must be on or before the end date.';
  if (end > today) return 'The end date cannot be in the future.';
  const span = Math.round((e - s) / 86400000) + 1;
  if (span > MAX_RANGE_DAYS) {
    return `A range can cover at most ${MAX_RANGE_DAYS} days; this one covers ${num(span)}.`;
  }
  return null;
}

/* ── Segment filters ────────────────────────────────────────────────────── */

/** Render the active segment the way the API parses it. */
export function serializeFilters(filters) {
  return filters.map((f) => `${f.dimension}${f.negated ? '!=' : '=='}${f.value}`).join(';');
}

/**
 * Parse `dimension==value;dimension!=value`.
 *
 * `allowed` is the set of dimensions that can be filtered on; anything else is
 * dropped, because a hand-edited or stale link must not turn into a 400 across
 * the whole dashboard. At most one condition per dimension survives, as in the
 * UI, and at most 12 overall, as on the server.
 */
export function parseFilters(text, allowed) {
  if (!text) return [];
  const out = [];
  for (const part of text.split(';')) {
    const m = /^([a-z-]+)(==|!=)(.+)$/s.exec(part);
    if (!m) continue;
    const [, dimension, op, value] = m;
    if (allowed && !allowed.has(dimension)) continue;
    if (value.length > 512) continue;
    if (out.some((f) => f.dimension === dimension)) continue;
    out.push({ dimension, negated: op === '!=', value });
    if (out.length >= 12) break;
  }
  return out;
}

/* ── URL state ──────────────────────────────────────────────────────────── */

export const PERIODS = ['day', '7d', '30d', '90d', '12mo'];
export const COMPARE_MODES = ['none', 'previous_period', 'year_over_year'];

/**
 * Read the shareable view out of a query string.
 *
 * Returns only the keys that were present and valid, so callers can lay the
 * result over their defaults. Anything malformed is ignored rather than
 * rejected: a bookmark from an older version must still open.
 */
export function readViewState(search, allowedFilters) {
  const p = new URLSearchParams(search);
  const out = {};
  const site = p.get('site');
  if (site && site.length <= 256) out.siteId = site;

  const period = p.get('period');
  const start = p.get('start');
  const end = p.get('end');
  if (start && end && /^\d{4}-\d{2}-\d{2}$/.test(start) && /^\d{4}-\d{2}-\d{2}$/.test(end)) {
    if (validateRange(start, end) === null) {
      out.period = 'custom';
      out.startDate = start;
      out.endDate = end;
    }
  } else if (period && (PERIODS.includes(period) || period === 'today')) {
    out.period = period === 'today' ? 'day' : period;
  }

  const compare = p.get('compare');
  if (compare && COMPARE_MODES.includes(compare)) out.compare = compare;

  const filters = p.get('filters');
  if (filters) out.filters = parseFilters(filters, allowedFilters);
  return out;
}

/** The query string (without "?") that reproduces a view. Defaults are omitted. */
export function writeViewState({ siteId, period, startDate, endDate, compare, filters }) {
  const p = new URLSearchParams();
  if (siteId) p.set('site', siteId);
  if (period === 'custom' && startDate && endDate) {
    p.set('start', startDate);
    p.set('end', endDate);
  } else if (period && period !== '30d') {
    p.set('period', period);
  }
  if (compare && compare !== 'none') p.set('compare', compare);
  if (filters && filters.length > 0) p.set('filters', serializeFilters(filters));
  return p.toString();
}

/* ── Charts ─────────────────────────────────────────────────────────────── */

/** Round `max` up to a value that gives clean gridlines. */
export function niceMax(max) {
  if (!isNum(max) || max <= 0) return 1;
  const exp = 10 ** Math.floor(Math.log10(max));
  const f = max / exp;
  const nice = f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10;
  return nice * exp;
}

/** Whole-number gridline values from 0 to `top` (a `niceMax` result). */
export function axisTicks(top) {
  const steps = [4, 5, 2, 3, 1].find((c) => Number.isInteger(top / c)) ?? 1;
  return Array.from({ length: steps + 1 }, (_, i) => (top * i) / steps);
}
