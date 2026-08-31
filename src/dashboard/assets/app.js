import { h, render, Component } from '/preact.js';
import htm from '/htm.js';

const html = htm.bind(h);

/* ── Small helpers ─────────────────────────────────────────────────────── */

const STORAGE_KEY = 'mallard.dashboard.v2';

/** Read persisted UI state. Storage can throw in private mode. */
function loadPrefs() {
  try {
    return JSON.parse(localStorage.getItem(STORAGE_KEY)) || {};
  } catch (e) {
    return {};
  }
}

function savePrefs(prefs) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(prefs));
  } catch (e) {
    /* storage unavailable; preferences simply do not persist */
  }
}

/** Format a count with thousands separators. */
function num(value) {
  if (value == null) return '—';
  return value.toLocaleString();
}

/** Format a 0–1 fraction as a percentage. */
function pct(value, digits = 1) {
  if (value == null) return '—';
  return `${(value * 100).toFixed(digits)}%`;
}

/** Format a duration in seconds as m:ss, or "—" when unavailable. */
function duration(seconds) {
  if (seconds == null) return '—';
  const total = Math.round(seconds);
  const mins = Math.floor(total / 60);
  const secs = total % 60;
  return mins > 0 ? `${mins}m ${String(secs).padStart(2, '0')}s` : `${secs}s`;
}

function money(amount, currency) {
  if (amount == null) return '—';
  try {
    return new Intl.NumberFormat(undefined, { style: 'currency', currency }).format(amount);
  } catch (e) {
    return `${amount.toFixed(2)} ${currency}`;
  }
}

/**
 * Fetch JSON, distinguishing the outcomes the dashboard must render
 * differently: unauthenticated, feature-unavailable, and everything else.
 */
async function getJSON(url) {
  const res = await fetch(url, { headers: { Accept: 'application/json' } });
  if (res.status === 401) return { kind: 'unauthorized' };
  if (res.ok) return { kind: 'ok', data: await res.json() };

  let message = `HTTP ${res.status}`;
  try {
    const body = await res.json();
    if (body && body.error) message = body.error;
  } catch (e) {
    /* keep the status-code message */
  }
  // 503 means the behavioral extension is missing: the deployment is healthy,
  // the feature simply is not available, and saying so beats an empty panel.
  return { kind: res.status === 503 ? 'unavailable' : 'error', message };
}

/* ── Presentational components ─────────────────────────────────────────── */

function Panel({ title, subtitle, children, actions }) {
  return html`
    <section class="panel">
      <header class="panel-header">
        <div>
          <h2>${title}</h2>
          ${subtitle && html`<p class="panel-subtitle">${subtitle}</p>`}
        </div>
        ${actions}
      </header>
      ${children}
    </section>
  `;
}

/** Renders whichever of loading / unavailable / error / empty applies. */
function Placeholder({ state, empty }) {
  if (!state) return null;
  if (state.loading) return html`<div class="placeholder">Loading…</div>`;
  if (state.kind === 'unavailable') {
    return html`
      <div class="placeholder placeholder-unavailable">
        <strong>Not available</strong>
        <p>${state.message}</p>
      </div>
    `;
  }
  if (state.kind === 'error') {
    return html`<div class="placeholder placeholder-error">${state.message}</div>`;
  }
  if (empty) return html`<div class="placeholder">No data for this period</div>`;
  return null;
}

/**
 * Relative change from `before` to `after`, as a rendered chip.
 *
 * Returns null when there is nothing honest to say: no comparison was
 * requested, a value is missing, or the baseline was zero — "up from zero" has
 * no percentage, and showing one anyway (or a bare "+100%") is the kind of
 * number people quote in a meeting.
 */
function Delta({ before, after, invert }) {
  if (before == null || after == null || before === 0) return null;
  const change = (after - before) / before;
  if (!Number.isFinite(change)) return null;

  const rounded = Math.round(change * 1000) / 10;
  if (rounded === 0) {
    return html`<div class="metric-delta metric-delta-flat" title="No change">±0%</div>`;
  }
  // For bounce rate, down is the good direction — so the colour follows the
  // metric's meaning, not the arithmetic sign.
  const good = invert ? rounded < 0 : rounded > 0;
  return html`
    <div class=${`metric-delta ${good ? 'metric-delta-up' : 'metric-delta-down'}`}>
      ${rounded > 0 ? '▲' : '▼'} ${Math.abs(rounded).toFixed(1)}%
    </div>
  `;
}

function MetricCard({ label, value, hint, muted, delta }) {
  return html`
    <div class=${`metric-card${muted ? ' metric-muted' : ''}`}>
      <div class="metric-value">${value}</div>
      <div class="metric-label">${label}</div>
      ${delta}
      ${hint && html`<div class="metric-hint">${hint}</div>`}
    </div>
  `;
}

/**
 * A line chart with a hover readout.
 *
 * Kept as hand-written SVG: the dashboard ships with no build step and no
 * third-party chart library, and its content security policy allows scripts
 * only from this origin.
 */
class TimeseriesChart extends Component {
  constructor() {
    super();
    this.state = { hover: null };
  }

