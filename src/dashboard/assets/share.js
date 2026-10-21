/*
 * Share links, client side.
 *
 * A share link looks like  https://host/#share=ms_<64 hex digits>.  The token
 * sits in the URL *fragment* on purpose: a browser never sends the fragment to
 * the server, to a proxy, or in a Referer header, so it cannot reach a log. The
 * dashboard reads it here, keeps it in memory only (never localStorage or
 * sessionStorage), and sends it as the X-Share-Token header to the read-only
 * /api/shared/ routes. Reloading works because the fragment stays in the
 * address bar; see docs/src/share-links.md for what that costs and why.
 *
 * Nothing in this file touches the DOM or Preact, so the rules are asserted in
 * scripts/check-dashboard-methods.mjs.
 */

export const TOKEN_PATTERN = /^ms_[0-9a-f]{64}$/;
export const TOKEN_HEADER = 'X-Share-Token';

/** The token in a location hash (`#share=ms_...`), or null when there is none or it is malformed. */
export function readShareToken(hash) {
  const text = String(hash ?? '').replace(/^#/, '');
  for (const part of text.split('&')) {
    if (!part.startsWith('share=')) continue;
    const token = part.slice('share='.length);
    return TOKEN_PATTERN.test(token) ? token : null;
  }
  return null;
}

/** Whether the address asks for a shared view at all, even with a token that cannot work. */
export function wantsSharedView(hash) {
  return /(^#|&)share=/.test(String(hash ?? ''));
}

/** The link to hand to a viewer. */
export function shareLink(origin, token) {
  return `${origin}/#share=${token}`;
}

/**
 * The URL a shared view fetches in place of an ordinary dashboard URL, or null
 * when a shared view may not fetch it at all.
 *
 * Only the read-only report routes exist for a viewer: no export, no site list,
 * nothing administrative. The server refuses everything else on its own; this
 * keeps the browser from even trying.
 */
export function sharedUrl(url) {
  const [path, query] = String(url).split('?');
  if (path.startsWith('/api/shared/')) return url;
  if (!path.startsWith('/api/stats/') || path === '/api/stats/export') return null;
  return `/api/shared/${path.slice('/api/'.length)}${query === undefined ? '' : `?${query}`}`;
}

/** A `fetch` that speaks for a share link: rewrites the URL, adds the header, refuses everything else. */
export function makeShareFetch(token, fetchImpl = (...args) => fetch(...args)) {
  return (url, options = {}) => {
    const target = sharedUrl(url);
    if (target === null || (options.method && options.method.toUpperCase() !== 'GET')) {
      return Promise.reject(new TypeError('a shared view cannot make this request'));
    }
    return fetchImpl(target, { ...options, headers: { ...(options.headers ?? {}), [TOKEN_HEADER]: token }, credentials: 'omit', referrerPolicy: 'no-referrer' });
  };
}

/** Choices for how long a new link lives. `days: null` is no expiry. */
export const EXPIRY_CHOICES = [
  { value: '7', label: '7 days', days: 7 },
  { value: '30', label: '30 days', days: 30 },
  { value: '90', label: '90 days', days: 90 },
  { value: '365', label: '1 year', days: 365 },
  { value: 'never', label: 'Never', days: null },
];

/** The `expires_in_days` the API takes for a choice value. */
export function expiryDays(value) {
  const choice = EXPIRY_CHOICES.find((c) => c.value === value);
  return choice ? choice.days : null;
}

/** Words for a link's state. */
export function linkStatusLabel(status) {
  return { active: 'Active', expired: 'Expired', revoked: 'Revoked' }[status] ?? 'Unknown';
}