  render({ data }, { hover }) {
    if (!data || data.length === 0) {
      return html`<div class="placeholder">No data for this period</div>`;
    }

    const W = 900;
    const H = 260;
    const PAD = { top: 16, right: 16, bottom: 32, left: 56 };
    const cw = W - PAD.left - PAD.right;
    const ch = H - PAD.top - PAD.bottom;

    const maxY = Math.max(1, ...data.map((d) => Math.max(d.visitors, d.pageviews)));
    const xAt = (i) => (data.length > 1 ? PAD.left + (i * cw) / (data.length - 1) : PAD.left + cw / 2);
    const yAt = (v) => PAD.top + ch - (v / maxY) * ch;

    const line = (key) => data.map((d, i) => `${xAt(i)},${yAt(d[key])}`).join(' ');
    const area = (key) =>
      `${PAD.left},${PAD.top + ch} ${line(key)} ${xAt(data.length - 1)},${PAD.top + ch}`;

    const ticks = [0, Math.round(maxY / 2), maxY].filter((v, i, a) => a.indexOf(v) === i);
    const labelStep = Math.max(1, Math.ceil(data.length / 7));

    const onMove = (event) => {
      const svg = event.currentTarget;
      const rect = svg.getBoundingClientRect();
      const x = ((event.clientX - rect.left) / rect.width) * W;
      const ratio = (x - PAD.left) / (cw || 1);
      const index = Math.round(ratio * (data.length - 1));
      this.setState({ hover: index >= 0 && index < data.length ? index : null });
    };

    const point = hover != null ? data[hover] : null;

    return html`
      <div class="chart">
        <svg
          viewBox="0 0 ${W} ${H}"
          class="chart-svg"
          role="img"
          aria-label="Visitors and pageviews over time"
          onMouseMove=${onMove}
          onMouseLeave=${() => this.setState({ hover: null })}
        >
          ${ticks.map(
            (v) => html`
              <line class="chart-gridline" x1=${PAD.left} y1=${yAt(v)} x2=${W - PAD.right} y2=${yAt(v)} />
              <text class="chart-tick" x=${PAD.left - 10} y=${yAt(v) + 4} text-anchor="end">${num(v)}</text>
            `,
          )}

          <polygon class="chart-area" points=${area('pageviews')} />
          <polyline class="chart-line chart-line-pageviews" points=${line('pageviews')} />
          <polyline class="chart-line chart-line-visitors" points=${line('visitors')} />

          ${data.map((d, i) =>
            i % labelStep === 0 || i === data.length - 1
              ? html`<text class="chart-tick" x=${xAt(i)} y=${H - 10} text-anchor="middle">
                  ${d.date.length > 10 ? d.date.slice(5) : d.date.slice(5)}
                </text>`
              : null,
          )}

          ${point &&
          html`
            <line class="chart-cursor" x1=${xAt(hover)} y1=${PAD.top} x2=${xAt(hover)} y2=${PAD.top + ch} />
            <circle class="chart-dot chart-dot-visitors" cx=${xAt(hover)} cy=${yAt(point.visitors)} r="4" />
            <circle class="chart-dot chart-dot-pageviews" cx=${xAt(hover)} cy=${yAt(point.pageviews)} r="4" />
          `}
        </svg>

        <div class="chart-footer">
          <div class="chart-legend">
            <span class="legend-item"><span class="legend-swatch swatch-visitors"></span> Visitors</span>
            <span class="legend-item"><span class="legend-swatch swatch-pageviews"></span> Pageviews</span>
          </div>
          <div class="chart-readout">
            ${point
              ? html`<strong>${point.date}</strong> · ${num(point.visitors)} visitors ·
                  ${num(point.pageviews)} pageviews`
              : html`<span class="muted">Hover the chart for a breakdown</span>`}
          </div>
        </div>
      </div>
    `;
  }
}

/** A ranked list with an inline bar for relative magnitude. */
function BreakdownList({ rows, valueLabel = 'Visitors', onFilter }) {
  if (!rows || rows.length === 0) {
    return html`<div class="placeholder">No data</div>`;
  }
  const max = Math.max(...rows.map((r) => r.visitors), 1);
  return html`
    <table class="data-table">
      <thead>
        <tr>
          <th>Value</th>
          <th class="numeric">${valueLabel}</th>
          <th class="numeric">Pageviews</th>
        </tr>
      </thead>
      <tbody>
        ${rows.map(
          (row) => html`
            <tr>
              <td class="bar-cell">
                <span class="bar" style=${`width:${(row.visitors / max) * 100}%`}></span>
                ${onFilter
                  ? html`<button
                      class="bar-label bar-link"
                      title=${`Filter to ${row.value}`}
                      onClick=${() => onFilter(row.value)}
                    >
                      ${row.value}
                    </button>`
                  : html`<span class="bar-label" title=${row.value}>${row.value}</span>`}
              </td>
              <td class="numeric">${num(row.visitors)}</td>
              <td class="numeric">${num(row.pageviews)}</td>
            </tr>
          `,
        )}
      </tbody>
    </table>
  `;
}

/** The active segment, with a chip per condition. */
function FilterBar({ filters, onRemove, onClear }) {
  if (!filters || filters.length === 0) return null;
  return html`
    <div class="filter-bar" role="region" aria-label="Active filters">
      <span class="filter-bar-label">Filtered by</span>
      ${filters.map(
        (f, i) => html`
          <span class="filter-chip">
            <span class="filter-chip-dim">${FILTER_LABELS[f.dimension] || f.dimension}</span>
            <span class="filter-chip-op">${f.negated ? 'is not' : 'is'}</span>
            <span class="filter-chip-value" title=${f.value}>${f.value}</span>
            <button
              class="filter-chip-remove"
              aria-label=${`Remove filter ${f.dimension} ${f.value}`}
              onClick=${() => onRemove(i)}
            >
              ×
            </button>
          </span>
        `,
      )}
      <button class="filter-clear" onClick=${onClear}>Clear all</button>
    </div>
  `;
}

/** Tabbed group of breakdowns sharing one panel. */
class BreakdownTabs extends Component {
  constructor(props) {
    super(props);
    this.state = { active: props.tabs[0].slug };
  }

  render({ tabs, data, onFilter }, { active }) {
    const current = data[active];
    const filterable = onFilter && FILTERABLE_SLUGS.has(active);
    return html`
      <div class="tabs">
        <div class="tab-strip" role="tablist">
          ${tabs.map(
            (tab) => html`
              <button
                role="tab"
                aria-selected=${active === tab.slug}
                class=${`tab${active === tab.slug ? ' tab-active' : ''}`}
                onClick=${() => this.setState({ active: tab.slug })}
              >
                ${tab.label}
              </button>
            `,
          )}
        </div>
        <div class="tab-panel">
          ${current && current.kind !== 'ok'
            ? html`<${Placeholder} state=${current} />`
            : html`<${BreakdownList}
                rows=${(current && current.data) || []}
                onFilter=${filterable ? (value) => onFilter(active, value) : null}
              />`}
        </div>
      </div>
    `;
  }
}

function RealtimePanel({ state }) {
  if (!state || state.kind !== 'ok') {
    return html`<${Panel} title="Right now"><${Placeholder} state=${state} /></${Panel}>`;
  }
  const data = state.data;
  const max = Math.max(...data.per_minute, 1);
  return html`
    <${Panel}
      title="Right now"
      subtitle=${`Last ${data.window_minutes} minutes`}
    >
      <div class="realtime">
        <div class="realtime-headline">
          <div class="realtime-count">${num(data.current_visitors)}</div>
          <div class="realtime-label">current visitors</div>
        </div>
        <div class="realtime-spark" aria-hidden="true">
          ${data.per_minute.map(
            (value) => html`<span class="spark-bar" style=${`height:${Math.max(2, (value / max) * 100)}%`}></span>`,
          )}
        </div>
      </div>
      <div class="realtime-lists">
        <div>
          <h3>Top pages</h3>
          <${BreakdownList}
            rows=${data.top_pages.map((p) => ({ ...p, pageviews: p.visitors }))}
          />
        </div>
        <div>
          <h3>Top sources</h3>
          <${BreakdownList}
            rows=${data.top_sources.map((p) => ({ ...p, pageviews: p.visitors }))}
          />
        </div>
      </div>
    </${Panel}>
  `;
}

function FunnelChart({ state }) {
  if (!state || state.kind !== 'ok') return html`<${Placeholder} state=${state} />`;
  const steps = state.data;
  if (steps.length === 0) return html`<div class="placeholder">No funnel data</div>`;

  const entered = steps[0].visitors;
  return html`
    <div class="funnel">
      ${steps.map(
        (step) => html`
          <div class="funnel-step">
            <div class="funnel-meta">
              <span class="funnel-step-name">Step ${step.step}</span>
              <span class="funnel-step-figures">
                ${num(step.visitors)} visitors · ${pct(step.conversion_rate)} of entrants
                ${step.dropped_off > 0
                  ? html`<span class="funnel-drop">−${num(step.dropped_off)} dropped</span>`
                  : null}
              </span>
            </div>
            <div class="funnel-track">
              <div
                class="funnel-fill"
                style=${`width:${entered > 0 ? (step.visitors / entered) * 100 : 0}%`}
              ></div>
            </div>
          </div>
        `,
      )}
    </div>
  `;
}

function RetentionGrid({ state }) {
  if (!state || state.kind !== 'ok') return html`<${Placeholder} state=${state} />`;
  const { cohorts, caveat } = state.data;
  if (!cohorts || cohorts.length === 0) {
    return html`
      ${caveat && html`<div class="notice">${caveat}</div>`}
      <div class="placeholder">No cohorts in this period</div>
    `;
  }

  const width = Math.max(...cohorts.map((c) => c.retained.length));
  return html`
    ${caveat && html`<div class="notice">${caveat}</div>`}
    <div class="table-scroll">
      <table class="data-table retention-table">
        <thead>
          <tr>
            <th>Cohort</th>
            <th class="numeric">Size</th>
            ${Array.from({ length: width }, (_, i) => html`<th class="numeric">W${i}</th>`)}
          </tr>
        </thead>
        <tbody>
          ${cohorts.map(
            (row) => html`
              <tr>
                <td>${row.cohort_date}</td>
                <td class="numeric">${num(row.cohort_size)}</td>
                ${row.retention_rates.map(
                  (rate, i) => html`
                    <td
                      class="numeric retention-cell"
                      style=${`background-color: color-mix(in srgb, var(--accent) ${Math.round(rate * 70)}%, transparent)`}
                      title=${`${num(row.retained[i])} of ${num(row.cohort_size)}`}
                    >
                      ${pct(rate, 0)}
                    </td>
                  `,
                )}
              </tr>
            `,
          )}
        </tbody>
      </table>
    </div>
  `;
}

function RevenuePanel({ state }) {
  if (!state || state.kind !== 'ok') {
    return html`<${Panel} title="Revenue"><${Placeholder} state=${state} /></${Panel}>`;
  }
  const report = state.data;
  if (report.by_currency.length === 0) {
    return html`
      <${Panel} title="Revenue" subtitle="Send a revenue amount with mallard() to populate this">
        <div class="placeholder">No revenue recorded in this period</div>
      </${Panel}>
    `;
  }
  return html`
    <${Panel} title="Revenue" subtitle="Totals are reported per currency and never summed across them">
      <div class="metrics-grid">
        ${report.by_currency.map(
          (row) => html`
            <${MetricCard}
              label=${`${row.currency} revenue`}
              value=${money(row.total, row.currency)}
              hint=${`${num(row.transactions)} orders · ${money(row.average_order_value, row.currency)} average`}
            />
          `,
        )}
      </div>
      <div class="two-column">
        <div>
          <h3>By event</h3>
          <table class="data-table">
            <thead><tr><th>Event</th><th class="numeric">Revenue</th><th class="numeric">Orders</th></tr></thead>
            <tbody>
              ${report.by_event.map(
                (row) => html`<tr>
                  <td>${row.value}</td>
                  <td class="numeric">${money(row.total, row.currency)}</td>
                  <td class="numeric">${num(row.transactions)}</td>
                </tr>`,
              )}
            </tbody>
          </table>
        </div>
        <div>
          <h3>By page</h3>
          <table class="data-table">
            <thead><tr><th>Page</th><th class="numeric">Revenue</th><th class="numeric">Orders</th></tr></thead>
            <tbody>
              ${report.by_page.map(
                (row) => html`<tr>
                  <td>${row.value}</td>
                  <td class="numeric">${money(row.total, row.currency)}</td>
                  <td class="numeric">${num(row.transactions)}</td>
                </tr>`,
              )}
            </tbody>
          </table>
        </div>
      </div>
    </${Panel}>
  `;
}

/** Goals, plus a drill-down into the custom properties attached to them. */
class GoalsPanel extends Component {
  constructor() {
    super();
    this.state = { propertyKey: '', values: null };
  }

  async loadValues(key) {
    if (!key) {
      this.setState({ propertyKey: '', values: null });
      return;
    }
    this.setState({ propertyKey: key, values: { loading: true } });
    const query = `${this.props.query}&key=${encodeURIComponent(key)}`;
    this.setState({ values: await getJSON(`/api/stats/property-values?${query}`) });
  }

  render({ goals, propertyKeys }, { propertyKey, values }) {
    return html`
      <${Panel}
        title="Goals & custom events"
        subtitle="Every event other than a pageview, with the share of visitors who triggered it"
      >
        ${goals && goals.kind !== 'ok'
          ? html`<${Placeholder} state=${goals} />`
          : (goals && goals.data.length > 0
              ? html`
                  <table class="data-table">
                    <thead>
                      <tr>
                        <th>Event</th>
                        <th class="numeric">Visitors</th>
                        <th class="numeric">Events</th>
                        <th class="numeric">Conversion</th>
                      </tr>
                    </thead>
                    <tbody>
                      ${goals.data.map(
                        (row) => html`<tr>
                          <td>${row.name}</td>
                          <td class="numeric">${num(row.visitors)}</td>
                          <td class="numeric">${num(row.events)}</td>
                          <td class="numeric">${pct(row.conversion_rate)}</td>
                        </tr>`,
                      )}
                    </tbody>
                  </table>
                `
              : html`<div class="placeholder">No custom events in this period</div>`)}

        ${propertyKeys && propertyKeys.kind === 'ok' && propertyKeys.data.length > 0
          ? html`
              <div class="property-drilldown">
                <label>
                  Break down a custom property
                  <select value=${propertyKey} onChange=${(e) => this.loadValues(e.target.value)}>
                    <option value="">Choose a property…</option>
                    ${propertyKeys.data.map((key) => html`<option value=${key}>${key}</option>`)}
                  </select>
                </label>
                ${values && values.kind === 'ok'
                  ? html`<${BreakdownList} rows=${values.data.map((v) => ({ ...v, pageviews: v.events }))} />`
                  : html`<${Placeholder} state=${values} />`}
              </div>
            `
          : null}
      </${Panel}>
    `;
  }
}

/* ── Authentication ────────────────────────────────────────────────────── */

/**
 * API key management.
 *
 * `POST/GET/DELETE /api/keys` shipped with no way to reach them from the
 * dashboard, so issuing a key for a script or a Grafana scrape meant hand-
 * writing a curl command with a session cookie pasted out of devtools. The
 * plaintext key is returned once and never again, which is exactly the kind of
 * thing a UI should be careful about — so it stays on screen until dismissed
 * rather than disappearing on the next render.
 */
class KeysPanel extends Component {
  constructor() {
    super();
    this.state = {
      keys: null,
      name: '',
      scope: 'ReadOnly',
      created: null,
      error: null,
      busy: false,
    };
  }

  componentDidMount() {
    this.load();
  }

  async load() {
    const result = await getJSON('/api/keys');
    if (result.kind === 'unauthorized') return this.props.onAuthExpired();
    if (result.kind === 'ok') {
      this.setState({ keys: result.data, error: null });
    } else {
      this.setState({ keys: [], error: result.message });
    }
  }

  async create(event) {
    event.preventDefault();
    const name = this.state.name.trim();
    if (!name) return;
    this.setState({ busy: true, error: null });
    try {
      const res = await fetch('/api/keys', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name, scope: this.state.scope }),
      });
      const body = await res.json().catch(() => ({}));
      if (res.ok) {
        this.setState({ created: body, name: '', busy: false });
        await this.load();
      } else {
        this.setState({ error: body.error || `Error ${res.status}`, busy: false });
      }
    } catch (e) {
      this.setState({ error: 'Could not reach the server.', busy: false });
    }
  }

  async revoke(keyHash, name) {
    // A revoked key stops working immediately and cannot be restored, so the
    // one destructive action in this panel asks first.
    if (!window.confirm(`Revoke the API key "${name}"? Anything using it stops working immediately.`)) {
      return;
    }
    this.setState({ busy: true, error: null });
    try {
      const res = await fetch(`/api/keys/${encodeURIComponent(keyHash)}`, { method: 'DELETE' });
      if (!res.ok) {
        const body = await res.json().catch(() => ({}));
        this.setState({ error: body.error || `Error ${res.status}` });
      }
    } catch (e) {
      this.setState({ error: 'Could not reach the server.' });
    }
    this.setState({ busy: false });
    await this.load();
  }

  render({ onClose }, { keys, name, scope, created, error, busy }) {
    const live = (keys || []).filter((k) => !k.revoked);
    return html`
      <${Panel}
        title="API keys"
        subtitle="Programmatic access to the stats endpoints. Keys are stored as SHA-256 hashes; the key itself is shown once."
        actions=${html`<button class="btn-secondary" onClick=${onClose}>Close</button>`}
      >
        ${created &&
        html`
          <div class="banner banner-key" role="alert">
            <strong>Copy this key now — it is not shown again.</strong>
            <code class="key-value">${created.key}</code>
            <button class="btn-secondary" onClick=${() => this.setState({ created: null })}>
              Done
            </button>
          </div>
        `}

        <form class="inline-controls" onSubmit=${(e) => this.create(e)}>
          <label>
            Name
            <input
              type="text"
              value=${name}
              maxlength="128"
              placeholder="grafana-scraper"
              onInput=${(e) => this.setState({ name: e.target.value })}
              required
            />
          </label>
          <label>
            Scope
            <select value=${scope} onChange=${(e) => this.setState({ scope: e.target.value })}>
              <option value="ReadOnly">Read only — stats endpoints</option>
              <option value="Admin">Admin — also key management and erasure</option>
            </select>
          </label>
          <button type="submit" disabled=${busy || !name.trim()}>Create key</button>
        </form>

        ${error && html`<div class="placeholder placeholder-error">${error}</div>`}

        ${keys === null
          ? html`<div class="placeholder">Loading…</div>`
          : live.length === 0
            ? html`<div class="placeholder">No API keys yet</div>`
            : html`
                <table class="data-table">
                  <thead>
                    <tr>
                      <th>Name</th>
                      <th>Scope</th>
                      <th>Created</th>
                      <th>Fingerprint</th>
                      <th></th>
                    </tr>
                  </thead>
                  <tbody>
                    ${live.map(
                      (key) => html`
                        <tr>
                          <td>${key.name}</td>
                          <td>${key.scope === 'Admin' ? 'Admin' : 'Read only'}</td>
                          <td>${key.created_at.slice(0, 10)}</td>
                          <td><code>${key.key_hash.slice(0, 12)}…</code></td>
                          <td>
                            <button
                              class="btn-secondary"
                              disabled=${busy}
                              onClick=${() => this.revoke(key.key_hash, key.name)}
                            >
                              Revoke
                            </button>
                          </td>
                        </tr>
                      `,
                    )}
                  </tbody>
                </table>
              `}
      </${Panel}>
    `;
  }
}

class LoginForm extends Component {
  constructor() {
    super();
    this.state = { error: null, busy: false };
  }

  async submit(event) {
    event.preventDefault();
    const password = event.target.elements.password.value;
    const endpoint = this.props.setupRequired ? '/api/auth/setup' : '/api/auth/login';
    this.setState({ busy: true, error: null });
    try {
      const res = await fetch(endpoint, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ password }),
      });
      if (res.ok) {
        this.props.onLogin();
        return;
      }
      const body = await res.json().catch(() => ({}));
      this.setState({ error: body.error || `Error ${res.status}`, busy: false });
    } catch (e) {
      this.setState({ error: 'Could not reach the server.', busy: false });
    }
  }

  render({ setupRequired, onCancel }, { error, busy }) {
    return html`
      <div class="auth-overlay">
        <form class="auth-card" onSubmit=${(e) => this.submit(e)}>
          <h1>Mallard Metrics</h1>
          <h2>${setupRequired ? 'Set admin password' : 'Sign in'}</h2>
          <p class="auth-hint">
            ${setupRequired
              ? 'No password is set yet. Choose one to protect this dashboard.'
              : 'Enter your admin password to continue.'}
          </p>
          <input
            type="password"
            name="password"
            placeholder="Password"
            minlength=${setupRequired ? 12 : 1}
            autocomplete=${setupRequired ? 'new-password' : 'current-password'}
            required
            autofocus
          />
          ${setupRequired && html`<p class="auth-hint">At least 12 characters.</p>`}
          ${error && html`<p class="auth-error" role="alert">${error}</p>`}
          <button type="submit" disabled=${busy}>
            ${busy ? 'Working…' : setupRequired ? 'Set password' : 'Sign in'}
          </button>
          ${onCancel &&
          html`<button type="button" class="btn-secondary" onClick=${onCancel}>Not now</button>`}
        </form>
      </div>
    `;
  }
}

/* ── Dashboard ─────────────────────────────────────────────────────────── */

const BREAKDOWN_GROUPS = [
  {
    title: 'Pages',
    tabs: [
      { slug: 'pages', label: 'Top pages' },
      { slug: 'entry-pages', label: 'Entry pages' },
      { slug: 'exit-pages', label: 'Exit pages' },
    ],
  },
  {
    title: 'Acquisition',
    tabs: [
      { slug: 'sources', label: 'Sources' },
      { slug: 'referrers', label: 'Referrers' },
      { slug: 'utm-sources', label: 'UTM source' },
      { slug: 'utm-mediums', label: 'UTM medium' },
      { slug: 'utm-campaigns', label: 'UTM campaign' },
      { slug: 'utm-contents', label: 'UTM content' },
      { slug: 'utm-terms', label: 'UTM term' },
    ],
  },
  {
    title: 'Locations',
    tabs: [
      { slug: 'countries', label: 'Countries' },
      { slug: 'regions', label: 'Regions' },
      { slug: 'cities', label: 'Cities' },
    ],
  },
  {
    title: 'Devices',
    tabs: [
      { slug: 'browsers', label: 'Browsers' },
      { slug: 'browser-versions', label: 'Browser versions' },
      { slug: 'os', label: 'Operating systems' },
      { slug: 'os-versions', label: 'OS versions' },
      { slug: 'devices', label: 'Device types' },
      { slug: 'screen-sizes', label: 'Screen widths' },
    ],
  },
];

const ALL_BREAKDOWN_SLUGS = BREAKDOWN_GROUPS.flatMap((g) => g.tabs.map((t) => t.slug));

// Filter chips read as a sentence — "Page is /pricing" — so they need singular
// labels, not the tab headings ("Top pages is /pricing" reads as nonsense).
const FILTER_LABELS = {
  pages: 'Page',
  referrers: 'Referrer',
  sources: 'Source',
  countries: 'Country',
  regions: 'Region',
  cities: 'City',
  browsers: 'Browser',
  'browser-versions': 'Browser version',
  os: 'OS',
  'os-versions': 'OS version',
  devices: 'Device',
  'screen-sizes': 'Screen width',
  'utm-sources': 'UTM source',
  'utm-mediums': 'UTM medium',
  'utm-campaigns': 'UTM campaign',
  'utm-contents': 'UTM content',
  'utm-terms': 'UTM term',
  events: 'Event',
};

// Entry and exit pages are derived from a whole session rather than stored on
// an event, so the server has no per-event predicate for them and answers 400.
// Rows in those tables are therefore not clickable.
const FILTERABLE_SLUGS = new Set(
  ALL_BREAKDOWN_SLUGS.filter((slug) => slug !== 'entry-pages' && slug !== 'exit-pages'),
);

/** Render the active segment the way the API parses it. */
function serializeFilters(filters) {
  return filters.map((f) => `${f.dimension}${f.negated ? '!=' : '=='}${f.value}`).join(';');
}

const REALTIME_REFRESH_MS = 15000;

class Dashboard extends Component {
  constructor(props) {
    super(props);
    const prefs = loadPrefs();
    this.state = {
      sites: [],
      siteId: prefs.siteId || '',
      period: prefs.period || '30d',
      startDate: prefs.startDate || '',
      endDate: prefs.endDate || '',
      theme: prefs.theme || 'auto',
      loading: false,
      error: null,
      main: null,
      timeseries: null,
      breakdowns: {},
      realtime: null,
      revenue: null,
      goals: null,
      propertyKeys: null,
      funnel: null,
      retention: null,
      sequences: null,
      flow: null,
      funnelSteps: prefs.funnelSteps || 'page:/,page:/pricing,event:signup',
      funnelModes: prefs.funnelModes || '',
      sequenceSteps: prefs.sequenceSteps || 'page:/,event:signup',
      flowPage: prefs.flowPage || '/',
      flowDirection: prefs.flowDirection || 'forward',
      compare: prefs.compare || 'none',
      // Active segment, as [{dimension, negated, value}]. Not persisted: a
      // filter is a question you are asking right now, not a preference.
      filters: [],
      showKeys: false,
    };
  }

  async componentDidMount() {
    this.applyTheme();
    await this.loadSites();
    // Load immediately rather than waiting for the operator to press a button.
    if (this.state.siteId) await this.refresh();
    this.realtimeTimer = setInterval(() => this.refreshRealtime(), REALTIME_REFRESH_MS);
  }

  componentWillUnmount() {
    clearInterval(this.realtimeTimer);
  }

  applyTheme() {
    const { theme } = this.state;
    document.documentElement.dataset.theme = theme === 'auto' ? '' : theme;
  }

  persist() {
    const { siteId, period, startDate, endDate, theme, funnelSteps, funnelModes, sequenceSteps, flowPage, flowDirection, compare } =
      this.state;
    savePrefs({ siteId, period, startDate, endDate, theme, funnelSteps, funnelModes, sequenceSteps, flowPage, flowDirection, compare });
  }

  /** The shared query string for the selected site, range and segment. */
  query() {
    const { siteId, period, startDate, endDate, filters } = this.state;
    const parts = [`site_id=${encodeURIComponent(siteId)}`];
    if (period === 'custom' && startDate && endDate) {
      parts.push(`start_date=${startDate}`, `end_date=${endDate}`);
    } else {
      parts.push(`period=${period}`);
    }
    if (filters.length > 0) {
      parts.push(`filters=${encodeURIComponent(serializeFilters(filters))}`);
    }
    return parts.join('&');
  }

  /** Add a filter, replacing any existing one on the same dimension. */
  addFilter(dimension, value, negated = false) {
    if (!FILTERABLE_SLUGS.has(dimension)) return;
    const filters = this.state.filters
      .filter((f) => f.dimension !== dimension)
      .concat([{ dimension, value, negated }]);
    this.setState({ filters }, () => this.refresh());
  }

  removeFilter(index) {
    const filters = this.state.filters.filter((_, i) => i !== index);
    this.setState({ filters }, () => this.refresh());
  }

  clearFilters() {
    if (this.state.filters.length === 0) return;
    this.setState({ filters: [] }, () => this.refresh());
  }

  async loadSites() {
    const result = await getJSON('/api/sites');
    if (result.kind === 'unauthorized') return this.props.onAuthExpired();
    if (result.kind !== 'ok') return;
    const sites = result.data.sites || [];
    this.setState((prev) => ({
      sites,
      siteId: prev.siteId || sites[0] || '',
    }));
  }

  /** Realtime takes the segment but not the date range: its window is its own. */
  realtimeQuery() {
    const { siteId, filters } = this.state;
    const parts = [`site_id=${encodeURIComponent(siteId)}`];
    if (filters.length > 0) {
      parts.push(`filters=${encodeURIComponent(serializeFilters(filters))}`);
    }
    return parts.join('&');
  }

  async refreshRealtime() {
    if (!this.state.siteId) return;
    const result = await getJSON(`/api/stats/realtime?${this.realtimeQuery()}`);
    if (result.kind === 'unauthorized') return this.props.onAuthExpired();
    this.setState({ realtime: result });
  }

  async refresh() {
    const { siteId, funnelSteps, funnelModes, sequenceSteps, flowPage, flowDirection } = this.state;
    if (!siteId) return;

    this.setState({ loading: true, error: null });
    this.persist();
    const query = this.query();

    const breakdownRequests = ALL_BREAKDOWN_SLUGS.map((slug) =>
      getJSON(`/api/stats/breakdown/${slug}?${query}&limit=10`).then((result) => [slug, result]),
    );

    const [
      main,
      timeseries,
      realtime,
      revenue,
      goals,
      propertyKeys,
      funnel,
      retention,
      sequences,
      flow,
      ...breakdownResults
    ] = await Promise.all([
      getJSON(
        `/api/stats/main?${query}` +
          (this.state.compare === 'none' ? '' : `&compare=${this.state.compare}`),
      ),
      getJSON(`/api/stats/timeseries?${query}`),
      getJSON(`/api/stats/realtime?${this.realtimeQuery()}`),
      getJSON(`/api/stats/revenue?${query}`),
      getJSON(`/api/stats/goals?${query}`),
      getJSON(`/api/stats/properties?${query}`),
      getJSON(
        `/api/stats/funnel?${query}&steps=${encodeURIComponent(funnelSteps)}&window=1 day` +
          (funnelModes ? `&modes=${encodeURIComponent(funnelModes)}` : ''),
      ),
      getJSON(`/api/stats/retention?${query}&weeks=4`),
      getJSON(`/api/stats/sequences?${query}&steps=${encodeURIComponent(sequenceSteps)}`),
      getJSON(`/api/stats/flow?${query}&page=${encodeURIComponent(flowPage)}&direction=${flowDirection}`),
      ...breakdownRequests,
    ]);

    if (main.kind === 'unauthorized') {
      this.props.onAuthExpired();
      return;
    }

    const breakdowns = {};
    for (const [slug, result] of breakdownResults) breakdowns[slug] = result;

    this.setState({
      loading: false,
      error: main.kind === 'error' ? main.message : null,
      main,
      timeseries,
      breakdowns,
      realtime,
      revenue,
      goals,
      propertyKeys,
      funnel,
      retention,
      sequences,
      flow,
    });
  }

  renderControls() {
    const { sites, siteId, period, startDate, endDate, loading, theme } = this.state;
    return html`
      <div class="controls">
        ${sites.length > 0
          ? html`
              <select
                aria-label="Site"
                value=${siteId}
                onChange=${(e) => this.setState({ siteId: e.target.value }, () => this.refresh())}
              >
                ${sites.map((site) => html`<option value=${site}>${site}</option>`)}
              </select>
            `
          : html`
              <input
                type="text"
                aria-label="Site ID"
                placeholder="Site ID (e.g. example.com)"
                value=${siteId}
                onInput=${(e) => this.setState({ siteId: e.target.value })}
              />
            `}

        <select
          aria-label="Period"
          value=${period}
          onChange=${(e) => this.setState({ period: e.target.value }, () => {
            if (e.target.value !== 'custom') this.refresh();
          })}
        >
          <option value="day">Today</option>
          <option value="7d">Last 7 days</option>
          <option value="30d">Last 30 days</option>
          <option value="90d">Last 90 days</option>
          <option value="12mo">Last 12 months</option>
          <option value="custom">Custom range…</option>
        </select>

        ${period === 'custom' &&
        html`
          <input
            type="date"
            aria-label="Start date"
            value=${startDate}
            onInput=${(e) => this.setState({ startDate: e.target.value })}
          />
          <input
            type="date"
            aria-label="End date"
            value=${endDate}
            onInput=${(e) => this.setState({ endDate: e.target.value })}
          />
        `}

        <button onClick=${() => this.refresh()} disabled=${loading}>
          ${loading ? 'Loading…' : 'Refresh'}
        </button>

        <select
          aria-label="Compare against"
          value=${this.state.compare}
          onChange=${(e) => this.setState({ compare: e.target.value }, () => this.refresh())}
        >
          <option value="none">No comparison</option>
          <option value="previous_period">vs. previous period</option>
          <option value="year_over_year">vs. same period last year</option>
        </select>

        <select
          aria-label="Theme"
          value=${theme}
          onChange=${(e) => this.setState({ theme: e.target.value }, () => {
            this.applyTheme();
            this.persist();
          })}
        >
          <option value="auto">Auto theme</option>
          <option value="light">Light</option>
          <option value="dark">Dark</option>
        </select>

        ${this.props.onLogout &&
        html`
          <button
            class="btn-secondary"
            aria-pressed=${this.state.showKeys}
            onClick=${() => this.setState((prev) => ({ showKeys: !prev.showKeys }))}
          >
            API keys
          </button>
        `}

        ${this.props.onLogout &&
        html`<button class="btn-secondary" onClick=${this.props.onLogout}>Sign out</button>`}
      </div>
    `;
  }

  renderHeadline() {
    const { main } = this.state;
    if (!main || main.kind !== 'ok') return html`<${Placeholder} state=${main} />`;
    const m = main.data;
    const behavioralHint = m.behavioral_available ? null : 'Needs the behavioral extension';
    const prev = m.comparison || null;
    // `invert` marks the metrics where a fall is the good news.
    const delta = (key, invert) =>
      prev ? html`<${Delta} before=${prev[key]} after=${m[key]} invert=${invert} />` : null;

    // A baseline of zero produces no percentages — see `Delta` — so the note
    // has to say why, or it reads as a comparison that silently failed.
    const baselineEmpty = prev != null && prev.total_pageviews === 0 && prev.unique_visitors === 0;

    return html`
      ${prev &&
      html`
        <p class="comparison-note">
          Compared with ${prev.start_date} – ${prev.end_date}${baselineEmpty
            ? ' — no data in that period, so there is nothing to compare against'
            : ''}
        </p>
      `}
      <div class="metrics-grid">
        <${MetricCard}
          label="Unique visitors"
          value=${num(m.unique_visitors)}
          delta=${delta('unique_visitors')}
        />
        <${MetricCard}
          label="Pageviews"
          value=${num(m.total_pageviews)}
          delta=${delta('total_pageviews')}
        />
        <${MetricCard}
          label="Total events"
          value=${num(m.total_events)}
          delta=${delta('total_events')}
        />
        <${MetricCard} label="Views per visitor" value=${m.views_per_visitor.toFixed(2)} />
        <${MetricCard}
          label="Visits"
          value=${num(m.total_sessions)}
          hint=${behavioralHint}
          muted=${!m.behavioral_available}
          delta=${delta('total_sessions')}
        />
        <${MetricCard}
          label="Bounce rate"
          value=${pct(m.bounce_rate)}
          hint=${behavioralHint}
          muted=${!m.behavioral_available}
          delta=${delta('bounce_rate', true)}
        />
        <${MetricCard}
          label="Visit duration"
          value=${duration(m.avg_visit_duration_secs)}
          hint=${behavioralHint}
          muted=${!m.behavioral_available}
          delta=${delta('avg_visit_duration_secs')}
        />
        <${MetricCard}
          label="Views per visit"
          value=${m.views_per_visit == null ? '—' : m.views_per_visit.toFixed(2)}
          hint=${behavioralHint}
          muted=${!m.behavioral_available}
        />
      </div>
    `;
  }

  render() {
    const {
      siteId, error, loading, timeseries, breakdowns, realtime, revenue, goals, propertyKeys,
      funnel, retention, sequences, flow, funnelSteps, funnelModes, sequenceSteps, flowPage, flowDirection,
    } = this.state;
    const query = this.query();

    return html`
      <div class="dashboard">
        <header class="app-header">
          <h1>Mallard Metrics</h1>
          ${this.renderControls()}
        </header>

        ${error && html`<div class="banner banner-error" role="alert">${error}</div>`}

        ${this.props.setupRequired &&
        html`
          <div class="banner banner-warning" role="alert">
            <strong>This instance has no admin password.</strong>
            <span>
              Anyone who can reach it can read every site's analytics. Set one
              before exposing it to a network.
            </span>
            <button onClick=${this.props.onStartSetup}>Set admin password</button>
          </div>
        `}

        ${this.state.showKeys &&
        html`
          <${KeysPanel}
            onClose=${() => this.setState({ showKeys: false })}
            onAuthExpired=${this.props.onAuthExpired}
          />
        `}

        ${!siteId && html`<div class="banner">Choose a site to see its analytics.</div>`}

        ${siteId &&
        html`
          <${Panel}
            title="Overview"
            actions=${html`
              <div class="panel-actions">
                <a class="btn-link" href=${`/api/stats/export?${query}&kind=daily&format=csv`}>Daily CSV</a>
                <a class="btn-link" href=${`/api/stats/export?${query}&kind=raw&format=csv`}>Raw CSV</a>
                <a class="btn-link" href=${`/api/stats/export?${query}&kind=raw&format=json`}>Raw JSON</a>
              </div>
            `}
          >
            <${FilterBar}
              filters=${this.state.filters}
              onRemove=${(i) => this.removeFilter(i)}
              onClear=${() => this.clearFilters()}
            />
            ${this.renderHeadline()}
            ${timeseries && timeseries.kind === 'ok'
              ? html`<${TimeseriesChart} data=${timeseries.data} />`
              : html`<${Placeholder} state=${timeseries} />`}
          </${Panel}>

          <${RealtimePanel} state=${realtime} />

          <div class="panel-grid">
            ${BREAKDOWN_GROUPS.map(
              (group) => html`
                <${Panel} title=${group.title}>
                  <${BreakdownTabs}
                    tabs=${group.tabs}
                    data=${breakdowns}
                    onFilter=${(dimension, value) => this.addFilter(dimension, value)}
                  />
                </${Panel}>
              `,
            )}
          </div>

          <${GoalsPanel} goals=${goals} propertyKeys=${propertyKeys} query=${query} />
          <${RevenuePanel} state=${revenue} />

          <${Panel}
            title="Funnel"
            subtitle="Visitors reaching at least each step, within the conversion window"
          >
            <div class="inline-controls">
              <label>
                Steps
                <input
                  type="text"
                  value=${funnelSteps}
                  placeholder="page:/,page:/pricing,event:signup"
                  onInput=${(e) => this.setState({ funnelSteps: e.target.value })}
                />
              </label>
              <label>
                Modes
                <input
                  type="text"
                  value=${funnelModes}
                  placeholder="e.g. strict_order, strict_increase"
                  onInput=${(e) => this.setState({ funnelModes: e.target.value })}
                />
              </label>
              <button onClick=${() => this.refresh()} disabled=${loading}>Apply</button>
            </div>
            <${FunnelChart} state=${funnel} />
          </${Panel}>

          <${Panel} title="Retention cohorts" subtitle="Weekly cohorts by first visit">
            <${RetentionGrid} state=${retention} />
          </${Panel}>

          <${Panel} title="Sequence" subtitle="Visitors whose events matched this ordered pattern">
            <div class="inline-controls">
              <label>
                Steps
                <input
                  type="text"
                  value=${sequenceSteps}
                  placeholder="page:/,event:signup"
                  onInput=${(e) => this.setState({ sequenceSteps: e.target.value })}
                />
              </label>
              <button onClick=${() => this.refresh()} disabled=${loading}>Apply</button>
            </div>
            ${sequences && sequences.kind === 'ok'
              ? html`
                  <div class="metrics-grid">
                    <${MetricCard} label="Converting visitors" value=${num(sequences.data.converting_visitors)} />
                    <${MetricCard} label="Total visitors" value=${num(sequences.data.total_visitors)} />
                    <${MetricCard} label="Conversion rate" value=${pct(sequences.data.conversion_rate)} />
                    <${MetricCard} label="Total completions" value=${num(sequences.data.total_matches)} />
                  </div>
                `
              : html`<${Placeholder} state=${sequences} />`}
          </${Panel}>

          <${Panel} title="Flow" subtitle="Where visitors go next, or where they came from">
            <div class="inline-controls">
              <label>
                Page
                <input
                  type="text"
                  value=${flowPage}
                  onInput=${(e) => this.setState({ flowPage: e.target.value })}
                />
              </label>
              <label>
                Direction
                <select
                  value=${flowDirection}
                  onChange=${(e) => this.setState({ flowDirection: e.target.value })}
                >
                  <option value="forward">Next page</option>
                  <option value="backward">Previous page</option>
                </select>
              </label>
              <button onClick=${() => this.refresh()} disabled=${loading}>Apply</button>
            </div>
            ${flow && flow.kind === 'ok'
              ? (flow.data.length > 0
                  ? html`
                      <table class="data-table">
                        <thead>
                          <tr><th>Page</th><th class="numeric">Visitors</th><th class="numeric">Share</th></tr>
                        </thead>
                        <tbody>
                          ${flow.data.map(
                            (row) => html`<tr>
                              <td>${row.next_page}</td>
                              <td class="numeric">${num(row.visitors)}</td>
                              <td class="numeric">${pct(row.share)}</td>
                            </tr>`,
                          )}
                        </tbody>
                      </table>
                    `
                  : html`<div class="placeholder">No flow data for this page</div>`)
              : html`<${Placeholder} state=${flow} />`}
          </${Panel}>
        `}
      </div>
    `;
  }
}

/* ── App shell ─────────────────────────────────────────────────────────── */

class App extends Component {
  constructor() {
    super();
    this.state = { checked: false, authenticated: false, setupRequired: false, showSetup: false };
  }

  componentDidMount() {
    this.checkAuth();
  }

  async checkAuth() {
    try {
      const res = await fetch('/api/auth/status');
      if (res.ok) {
        const { authenticated, setup_required } = await res.json();
        this.setState({ checked: true, authenticated, setupRequired: setup_required });
        return;
      }
    } catch (e) {
      /* fall through to unauthenticated */
    }
    this.setState({ checked: true, authenticated: false, setupRequired: false });
  }

  async logout() {
    await fetch('/api/auth/logout', { method: 'POST' }).catch(() => {});
    this.setState({ authenticated: false });
  }

  render(_, { checked, authenticated, setupRequired, showSetup }) {
    if (!checked) return html`<div class="loading-screen">Loading…</div>`;
    // Before setup the API reports `authenticated: true` — reads are open, and
    // that is a deliberate deployment mode. It also meant the login form was
    // never shown, so there was no way to set an admin password from the
    // dashboard at all, despite the README promising a first-run prompt. The
    // setup form is now reachable from the banner below.
    if (!authenticated || showSetup) {
      return html`
        <${LoginForm}
          setupRequired=${setupRequired}
          onLogin=${() => this.setState({ showSetup: false }, () => this.checkAuth())}
          onCancel=${showSetup ? () => this.setState({ showSetup: false }) : null}
        />
      `;
    }
    return html`
      <${Dashboard}
        setupRequired=${setupRequired}
        onStartSetup=${() => this.setState({ showSetup: true })}
        onLogout=${setupRequired ? null : () => this.logout()}
        onAuthExpired=${() => this.setState({ authenticated: false })}
      />
    `;
  }
}

render(html`<${App} />`, document.getElementById('app'));
