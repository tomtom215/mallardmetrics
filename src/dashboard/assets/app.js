import { h, render, Component, createRef } from '/preact.js';
import htm from '/htm.js';
import {
  DASH, num, compact, tick, pct, fixed, duration, money, fmtDate, fmtDay, fmtMonth, fmtHour, hasTime,
  todayUTC, shiftDay, rangePreset, periodRange, validateRange, serializeFilters, readViewState,
  writeViewState, niceMax, axisTicks, retentionCellState, MAX_RANGE_DAYS,
} from '/format.js';
import { Api } from '/api.js';

const html = htm.bind(h);
const api = new Api();
// The browser check reads this to prove a cold load stayed within the server's
// concurrency cap and never had to fall back on retries.
window.__mallardApi = api.stats;

/* ── Preferences ───────────────────────────────────────────────────────── */

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

/* ── Loading, error and empty states ───────────────────────────────────── */

function Skel({ width, height, className = '' }) {
  const style = {};
  if (width) style.width = width;
  if (height) style.height = height;
  return html`<span class=${`skeleton ${className}`} style=${style} aria-hidden="true"></span>`;
}

function SkelRows({ rows = 5 }) {
  return html`
    <div class="skel-rows" role="status" aria-label="Loading">
      ${Array.from({ length: rows }, (_, i) => html`<${Skel} className="skel-row" width=${`${95 - (i % 3) * 12}%`} />`)}
    </div>
  `;
}

/**
 * Placeholder for a ten-row table, sized to the real thing (a header plus ten
 * rows), so the panels below do not jump down when a breakdown arrives.
 */
function SkelTable({ rows = 11 }) {
  return html`
    <div class="skel-rows skel-table" role="status" aria-label="Loading">
      ${Array.from({ length: rows }, (_, i) => html`<${Skel} className="skel-row" width=${`${96 - (i % 3) * 10}%`} />`)}
    </div>
  `;
}

function SkelCards({ count = 8 }) {
  return html`
    <div class="metrics-grid" role="status" aria-label="Loading">
      ${Array.from({ length: count }, () => html`<div class="metric-card"><${Skel} className="skel-value" /><${Skel} className="skel-label" /></div>`)}
    </div>
  `;
}

function SkelChart() {
  return html`<div class="chart-plot" role="status" aria-label="Loading"><${Skel} className="skel-chart" /></div>`;
}

/**
 * Catches a render crash caused by an unexpected payload shape, so one panel
 * degrades to a sentence instead of blanking the whole page.
 */
class Boundary extends Component {
  constructor() {
    super();
    this.state = { failed: false };
  }

  componentDidCatch(error) {
    console.error('dashboard panel failed to render', error);
    this.setState({ failed: true });
  }

  componentDidUpdate(prev) {
    if (this.state.failed && prev.resetKey !== this.props.resetKey) this.setState({ failed: false });
  }

  render({ children, fallback }, { failed }) {
    if (failed) {
      return (
        fallback ||
        html`<div class="placeholder placeholder-error">This panel could not display the server's response.</div>`
      );
    }
    return children;
  }
}

/** Calls `fn(data)` inside a component so a throw is caught by the nearest Boundary. */
function Render({ fn, data }) {
  return fn(data);
}

/** The explanation shown wherever the `behavioral` extension is missing. */
function BehavioralHelp({ detail, compactView }) {
  return html`
    <div class="placeholder placeholder-unavailable behavioral-help">
      <strong>Behavioral analytics is unavailable on this server</strong>
      <p>
        ${compactView
          ? html`This report needs DuckDB's${' '}<code>behavioral</code> extension, which is not loaded. See Overview.`
          : html`Visits, bounce rate, funnels, retention, sequences, flow and entry and exit pages depend on
              DuckDB's${' '}<code>behavioral</code> community extension, and it is not loaded. Page views,
              events, breakdowns, goals and revenue are unaffected.`}
      </p>
      ${!compactView &&
      html`
        <details>
          <summary>How to fix it</summary>
          <ol>
            <li>Look for a <code>behavioral</code> line in the server log; it names the reason.</li>
            <li>
              The extension is installed from the DuckDB community repository at start-up, so the
              server needs outbound HTTP access (port 80) to it. Offline hosts can place the extension file in
              the extension directory (<code>MALLARD_EXTENSION_DIR</code>, default${' '}
              <code>&lt;data dir&gt;/extensions</code>).
            </li>
            <li>
              That directory must be writable. On a read-only container, mount the data volume
              read-write.
            </li>
            <li>Restart the server, then reload this page.</li>
          </ol>
          ${detail && html`<p class="muted">The server said: ${detail}</p>`}
        </details>
      `}
    </div>
  `;
}

/**
 * The explanation shown wherever a report needs exact timestamps.
 *
 * `gdpr_mode` rounds every event to the top of its hour, which destroys the
 * order and spacing of events inside the hour that sessions, funnels, sequences
 * and flow are built from. That is a deliberate privacy setting, not a failure,
 * so it is presented as information in the accent colour, with no error styling.
 */
function RoundedHelp({ compactView }) {
  return html`
    <div class="placeholder placeholder-info rounded-help">
      <strong>This report needs session data that gdpr_mode rounds away</strong>
      <p>
        ${compactView
          ? 'Timestamps are rounded to the hour, so visits cannot be reconstructed.'
          : html`With <code>gdpr_mode</code> every timestamp is rounded to the hour. The server then
              cannot tell which events belong to one visit or in what order they happened, so it does
              not report visits, bounce rate, visit duration, funnels, sequences, flow or entry and
              exit pages. Visitors, pageviews, breakdowns, goals, revenue and weekly retention are
              unaffected.`}
      </p>
      ${!compactView &&
      html`<p class="muted">
        This is the privacy setting working as designed. To get these reports, turn off${' '}
        <code>gdpr_mode</code> (or <code>round_timestamps</code>) and restart; the data collected
        from then on keeps its exact times.
      </p>`}
    </div>
  `;
}

/** Why a report is unavailable: the extension is missing, or timestamps are rounded. */
function Unavailable({ reason, detail, compactView }) {
  return reason === 'rounded'
    ? html`<${RoundedHelp} compactView=${compactView} />`
    : html`<${BehavioralHelp} detail=${detail} compactView=${compactView} />`;
}

/**
 * Renders whichever of loading, unavailable, failed or loaded applies, then
 * hands loaded data to `render`. While a refresh is under way the previous
 * data stays on screen, dimmed, so the layout does not jump.
 */
function Async({ state, onRetry, skeleton, render: renderData, behavioral }) {
  const first = !state || (state.loading && state.kind !== 'ok');
  if (first) return skeleton || html`<${SkelRows} />`;
  if (state.kind === 'unavailable') {
    return html`<${Unavailable} reason=${state.reason} detail=${state.message} compactView=${behavioral === 'compact'} />`;
  }
  if (state.kind === 'network') {
    // A lost connection hits every panel at once. The banner at the top says so
    // and offers the one Retry; repeating the sentence and button in all twenty
    // panels buried the page in identical errors.
    return html`<div class="placeholder placeholder-offline" role="status">Waiting for the server…</div>`;
  }
  if (state.kind === 'timeout') {
    return html`
      <div class="placeholder placeholder-notice">
        <strong>${state.message}</strong>
        <p>
          The server stopped it at its time limit. Choose a shorter date range or remove a filter, then
          try again.
        </p>
        ${onRetry && html`<button type="button" class="btn-secondary" onClick=${onRetry}>Try again</button>`}
      </div>
    `;
  }
  if (state.kind === 'busy') {
    return html`
      <div class="placeholder placeholder-notice">
        <strong>${state.message}</strong>
        <p>It is answering other requests. Try again in a moment.</p>
        ${onRetry && html`<button type="button" class="btn-secondary" onClick=${onRetry}>Try again</button>`}
      </div>
    `;
  }
  if (state.kind === 'error') {
    return html`
      <div class="placeholder placeholder-error">
        <p>${state.message}</p>
        ${onRetry && html`<button type="button" class="btn-secondary" onClick=${onRetry}>Retry</button>`}
      </div>
    `;
  }
  if (state.kind !== 'ok') return skeleton || html`<${SkelRows} />`;
  return html`
    <div class=${state.loading ? 'is-stale' : ''} aria-busy=${state.loading ? 'true' : 'false'}>
      <${Boundary} resetKey=${state}><${Render} fn=${renderData} data=${state.data} /></${Boundary}>
    </div>
  `;
}

const Empty = ({ children }) => html`<div class="placeholder">${children}</div>`;

/** Calls `onVisible` once when it scrolls near the viewport, so off-screen panels load lazily. */
class Sentinel extends Component {
  componentDidMount() {
    if (!('IntersectionObserver' in window)) {
      this.props.onVisible();
      return;
    }
    this.io = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) {
          this.io.disconnect();
          this.props.onVisible();
        }
      },
      { rootMargin: '500px 0px' },
    );
    this.io.observe(this.base);
  }

  componentWillUnmount() {
    if (this.io) this.io.disconnect();
  }

  render() {
    return html`<span class="sentinel" aria-hidden="true"></span>`;
  }
}

/* ── Presentational components ─────────────────────────────────────────── */

function Panel({ id, title, subtitle, children, actions, focusTitle }) {
  const titleId = `${id}-title`;
  return html`
    <section class="panel" id=${id} aria-labelledby=${titleId}>
      <header class="panel-header">
        <div>
          <h2 id=${titleId} tabindex=${focusTitle ? '-1' : undefined}>${title}</h2>
          ${subtitle && html`<p class="panel-subtitle">${subtitle}</p>`}
        </div>
        ${actions}
      </header>
      ${children}
    </section>
  `;
}

/**
 * Relative change from `before` to `after`, as a rendered chip.
 *
 * Returns null when there is nothing honest to say: no comparison was
 * requested, a value is missing, or the baseline was zero. "Up from zero" has no
 * percentage, and showing one anyway is the kind of number people quote in a
 * meeting.
 */
function Delta({ before, after, invert }) {
  if (typeof before !== 'number' || typeof after !== 'number' || before === 0) return null;
  const change = (after - before) / before;
  if (!Number.isFinite(change)) return null;

  const rounded = Math.round(change * 1000) / 10;
  if (rounded === 0) {
    return html`<div class="metric-delta metric-delta-flat">±0%<span class="sr-only"> versus the comparison period</span></div>`;
  }
  // For bounce rate, down is the good direction, so the colour follows the
  // metric's meaning, not the arithmetic sign.
  const good = invert ? rounded < 0 : rounded > 0;
  return html`
    <div class=${`metric-delta ${good ? 'metric-delta-up' : 'metric-delta-down'}`}>
      <span aria-hidden="true">${rounded > 0 ? '▲' : '▼'}</span> ${Math.abs(rounded).toFixed(1)}%
      <span class="sr-only"> ${rounded > 0 ? 'higher' : 'lower'} than the comparison period</span>
    </div>
  `;
}

// What each headline figure means, in the server's own terms. The same words
// are the hover text on a card and the list under the cards, so a reader who
// never opens the list still gets the one-line version.
const HELP = {
  visitors: 'Distinct visitor IDs. IDs are replaced on a schedule the server sets (visitor_salt_rotation_days), so over a long range one person can be counted more than once.',
  pageviews: 'Page loads, including navigation inside single-page apps.',
  events: 'Every recorded event: pageviews plus custom events such as sign-ups.',
  viewsPerVisitor: 'Pageviews divided by unique visitors.',
  visits: 'A run of one visitor’s events with no gap longer than the session window (30 minutes by default). Only visits containing at least one pageview count.',
  bounce: 'The share of visits that were exactly one event, and that event a pageview. A visit with one pageview and a click or other custom event is not a bounce.',
  duration: 'Time from a visit’s first event to its last, averaged over visits. A one-event visit lasts 0 seconds, so a high bounce rate pulls this down.',
  viewsPerVisit: 'Pageviews divided by visits.',
};
const DEFINITIONS = [
  ['Unique visitors', HELP.visitors],
  ['Pageviews and events', 'A pageview is a page load. Total events add custom events (sign-ups, clicks you track, purchases) to that.'],
  ['Visit', HELP.visits + ' Custom events keep a visit alive, so a visitor who watches a video between two pageviews stays in one visit.'],
  ['Bounce rate', HELP.bounce],
  ['Visit duration', HELP.duration],
  ['Views per visit', 'Pageviews divided by visits. Views per visitor divides by people instead, so it is higher whenever people come back.'],
  ['Entry and exit pages', 'The entry page is the first pageview of a visit and the exit page is the last. Each counts visits, not pageviews.'],
  ['Filters', 'A filter keeps every visit that contains at least one matching event, and measures that visit in full. Filtering to one page therefore does not turn two-page visits into bounces.'],
  ['Date ranges', 'All dates are UTC. “Today” is midnight UTC until now. “7D” is the last seven days including today, and likewise 30D and 90D; 12M is the twelve months ending today.'],
];

function MetricCard({ label, value, full, hint, muted, delta, help }) {
  return html`
    <div class=${`metric-card${muted ? ' metric-muted' : ''}`} title=${help}>
      <dt class="metric-label">${label}</dt>
      <dd class="metric-value" title=${full}>${value}</dd>
      ${delta && html`<dd class="metric-extra">${delta}</dd>`}
      ${hint && html`<dd class="metric-hint">${hint}</dd>`}
    </div>
  `;
}

/** Label for one chart bucket, in the finest unit the series carries. */
function bucketLabel(date, spanDays, long) {
  if (hasTime(date)) return long ? `${fmtDate(date)} ${fmtHour(date)}` : fmtHour(date);
  if (long) return fmtDate(date);
  return spanDays > 150 ? fmtMonth(date) : fmtDay(date);
}

/**
 * A line chart with a hover readout.
 *
 * Hand-written SVG: the dashboard has no build step and no chart library, and
 * its content security policy allows scripts only from this origin. The SVG is
 * drawn in real pixels (measured from its container) rather than scaled from a
 * fixed viewBox, because scaling a 900-wide drawing onto a phone turned its
 * 11px labels into 5px ones. The same data is available as a table.
 */
class TimeseriesChart extends Component {
  constructor() {
    super();
    this.state = { hover: null, width: 0, table: false };
    this.plot = createRef();
  }

  componentDidMount() {
    this.measure();
    if ('ResizeObserver' in window && this.plot.current) {
      this.ro = new ResizeObserver(() => this.measure());
      this.ro.observe(this.plot.current);
    }
  }

  componentWillUnmount() {
    if (this.ro) this.ro.disconnect();
  }

  measure() {
    const el = this.plot.current;
    if (el && Math.round(el.clientWidth) !== this.state.width) {
      this.setState({ width: Math.round(el.clientWidth) });
    }
  }

  summary(data, spanDays) {
    let peak = data[0];
    let views = 0;
    for (const d of data) {
      views += d.pageviews;
      if (d.visitors > peak.visitors) peak = d;
    }
    const first = bucketLabel(data[0].date, spanDays, true);
    const last = bucketLabel(data[data.length - 1].date, spanDays, true);
    return (
      `Visitors and pageviews from ${first} to ${last}, ${num(data.length)} points. ` +
      `${num(views)} pageviews in total. Peak: ${num(peak.visitors)} visitors on ` +
      `${bucketLabel(peak.date, spanDays, true)}.`
    );
  }

  render({ data }, { hover, width, table }) {
    if (!data || data.length === 0) return html`<${Empty}>No data for this period</${Empty}>`;

    const spanDays = data.length;
    const W = Math.max(280, width || 640);
    const H = W < 520 ? 220 : 280;
    const PAD = { top: 12, right: 12, bottom: 28, left: W < 520 ? 40 : 48 };
    const cw = W - PAD.left - PAD.right;
    const ch = H - PAD.top - PAD.bottom;

    const top = niceMax(Math.max(1, ...data.map((d) => Math.max(d.visitors, d.pageviews))));
    const ticks = axisTicks(top);
    const xAt = (i) => (data.length > 1 ? PAD.left + (i * cw) / (data.length - 1) : PAD.left + cw / 2);
    const yAt = (v) => PAD.top + ch - (v / top) * ch;

    const line = (key) => data.map((d, i) => `${xAt(i).toFixed(1)},${yAt(d[key]).toFixed(1)}`).join(' ');
    const area = (key) => `${PAD.left},${PAD.top + ch} ${line(key)} ${xAt(data.length - 1).toFixed(1)},${PAD.top + ch}`;

    // About one label per 80px, and the last point always labelled.
    const labelStep = Math.max(1, Math.ceil(data.length / Math.max(2, Math.floor(cw / 80))));
    const labelled = (i) => i % labelStep === 0 && (data.length - 1 - i >= labelStep * 0.6 || i === data.length - 1);

    const pick = (index) => {
      const bounded = Math.min(data.length - 1, Math.max(0, index));
      if (bounded !== this.state.hover) this.setState({ hover: bounded });
    };
    const onPointer = (event) => {
      const rect = this.plot.current.getBoundingClientRect();
      const ratio = (event.clientX - rect.left - PAD.left) / (cw || 1);
      pick(Math.round(ratio * (data.length - 1)));
    };
    const onKey = (event) => {
      const at = hover == null ? data.length - 1 : hover;
      const moves = { ArrowLeft: at - 1, ArrowRight: at + 1, Home: 0, End: data.length - 1 };
      if (event.key in moves) {
        event.preventDefault();
        pick(moves[event.key]);
      } else if (event.key === 'Escape') {
        this.setState({ hover: null });
      }
    };

    const point = hover != null ? data[hover] : null;
    const summaryId = 'chart-summary';

    return html`
      <div class="chart">
        <div
          class="chart-plot"
          ref=${this.plot}
          tabindex="0"
          role="group"
          aria-label="Visitors and pageviews chart. Use the arrow keys to read each point."
          aria-describedby=${summaryId}
          onPointerMove=${onPointer}
          onPointerDown=${onPointer}
          onPointerLeave=${() => this.setState({ hover: null })}
          onKeyDown=${onKey}
          onBlur=${() => this.setState({ hover: null })}
        >
          <svg width=${W} height=${H} viewBox=${`0 0 ${W} ${H}`} class="chart-svg" aria-hidden="true" focusable="false">
            ${ticks.map(
              (v) => html`
                <line class="chart-gridline" x1=${PAD.left} y1=${yAt(v)} x2=${W - PAD.right} y2=${yAt(v)} />
                <text class="chart-tick" x=${PAD.left - 8} y=${yAt(v) + 4} text-anchor="end">${tick(v)}</text>
              `,
            )}
            <polygon class="chart-area" points=${area('pageviews')} />
            <polyline class="chart-line chart-line-pageviews" points=${line('pageviews')} />
            <polyline class="chart-line chart-line-visitors" points=${line('visitors')} />
            ${data.length === 1 &&
            html`
              <circle class="chart-dot chart-dot-visitors" cx=${xAt(0)} cy=${yAt(data[0].visitors)} r="4" />
              <circle class="chart-dot chart-dot-pageviews" cx=${xAt(0)} cy=${yAt(data[0].pageviews)} r="4" />
            `}
            ${data.map((d, i) =>
              labelled(i)
                ? html`<text class="chart-tick" x=${xAt(i)} y=${H - 8} text-anchor=${i === 0 && data.length > 1 ? 'start' : i === data.length - 1 && data.length > 1 ? 'end' : 'middle'}>${bucketLabel(d.date, spanDays, false)}</text>`
                : null,
            )}
            ${point &&
            html`
              <line class="chart-cursor" x1=${xAt(hover)} y1=${PAD.top} x2=${xAt(hover)} y2=${PAD.top + ch} />
              <circle class="chart-dot chart-dot-visitors" cx=${xAt(hover)} cy=${yAt(point.visitors)} r="4" />
              <circle class="chart-dot chart-dot-pageviews" cx=${xAt(hover)} cy=${yAt(point.pageviews)} r="4" />
            `}
          </svg>
        </div>
        <p id=${summaryId} class="sr-only">${this.summary(data, spanDays)}</p>

        <div class="chart-footer">
          <div class="chart-legend" aria-hidden="true">
            <span class="legend-item"><span class="legend-swatch swatch-visitors"></span> Visitors</span>
            <span class="legend-item"><span class="legend-swatch swatch-pageviews"></span> Pageviews</span>
          </div>
          <div class="chart-readout" aria-live="off">
            ${point
              ? html`<strong>${bucketLabel(point.date, spanDays, true)}</strong> · ${num(point.visitors)} visitors · ${num(point.pageviews)} pageviews`
              : html`<span class="muted">Hover, touch or focus the chart to read a point</span>`}
          </div>
          <button
            type="button"
            class="btn-link"
            aria-expanded=${table}
            onClick=${() => this.setState({ table: !table })}
          >
            ${table ? 'Hide table' : 'View as table'}
          </button>
        </div>

        ${table &&
        html`
          <div class="table-scroll chart-table" tabindex="0" role="region" aria-label="Chart data as a table">
            <table class="data-table">
              <caption class="sr-only">Visitors and pageviews per point in the chart</caption>
              <thead><tr><th scope="col">Date</th><th scope="col" class="numeric">Visitors</th><th scope="col" class="numeric">Pageviews</th></tr></thead>
              <tbody>
                ${data.map(
                  (d) => html`<tr><td>${bucketLabel(d.date, spanDays, true)}</td><td class="numeric">${num(d.visitors)}</td><td class="numeric">${num(d.pageviews)}</td></tr>`,
                )}
              </tbody>
            </table>
          </div>
        `}
      </div>
    `;
  }
}

/** A ranked list with an inline bar for relative magnitude. */
function BreakdownList({ rows, label = 'Value', columns, onFilter, caption, empty = 'No data for this period' }) {
  if (!rows || rows.length === 0) return html`<${Empty}>${empty}</${Empty}>`;
  const cols = columns || [['Visitors', 'visitors'], ['Pageviews', 'pageviews']];
  const max = Math.max(...rows.map((r) => r[cols[0][1]] || 0), 1);
  return html`
    <table class="data-table breakdown-table">
      ${caption && html`<caption class="sr-only">${caption}</caption>`}
      <thead>
        <tr>
          <th scope="col">${label}</th>
          ${cols.map(([name]) => html`<th scope="col" class="numeric">${name}</th>`)}
        </tr>
      </thead>
      <tbody>
        ${rows.map((row) => {
          const value = row.value == null || row.value === '' ? '(unknown)' : String(row.value);
          return html`
            <tr>
              <td class="bar-cell">
                <span class="bar" style=${{ width: `${Math.max(1, ((row[cols[0][1]] || 0) / max) * 100)}%` }} aria-hidden="true"></span>
                ${onFilter
                  ? html`<button type="button" class="bar-label bar-link" title=${`${value} - click to filter everything to this`} onClick=${() => onFilter(row.value == null ? '(unknown)' : row.value)}>${value}</button>`
                  : html`<span class="bar-label" title=${value}>${value}</span>`}
              </td>
              ${cols.map(([, key, fmt]) => html`<td class="numeric">${(fmt || num)(row[key])}</td>`)}
            </tr>
          `;
        })}
      </tbody>
    </table>
  `;
}

/** The active segment, with a chip per condition. */
function FilterBar({ filters, onRemove, onToggle, onClear }) {
  if (!filters || filters.length === 0) return null;
  return html`
    <div class="filter-bar" role="region" aria-label="Active filters">
      <span class="filter-bar-label">Filtered by</span>
      ${filters.map(
        (f, i) => html`
          <span class="filter-chip">
            <span class="filter-chip-dim">${FILTER_LABELS[f.dimension] || f.dimension}</span>
            <button
              type="button"
              class="filter-chip-op"
              title="Switch between is and is not"
              onClick=${() => onToggle(i)}
            >${f.negated ? 'is not' : 'is'}</button>
            <span class="filter-chip-value" title=${f.value}>${f.value}</span>
            <button
              type="button"
              class="filter-chip-remove"
              aria-label=${`Remove filter: ${FILTER_LABELS[f.dimension] || f.dimension} ${f.negated ? 'is not' : 'is'} ${f.value}`}
              onClick=${() => onRemove(i)}
            >×</button>
          </span>
        `,
      )}
      <button type="button" class="filter-clear" onClick=${onClear}>Clear all</button>
    </div>
  `;
}

/** Segmented control: a group of toggle buttons with a visible selected state. */
function Segmented({ label, options, value, onChange, pressedOverride }) {
  return html`
    <div class="segmented" role="group" aria-label=${label}>
      ${options.map((o) => {
        const on = pressedOverride ? pressedOverride(o.value) : o.value === value;
        return html`<button type="button" class=${`seg${on ? ' seg-on' : ''}`} aria-pressed=${on ? 'true' : 'false'} title=${o.title} onClick=${() => onChange(o.value)}>${o.label}</button>`;
      })}
    </div>
  `;
}

// Rows a breakdown asks for, in the order "Show more" steps through them. The
// server caps a request at 1000; 250 is already more than a person scans, and
// the list scrolls inside its panel instead of lengthening the page.
const LIMIT_STEPS = [10, 50, 250];

// Entry and exit pages are counted per visit, so their figure is a count of
// visits that began or ended there. The server reports it as `sessions`, and
// the other numbers are totals over those visits.
const SESSION_COLUMNS = {
  'entry-pages': [['Entrances', 'sessions'], ['Visitors', 'visitors']],
  'exit-pages': [['Exits', 'sessions'], ['Visitors', 'visitors']],
};
const TAB_HELP = {
  'entry-pages': 'Entrances: visits that began on this page, that is, whose first pageview was this one.',
  'exit-pages': 'Exits: visits that ended on this page, that is, whose last pageview was this one.',
};

/** Tabbed group of breakdowns sharing one panel. Fully keyboard operable. */
function BreakdownGroup({ group, active, data, limits, onTab, onFilter, onRetry, onLimit }) {
  const current = data[`bd:${active}`];
  const filterable = FILTERABLE_SLUGS.has(active);
  const tabLabel = group.tabs.find((t) => t.slug === active).label;
  const slugs = group.tabs.map((t) => t.slug);
  const limit = limits[active] || LIMIT_STEPS[0];
  const onKey = (event) => {
    const i = slugs.indexOf(active);
    const next = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: slugs.length - 1 }[event.key];
    if (next === undefined) return;
    event.preventDefault();
    const slug = slugs[(next + slugs.length) % slugs.length];
    onTab(group, slug);
    requestAnimationFrame(() => document.getElementById(`tab-${slug}`)?.focus());
  };
  const panelId = `bd-${group.id}`;
  const render = (rows) => {
    const sessionCols = SESSION_COLUMNS[active];
    // Older servers do not send `sessions`; fall back to the plain columns.
    const columns = sessionCols && rows.length > 0 && rows.every((r) => typeof r.sessions === 'number') ? sessionCols : undefined;
    const more = rows.length >= limit && limit < LIMIT_STEPS[LIMIT_STEPS.length - 1];
    const nextLimit = LIMIT_STEPS[LIMIT_STEPS.indexOf(limit) + 1];
    const scrolls = limit > LIMIT_STEPS[0] && rows.length > LIMIT_STEPS[0];
    return html`
      ${TAB_HELP[active] && html`<p class="table-note muted">${TAB_HELP[active]}</p>`}
      <div class=${scrolls ? 'breakdown-scroll' : ''} tabindex=${scrolls ? '0' : undefined} role=${scrolls ? 'region' : undefined} aria-label=${scrolls ? `${tabLabel}, ${rows.length} rows` : undefined}>
        <${BreakdownList} rows=${rows} label=${FILTER_LABELS[active] || tabLabel} columns=${columns} caption=${`${group.title}: ${tabLabel}`} onFilter=${filterable ? (value) => onFilter(active, value) : null} />
      </div>
      ${(more || limit > LIMIT_STEPS[0]) &&
      html`
        <div class="list-more">
          ${more && html`<button type="button" class="btn-link" onClick=${() => onLimit(active, nextLimit)}>Show more (up to ${nextLimit})</button>`}
          ${limit > LIMIT_STEPS[0] && html`<button type="button" class="btn-link" onClick=${() => onLimit(active, LIMIT_STEPS[0])}>Show top ${LIMIT_STEPS[0]}</button>`}
          <span class="muted" aria-live="polite">Showing ${num(rows.length)} ${rows.length === 1 ? 'row' : 'rows'}</span>
        </div>
      `}
    `;
  };
  return html`
    <${Panel} id=${panelId} title=${group.title}>
      <${Sentinel} onVisible=${() => onTab(group, active, true)} />
      <div class="tab-strip" role="tablist" aria-label=${`${group.title} breakdowns`} onKeyDown=${onKey}>
        ${group.tabs.map(
          (tab) => html`
            <button
              type="button"
              role="tab"
              id=${`tab-${tab.slug}`}
              aria-selected=${active === tab.slug ? 'true' : 'false'}
              aria-controls=${`${panelId}-body`}
              tabindex=${active === tab.slug ? '0' : '-1'}
              class=${`tab${active === tab.slug ? ' tab-active' : ''}`}
              onClick=${() => onTab(group, tab.slug)}
            >${tab.label}</button>
          `,
        )}
      </div>
      <div class="tab-panel" id=${`${panelId}-body`} role="tabpanel" aria-labelledby=${`tab-${active}`}>
        <${Async}
          state=${current}
          behavioral="compact"
          skeleton=${html`<${SkelTable} />`}
          onRetry=${() => onRetry(`bd:${active}`)}
          render=${render}
        />
      </div>
    </${Panel}>
  `;
}

function RealtimePanel({ state, onRetry, onVisible, filtered }) {
  return html`
    <${Panel} id="realtime" title="Right now" subtitle=${state && state.kind === 'ok' ? `Last ${state.data.window_minutes} minutes` : undefined}>
      <${Sentinel} onVisible=${onVisible} />
      <${Async}
        state=${state}
        onRetry=${onRetry}
        skeleton=${html`<${SkelTable} rows=${8} />`}
        render=${(data) => {
          const max = Math.max(...data.per_minute, 1);
          const total = data.per_minute.reduce((a, b) => a + b, 0);
          if (data.current_visitors === 0 && total === 0) {
            return html`
              <div class="placeholder realtime-empty">
                <strong>No one is on ${filtered ? 'the filtered part of ' : ''}the site right now</strong>
                <p>
                  No pageviews in the last ${data.window_minutes} minutes${filtered ? ' that match the active filters' : ''}.
                  This panel checks again every ${REALTIME_REFRESH_MS / 1000} seconds${state.offline ? html` <span class="offline-tag">(offline, retrying)</span>` : ''}.
                </p>
              </div>
            `;
          }
          return html`
            <div class="realtime">
              <div class="realtime-headline">
                <div class="realtime-count">${num(data.current_visitors)}</div>
                <div class="realtime-label">current visitors${state.offline ? html` <span class="offline-tag">offline, retrying</span>` : ''}</div>
              </div>
              <div class="realtime-spark" role="img" aria-label=${`${num(total)} pageviews in the last ${data.window_minutes} minutes, one bar per minute`}>
                ${data.per_minute.map((value, i) => html`<span class="spark-bar" title=${`${num(value)} ${i === data.per_minute.length - 1 ? 'in the current minute' : `${data.per_minute.length - 1 - i} min ago`}`} style=${{ height: `${Math.max(4, (value / max) * 100)}%` }}></span>`)}
              </div>
            </div>
            <div class="realtime-lists">
              <div>
                <h3>Top pages</h3>
                <${BreakdownList} rows=${data.top_pages} label="Page" columns=${[['Visitors', 'visitors']]} caption="Top pages right now" empty="No pageviews in this window" />
              </div>
              <div>
                <h3>Top sources</h3>
                <${BreakdownList} rows=${data.top_sources} label="Source" columns=${[['Visitors', 'visitors']]} caption="Top sources right now" empty="No sources in this window" />
              </div>
            </div>
          `;
        }}
      />
    </${Panel}>
  `;
}

function FunnelChart({ steps, specs }) {
  if (steps.length === 0) return html`<${Empty}>No funnel data</${Empty}>`;
  const entered = steps[0].visitors;
  return html`
    <ol class="funnel">
      ${steps.map(
        (step, i) => html`
          <li class="funnel-step">
            <div class="funnel-meta">
              <span class="funnel-step-name">Step ${step.step}${specs[i] ? html` <code title=${specs[i]}>${specs[i]}</code>` : ''}</span>
              <span class="funnel-step-figures">
                ${num(step.visitors)} visitors · ${pct(step.conversion_rate)} of entrants
                ${step.dropped_off > 0 ? html`<span class="funnel-drop">−${num(step.dropped_off)} dropped</span>` : null}
              </span>
            </div>
            <div class="funnel-track" aria-hidden="true">
              <div class="funnel-fill" style=${{ width: `${entered > 0 ? (step.visitors / entered) * 100 : 0}%` }}></div>
            </div>
          </li>
        `,
      )}
    </ol>
  `;
}

// Weeks requested from the retention report, and so how long a visitor ID has
// to survive for the later columns to mean anything: week N needs 7 x N days.
const RETENTION_WEEKS = 4;
const RETENTION_MIN_ROTATION_DAYS = (RETENTION_WEEKS - 1) * 7;

/**
 * Weekly cohorts.
 *
 * A cell is only a percentage when its week is over. Weeks that have not
 * finished (`complete_weeks`) are not yet a measurement, and printing them as 0%
 * told people nobody came back when the week had simply not happened. A cohort
 * whose first week began before the report's start (`partial_first_week`) is
 * missing the visitors first seen earlier that week, so it is labelled.
 */
function RetentionGrid({ data }) {
  const { cohorts, identity_supports_cohorts: supported, visitor_salt_rotation_days: rotation } = data;
  // An informational note, not an error: nothing is broken, the server is set up
  // to forget visitors faster than this report needs it to remember them.
  const notice =
    supported === false &&
    html`
      <div class="notice notice-info" role="note">
        <strong>Weeks after the first cannot be measured with the current setting.</strong>
        Retention follows the same visitor from one week to the next, but this server replaces visitor
        IDs${Number.isFinite(rotation) ? html` every ${rotation} day${rotation === 1 ? '' : 's'}` : ' sooner than that'}, so a returning visitor looks like a new one. To measure ${RETENTION_WEEKS}-week
        cohorts, set <code>visitor_salt_rotation_days</code> (environment variable${' '}
        <code>MALLARD_VISITOR_SALT_ROTATION_DAYS</code>) to at least ${RETENTION_MIN_ROTATION_DAYS} and
        restart. It applies to events collected from then on, and a longer setting lets a visitor be
        recognised for longer.
      </div>
    `;
  if (!cohorts || cohorts.length === 0) {
    return html`${notice}<${Empty}>No cohorts in this period</${Empty}>`;
  }
  const width = Math.max(...cohorts.map((c) => c.retained.length));
  const stateOf = (row, i) => retentionCellState(row, i, supported);
  const anyPending = cohorts.some((row) => row.retention_rates.some((_, i) => stateOf(row, i) === 'pending'));
  const anyPartial = cohorts.some((row) => row.partial_first_week);
  return html`
    ${notice}
    <div class="table-scroll retention-scroll" tabindex="0" role="region" aria-label="Retention cohort table. Scroll sideways to see later weeks.">
      <table class="data-table retention-table">
        <caption class="sr-only">Share of each weekly cohort that returned in each following week</caption>
        <thead>
          <tr>
            <th scope="col">Cohort (week of)</th>
            <th scope="col" class="numeric">Size</th>
            ${Array.from({ length: width }, (_, i) => html`<th scope="col" class="numeric" title=${i === 0 ? 'The cohort week itself' : `${i} week${i === 1 ? '' : 's'} later`}>${i === 0 ? 'Week 0' : `Week ${i}`}</th>`)}
          </tr>
        </thead>
        <tbody>
          ${cohorts.map(
            (row) => html`
              <tr>
                <th scope="row">
                  ${fmtDate(row.cohort_date)}
                  ${row.partial_first_week && html` <span class="tag" title="This cohort's first week began before the report's start date, so visitors first seen earlier that week are not counted.">partial week</span>`}
                </th>
                <td class="numeric">${num(row.cohort_size)}</td>
                ${row.retention_rates.map((rate, i) =>
                  stateOf(row, i) === 'value'
                    ? html`<td class="numeric retention-cell" style=${{ '--r': Math.min(1, Math.max(0, rate)) }} title=${`${num(row.retained[i])} of ${num(row.cohort_size)}`}>${pct(rate, 0)}</td>`
                    : html`<td class="numeric retention-pending" title=${stateOf(row, i) === 'unmeasurable' ? 'Not measurable: visitor IDs do not last this long' : 'This week has not finished yet'}><span aria-hidden="true">–</span><span class="sr-only">${stateOf(row, i) === 'unmeasurable' ? 'not measurable' : 'not finished yet'}</span></td>`,
                )}
              </tr>
            `,
          )}
        </tbody>
      </table>
    </div>
    ${(anyPending || anyPartial) &&
    html`<p class="table-note muted">
      ${anyPending ? html`A dash means that week has not finished yet, so there is nothing to compare. ` : ''}
      ${anyPartial ? html`A partial-week cohort began before the start of the report, so it leaves out visitors first seen earlier that week.` : ''}
    </p>`}
  `;
}

function MoneyTable({ rows, label }) {
  return html`
    <div class="table-scroll">
      <table class="data-table">
        <thead><tr><th scope="col">${label}</th><th scope="col" class="numeric">Revenue</th><th scope="col" class="numeric">Orders</th></tr></thead>
        <tbody>
          ${rows.map((row) => html`<tr>
            <td class="cell-text" title=${row.value}>${row.value}</td>
            <td class="numeric">${money(row.total, row.currency)}</td>
            <td class="numeric">${num(row.transactions)}</td>
          </tr>`)}
        </tbody>
      </table>
    </div>
  `;
}

function RevenuePanel({ state, onRetry, onVisible }) {
  const report = state && state.kind === 'ok' ? state.data : null;
  const none = report && Array.isArray(report.by_currency) && report.by_currency.length === 0;
  return html`
    <${Panel} id="revenue" title="Revenue" subtitle=${none ? 'Send a revenue amount with mallard() to populate this' : 'Totals are reported per currency and never summed across them'}>
      <${Sentinel} onVisible=${onVisible} />
      <${Async}
        state=${state}
        onRetry=${onRetry}
        skeleton=${html`<${SkelCards} count=${2} />`}
        render=${(r) =>
          r.by_currency.length === 0
            ? html`<${Empty}>No revenue recorded in this period</${Empty}>`
            : html`
                <dl class="metrics-grid">
                  ${r.by_currency.map((row) => html`<${MetricCard} label=${`${row.currency} revenue`} value=${money(row.total, row.currency)} hint=${`${num(row.transactions)} orders · ${money(row.average_order_value, row.currency)} average`} />`)}
                </dl>
                <div class="two-column">
                  <div><h3>By event</h3><${MoneyTable} rows=${r.by_event} label="Event" /></div>
                  <div><h3>By page</h3><${MoneyTable} rows=${r.by_page} label="Page" /></div>
                </div>
              `}
      />
    </${Panel}>
  `;
}

/** Goals, plus a drill-down into the custom properties attached to them. */
class GoalsPanel extends Component {
  constructor() {
    super();
    this.state = { propertyKey: '', values: null };
    this.token = 0;
  }

  async loadValues(key) {
    const token = ++this.token;
    if (!key) {
      this.setState({ propertyKey: '', values: null });
      return;
    }
    this.setState({ propertyKey: key, values: { loading: true } });
    const values = await api.get(`/api/stats/property-values?${this.props.query}&key=${encodeURIComponent(key)}`);
    if (token === this.token) this.setState({ values });
  }

  componentDidUpdate(prev) {
    // A new range or segment makes the old drill-down stale.
    if (prev.query !== this.props.query && this.state.propertyKey) this.loadValues(this.state.propertyKey);
  }

  render({ goals, propertyKeys, onRetry, onVisible }, { propertyKey, values }) {
    return html`
      <${Panel} id="goals" title="Goals & custom events" subtitle="Every event other than a pageview, with the share of visitors who triggered it">
        <${Sentinel} onVisible=${onVisible} />
        <${Async}
          state=${goals}
          onRetry=${() => onRetry('goals')}
          render=${(rows) =>
            rows.length === 0
              ? html`<${Empty}>No custom events in this period</${Empty}>`
              : html`
                  <div class="table-scroll">
                    <table class="data-table">
                      <thead><tr><th scope="col">Event</th><th scope="col" class="numeric">Visitors</th><th scope="col" class="numeric">Events</th><th scope="col" class="numeric">Conversion</th></tr></thead>
                      <tbody>
                        ${rows.map((row) => html`<tr>
                          <td class="cell-text" title=${row.name}>${row.name}</td>
                          <td class="numeric">${num(row.visitors)}</td>
                          <td class="numeric">${num(row.events)}</td>
                          <td class="numeric">${pct(row.conversion_rate)}</td>
                        </tr>`)}
                      </tbody>
                    </table>
                  </div>
                `}
        />
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
                ${values &&
                html`<${Async} state=${values} onRetry=${() => this.loadValues(propertyKey)} render=${(rows) => html`<${BreakdownList} rows=${rows} label=${propertyKey} columns=${[['Events', 'events']]} />`} />`}
              </div>
            `
          : null}
      </${Panel}>
    `;
  }
}

/* ── Copy and install ──────────────────────────────────────────────────── */

/** Copy text, with a selection fallback for plain-HTTP origins where the Clipboard API is blocked. */
async function copyText(text, fallbackEl) {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch (e) {
    /* fall through to the selection fallback */
  }
  try {
    if (fallbackEl) {
      const range = document.createRange();
      range.selectNodeContents(fallbackEl);
      const sel = window.getSelection();
      sel.removeAllRanges();
      sel.addRange(range);
      return document.execCommand('copy');
    }
  } catch (e) {
    /* nothing more to try */
  }
  return false;
}

class CopyButton extends Component {
  constructor() {
    super();
    this.state = { status: '' };
  }

  componentWillUnmount() {
    clearTimeout(this.timer);
  }

  async copy() {
    const ok = await copyText(this.props.text, this.props.source?.current);
    this.setState({ status: ok ? 'Copied to clipboard' : 'Copy failed: select the text and copy it manually' });
    clearTimeout(this.timer);
    this.timer = setTimeout(() => this.setState({ status: '' }), 2500);
  }

  render({ label = 'Copy' }, { status }) {
    return html`
      <button type="button" class="btn-secondary" onClick=${() => this.copy()}>${status === 'Copied to clipboard' ? 'Copied' : label}</button>
      <span class="sr-only" role="status">${status}</span>
      ${status && status !== 'Copied to clipboard' && html`<span class="copy-status" aria-hidden="true">${status}</span>`}
    `;
  }
}

function snippetFor(site) {
  return `<script defer data-domain="${site || 'yourdomain.com'}"\n  src="${location.origin}/mallard.js"></script>`;
}

function Snippet({ site }) {
  const code = snippetFor(site);
  const ref = createRef();
  return html`
    <div class="snippet">
      <pre class="snippet-code" tabindex="0" aria-label="Tracking snippet"><code ref=${ref}>${code}</code></pre>
      <${CopyButton} text=${code} source=${ref} label="Copy snippet" />
    </div>
  `;
}

function InstallPanel({ site, onClose }) {
  return html`
    <${ToolPanel} id="install" title="Install the tracking snippet" subtitle=${`Add this to every page of ${site || 'your site'}, before the closing </head> tag.`} onClose=${onClose}>
      <${Snippet} site=${site} />
      <p class="muted">
        Pageviews are recorded automatically, including single-page-app navigation. Optional
        attributes such as <code>data-exclude</code>, <code>data-outbound</code> and${' '}
        <code>data-hash</code> are described in the tracking script's header. Record a custom event
        or revenue from your own code with <code>mallard('signup')</code> or${' '}
        <code>mallard('purchase', { revenue: 49, currency: 'USD' })</code>.
      </p>
    </${ToolPanel}>
  `;
}

function Welcome({ siteDraft, onDraft, onUse }) {
  return html`
    <${Panel} id="welcome" title="Welcome to Mallard Metrics" subtitle="No site has reported an event yet.">
      <p>Add the tracking snippet to your site. The site appears in the selector as soon as its first pageview arrives.</p>
      <form class="inline-controls" onSubmit=${(e) => { e.preventDefault(); onUse(); }}>
        <label for="welcome-site">Your site's domain<input id="welcome-site" type="text" value=${siteDraft} placeholder="example.com" autocomplete="off" onInput=${(e) => onDraft(e.target.value.trim())} /></label>
        <button type="submit" class="btn-secondary">Look up this site</button>
      </form>
      <${Snippet} site=${siteDraft} />
    </${Panel}>
  `;
}

/** A panel opened from the header: takes focus when it appears, closes with Escape. */
class ToolPanel extends Component {
  componentDidMount() {
    const heading = this.base.querySelector('h2');
    if (heading) heading.focus();
    if (this.base.scrollIntoView) this.base.scrollIntoView({ block: 'nearest' });
  }

  render({ id, title, subtitle, onClose, children }) {
    return html`
      <div onKeyDown=${(e) => e.key === 'Escape' && onClose()}>
        <${Panel} id=${id} title=${title} subtitle=${subtitle} focusTitle=${true} actions=${html`<button type="button" class="btn-secondary" onClick=${onClose}>Close</button>`}>
          ${children}
        </${Panel}>
      </div>
    `;
  }
}

/* ── Authentication ────────────────────────────────────────────────────── */

/**
 * API key management. The plaintext key is returned once and never again, so it
 * stays on screen until dismissed rather than vanishing on the next render.
 */
/** Snapshot the data on the server. Admin only: a 403 or 409 is shown in the server's own words. */
class BackupSection extends Component {
  constructor() {
    super();
    this.state = { busy: false, result: null };
  }

  async run() {
    this.setState({ busy: true, result: null });
    const res = await api.send('POST', '/api/admin/backup');
    if (res.kind === 'unauthorized') return this.props.onAuthExpired();
    this.setState({
      busy: false,
      result: res.kind === 'ok'
        ? { ok: true, ...res.data }
        : { ok: false, message: res.status === 403 ? 'This needs an admin key or an admin session.' : res.message },
    });
  }

  render(_, { busy, result }) {
    return html`
      <section aria-labelledby="backup-title">
        <h3 class="subhead" id="backup-title">Backup</h3>
        <p class="muted">Snapshot the database and event files into the server's <code>backups</code> folder while it keeps running. Restoring is done on the server.</p>
        <div class="inline-controls">
          <button type="button" class="btn-secondary" disabled=${busy} onClick=${() => this.run()}>${busy ? 'Creating backup…' : 'Create backup'}</button>
        </div>
        <div role="status">
          ${result && result.ok && html`<p>Backup <strong>${result.name}</strong> created: <code>${result.path}</code> (${num(result.hot_rows)} recent events, ${num(result.parquet_files)} archive files).</p>`}
          ${result && !result.ok && html`<div class="placeholder placeholder-error">${result.message}</div>`}
        </div>
      </section>
    `;
  }
}

class KeysPanel extends Component {
  constructor() {
    super();
    this.state = { keys: null, name: '', scope: 'ReadOnly', created: null, error: null, busy: false };
    this.keyRef = createRef();
  }

  componentDidMount() {
    this.load();
  }

  async load() {
    const result = await api.get('/api/keys');
    if (result.kind === 'unauthorized') return this.props.onAuthExpired();
    if (result.kind === 'ok') this.setState({ keys: result.data, error: null });
    else this.setState({ keys: this.state.keys || [], error: result.message });
  }

  async create(event) {
    event.preventDefault();
    const name = this.state.name.trim();
    if (!name) return;
    this.setState({ busy: true, error: null });
    const res = await api.send('POST', '/api/keys', { name, scope: this.state.scope });
    if (res.kind === 'unauthorized') return this.props.onAuthExpired();
    if (res.kind === 'ok') {
      this.setState({ created: res.data, name: '', busy: false });
      await this.load();
    } else {
      this.setState({ error: res.message, busy: false });
    }
  }

  async revoke(keyHash, name) {
    // A revoked key stops working immediately and cannot be restored, so the one
    // destructive action in this panel asks first.
    if (!window.confirm(`Revoke the API key "${name}"? Anything using it stops working immediately.`)) return;
    this.setState({ busy: true, error: null });
    const res = await api.send('DELETE', `/api/keys/${encodeURIComponent(keyHash)}`);
    if (res.kind === 'unauthorized') return this.props.onAuthExpired();
    this.setState({ busy: false, error: res.kind === 'ok' ? null : res.message });
    await this.load();
  }

  render({ onClose }, { keys, name, scope, created, error, busy }) {
    const live = (keys || []).filter((k) => !k.revoked);
    return html`
      <${ToolPanel} id="keys" title="API keys" onClose=${onClose}
        subtitle="Programmatic access to the stats endpoints. Keys are stored as SHA-256 hashes; the key itself is shown once.">
        ${created && html`
          <div class="banner banner-key" role="alert">
            <strong>Copy this key now — it is not shown again.</strong>
            <code class="key-value" ref=${this.keyRef}>${created.key}</code>
            <${CopyButton} text=${created.key} source=${this.keyRef} label="Copy key" />
            <button type="button" class="btn-secondary" onClick=${() => this.setState({ created: null })}>Done</button>
          </div>
        `}
        <form class="inline-controls" onSubmit=${(e) => this.create(e)}>
          <label>Name
            <input type="text" value=${name} maxlength="128" placeholder="grafana-scraper" autocomplete="off" onInput=${(e) => this.setState({ name: e.target.value })} required />
          </label>
          <label>Scope
            <select value=${scope} onChange=${(e) => this.setState({ scope: e.target.value })}>
              <option value="ReadOnly">Read only — stats endpoints</option>
              <option value="Admin">Admin — also key management and erasure</option>
            </select>
          </label>
          <button type="submit" disabled=${busy || !name.trim()}>Create key</button>
        </form>
        ${error && html`<div class="placeholder placeholder-error" role="alert">${error}</div>`}
        ${keys === null
          ? html`<${SkelRows} rows=${2} />`
          : live.length === 0
            ? html`<${Empty}>No API keys yet</${Empty}>`
            : html`
                <div class="table-scroll">
                  <table class="data-table">
                    <thead><tr><th scope="col">Name</th><th scope="col">Scope</th><th scope="col">Created</th><th scope="col">Fingerprint</th><th scope="col"><span class="sr-only">Actions</span></th></tr></thead>
                    <tbody>
                      ${live.map((key) => html`
                        <tr>
                          <td class="cell-text" title=${key.name}>${key.name}</td>
                          <td>${key.scope === 'Admin' ? 'Admin' : 'Read only'}</td>
                          <td>${fmtDate(key.created_at)}</td>
                          <td><code>${String(key.key_hash).slice(0, 12)}…</code></td>
                          <td><button type="button" class="btn-secondary" disabled=${busy} onClick=${() => this.revoke(key.key_hash, key.name)}>Revoke<span class="sr-only"> ${key.name}</span></button></td>
                        </tr>
                      `)}
                    </tbody>
                  </table>
                </div>
              `}
        <${BackupSection} onAuthExpired=${this.props.onAuthExpired} />
      </${ToolPanel}>
    `;
  }
}

class LoginForm extends Component {
  constructor() {
    super();
    this.state = { error: null, busy: false };
    this.input = createRef();
  }

  componentDidMount() {
    if (this.input.current) this.input.current.focus();
  }

  async submit(event) {
    event.preventDefault();
    const password = event.target.elements.password.value;
    this.setState({ busy: true, error: null });
    const res = await api.send('POST', this.props.setupRequired ? '/api/auth/setup' : '/api/auth/login', { password });
    if (res.kind === 'ok') {
      this.props.onLogin();
      return;
    }
    this.setState({ error: res.kind === 'unauthorized' ? 'That password is not correct.' : res.message, busy: false });
  }

  render({ setupRequired, onCancel }, { error, busy }) {
    return html`
      <main class="auth-overlay">
        <form class="auth-card" onSubmit=${(e) => this.submit(e)}>
          <h1><img src="/favicon.svg" alt="" width="28" height="28" /> Mallard Metrics</h1>
          <h2>${setupRequired ? 'Set admin password' : 'Sign in'}</h2>
          <p class="auth-hint" id="auth-hint">
            ${setupRequired ? 'No password is set yet. Choose one to protect this dashboard. At least 12 characters.' : 'Enter your admin password to continue.'}
          </p>
          <label class="sr-only" for="password">Admin password</label>
          <input id="password" ref=${this.input} type="password" name="password" placeholder="Password" minlength=${setupRequired ? 12 : 1}
            autocomplete=${setupRequired ? 'new-password' : 'current-password'} aria-describedby="auth-hint" required />
          ${error && html`<p class="auth-error" role="alert">${error}</p>`}
          <button type="submit" disabled=${busy}>${busy ? 'Working…' : setupRequired ? 'Set password' : 'Sign in'}</button>
          ${onCancel && html`<button type="button" class="btn-secondary" onClick=${onCancel}>Not now</button>`}
        </form>
      </main>
    `;
  }
}

/* ── Dashboard ─────────────────────────────────────────────────────────── */

const BREAKDOWN_GROUPS = [
  { id: 'pages', title: 'Pages', tabs: [
    { slug: 'pages', label: 'Top pages' }, { slug: 'entry-pages', label: 'Entry pages' }, { slug: 'exit-pages', label: 'Exit pages' } ] },
  { id: 'acquisition', title: 'Acquisition', tabs: [
    { slug: 'sources', label: 'Sources' }, { slug: 'referrers', label: 'Referrers' }, { slug: 'utm-sources', label: 'UTM source' },
    { slug: 'utm-mediums', label: 'UTM medium' }, { slug: 'utm-campaigns', label: 'UTM campaign' },
    { slug: 'utm-contents', label: 'UTM content' }, { slug: 'utm-terms', label: 'UTM term' } ] },
  { id: 'locations', title: 'Locations', tabs: [
    { slug: 'countries', label: 'Countries' }, { slug: 'regions', label: 'Regions' }, { slug: 'cities', label: 'Cities' } ] },
  { id: 'devices', title: 'Devices', tabs: [
    { slug: 'browsers', label: 'Browsers' }, { slug: 'browser-versions', label: 'Browser versions' },
    { slug: 'os', label: 'Operating systems' }, { slug: 'os-versions', label: 'OS versions' },
    { slug: 'devices', label: 'Device types' }, { slug: 'screen-sizes', label: 'Screen widths' } ] },
];

const ALL_BREAKDOWN_SLUGS = BREAKDOWN_GROUPS.flatMap((g) => g.tabs.map((t) => t.slug));

// Filter chips read as a sentence ("Page is /pricing"), so they need singular
// labels, not the tab headings.
const FILTER_LABELS = {
  pages: 'Page', 'entry-pages': 'Entry page', 'exit-pages': 'Exit page', referrers: 'Referrer', sources: 'Source',
  countries: 'Country', regions: 'Region', cities: 'City', browsers: 'Browser', 'browser-versions': 'Browser version',
  os: 'OS', 'os-versions': 'OS version', devices: 'Device', 'screen-sizes': 'Screen width', 'utm-sources': 'UTM source',
  'utm-mediums': 'UTM medium', 'utm-campaigns': 'UTM campaign', 'utm-contents': 'UTM content', 'utm-terms': 'UTM term',
  events: 'Event',
};

// Entry and exit pages are derived from a whole session rather than stored on
// an event, so the server has no per-event predicate for them and answers 400.
const FILTERABLE_SLUGS = new Set(ALL_BREAKDOWN_SLUGS.filter((slug) => slug !== 'entry-pages' && slug !== 'exit-pages'));

/**
 * Why session data is missing, from what the server says: 'rounded'
 * (gdpr_mode / round_timestamps), 'extension' (the behavioral extension is not
 * loaded) or 'unknown' (an older server that names no cause).
 */
const reasonFromMain = (value) => (value === 'timestamps_rounded' ? 'rounded' : value === 'extension_missing' ? 'extension' : 'unknown');
const reasonFromCode = (code) => (code === 'session_data_unavailable' ? 'rounded' : code === 'behavioral_unavailable' ? 'extension' : 'unknown');

// Reports computed from sessions or ordered events. `retention` is separate: it
// needs the extension but not exact timestamps, so gdpr_mode leaves it working.
const BEHAVIORAL_KEYS = new Set(['funnel', 'retention', 'sequences', 'flow']);
const NEEDS_EXACT_TIME = (key) => key === 'bd:entry-pages' || key === 'bd:exit-pages' || (BEHAVIORAL_KEYS.has(key) && key !== 'retention');
const REALTIME_REFRESH_MS = 15000;
const RANGE_OPTIONS = [
  { value: 'day', label: 'Today', title: 'Today so far: midnight UTC until now' },
  { value: '7d', label: '7D', title: 'The last 7 days, including today' },
  { value: '30d', label: '30D', title: 'The last 30 days, including today' },
  { value: '90d', label: '90D', title: 'The last 90 days, including today' },
  { value: '12mo', label: '12M', title: 'The last 12 months, ending today' },
  { value: 'custom', label: 'Custom', title: 'Pick exact dates' },
];
const COMPARE_OPTIONS = [
  { value: 'none', label: 'Off', title: 'Do not compare' },
  { value: 'previous_period', label: 'Previous period', title: 'Compare with the equally long period immediately before this one' },
  { value: 'year_over_year', label: 'Last year', title: 'Compare with the same dates 365 days earlier, the same length' },
];

class Dashboard extends Component {
  constructor(props) {
    super(props);
    const prefs = loadPrefs();
    // The address bar wins over saved preferences: a shared link must open
    // exactly the view it was copied from.
    const url = readViewState(location.search, FILTERABLE_SLUGS);
    const period = url.period || prefs.period || '30d';
    this.state = {
      sites: null,
      siteEvents: {},
      // The address bar names a site; otherwise the busiest one is chosen once
      // the list arrives (a remembered site would hide that default).
      siteId: url.siteId || '',
      siteDraft: '',
      period: period === 'today' ? 'day' : period,
      startDate: url.startDate || prefs.startDate || '',
      endDate: url.endDate || prefs.endDate || '',
      compare: url.compare || prefs.compare || 'none',
      filters: url.filters || [],
      theme: prefs.theme || 'auto',
      res: {},
      reason: null,
      limits: {},
      tabs: Object.fromEntries(BREAKDOWN_GROUPS.map((g) => [g.id, prefs.tabs?.[g.id] && g.tabs.some((t) => t.slug === prefs.tabs[g.id]) ? prefs.tabs[g.id] : g.tabs[0].slug])),
      funnelSteps: prefs.funnelSteps || 'page:/,page:/pricing,event:signup',
      funnelModes: prefs.funnelModes || '',
      funnelError: '',
      sequenceSteps: prefs.sequenceSteps || 'page:/,event:signup',
      flowPage: prefs.flowPage || '/',
      flowDirection: prefs.flowDirection || 'forward',
      rangeOpen: false,
      draftStart: '',
      draftEnd: '',
      rangeError: '',
      panel: null,
      notice: null,
      updatedAt: null,
    };
    this.res = {};
    this.gen = 0;
    this.abort = null;
    this.wanted = new Set();
    this.fetched = new Set();
    this.behavioral = null;
    this.reason = null;
    this.mainWaiters = [];
    this.onPop = () => this.restoreFromUrl();
    this.onVisibility = () => {
      if (!document.hidden) this.pollRealtime();
    };
  }

  async componentDidMount() {
    this.applyTheme();
    window.addEventListener('popstate', this.onPop);
    document.addEventListener('visibilitychange', this.onVisibility);
    await this.loadSites();
    if (this.state.siteId) this.refresh({ push: false });
    this.timer = setInterval(() => this.pollRealtime(), REALTIME_REFRESH_MS);
  }

  componentWillUnmount() {
    clearInterval(this.timer);
    window.removeEventListener('popstate', this.onPop);
    document.removeEventListener('visibilitychange', this.onVisibility);
    if (this.abort) this.abort.abort();
  }

  applyTheme() {
    const { theme } = this.state;
    if (theme === 'auto') delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = theme;
  }

  persist() {
    const { siteId, period, startDate, endDate, theme, funnelSteps, funnelModes, sequenceSteps, flowPage, flowDirection, compare, tabs } = this.state;
    savePrefs({ siteId, period, startDate, endDate, theme, funnelSteps, funnelModes, sequenceSteps, flowPage, flowDirection, compare, tabs });
  }

  /** The shared query string for the selected site, range and segment. */
  query() {
    const { siteId, period, startDate, endDate, filters } = this.state;
    const parts = [`site_id=${encodeURIComponent(siteId)}`];
    if (period === 'custom' && startDate && endDate) parts.push(`start_date=${startDate}`, `end_date=${endDate}`);
    else parts.push(`period=${period}`);
    if (filters.length > 0) parts.push(`filters=${encodeURIComponent(serializeFilters(filters))}`);
    return parts.join('&');
  }

  /** Realtime takes the segment but not the date range: its window is its own. */
  realtimeQuery() {
    const { siteId, filters } = this.state;
    const parts = [`site_id=${encodeURIComponent(siteId)}`];
    if (filters.length > 0) parts.push(`filters=${encodeURIComponent(serializeFilters(filters))}`);
    return parts.join('&');
  }

  urlFor(key) {
    const s = this.state;
    const q = this.query();
    switch (key) {
      case 'main': return `/api/stats/main?${q}${s.compare === 'none' ? '' : `&compare=${s.compare}`}`;
      case 'timeseries': return `/api/stats/timeseries?${q}`;
      case 'realtime': return `/api/stats/realtime?${this.realtimeQuery()}`;
      case 'revenue': return `/api/stats/revenue?${q}`;
      case 'goals': return `/api/stats/goals?${q}`;
      case 'properties': return `/api/stats/properties?${q}`;
      case 'funnel':
        return `/api/stats/funnel?${q}&steps=${encodeURIComponent(s.funnelSteps)}&window=${encodeURIComponent('1 day')}${s.funnelModes ? `&modes=${encodeURIComponent(s.funnelModes)}` : ''}`;
      case 'retention': return `/api/stats/retention?${q}&weeks=${RETENTION_WEEKS}`;
      case 'sequences': return `/api/stats/sequences?${q}&steps=${encodeURIComponent(s.sequenceSteps)}`;
      case 'flow': return `/api/stats/flow?${q}&page=${encodeURIComponent(s.flowPage)}&direction=${s.flowDirection}`;
      default: return `/api/stats/breakdown/${key.slice(3)}?${q}&limit=${s.limits[key.slice(3)] || LIMIT_STEPS[0]}`;
    }
  }

  setRes(key, value) {
    this.res = { ...this.res, [key]: value };
    this.setState({ res: this.res, updatedAt: new Date() });
  }

  whenMain() {
    return this.mainSettled ? Promise.resolve() : new Promise((resolve) => this.mainWaiters.push(resolve));
  }

  /** Fetch one report. Stale results from an earlier view are dropped. */
  async load(key, { quiet = false } = {}) {
    const gen = this.gen;
    const signal = this.abort ? this.abort.signal : undefined;
    const prev = this.res[key];
    this.fetched.add(key);

    // Reports built from sessions answer 503 (`behavioral_unavailable`) when the
    // extension is missing or gdpr_mode has rounded the timestamps. `main`
    // already says whether session data exists, so ask it first rather than
    // provoke an error (and a console message) per panel.
    if (BEHAVIORAL_KEYS.has(key) || NEEDS_EXACT_TIME(key)) {
      await this.whenMain();
      if (gen !== this.gen) return;
      if (this.behavioral === false) {
        const reason = this.reason;
        // Weekly retention does not depend on the order of events inside an
        // hour, so a rounded-timestamp server can still answer it.
        if (!(key === 'retention' && reason === 'rounded')) {
          this.setRes(key, { kind: 'unavailable', reason, message: '' });
          return;
        }
      }
    }
    if (!quiet) this.setRes(key, prev && prev.kind === 'ok' ? { ...prev, loading: true } : { loading: true });

    const result = await api.get(this.urlFor(key), { signal });
    if (gen !== this.gen || result.kind === 'aborted') return;
    if (result.kind === 'unauthorized') {
      this.props.onAuthExpired();
      return;
    }
    if (key === 'main') {
      this.behavioral = result.kind === 'ok' ? result.data.behavioral_available !== false : null;
      // The server names the cause itself (`sessions_unavailable_reason`); an
      // older one that does not leaves it 'unknown' and the generic help shows.
      this.reason = this.behavioral === false ? reasonFromMain(result.data.sessions_unavailable_reason) : null;
      this.setState({ reason: this.reason });
      this.mainSettled = true;
      this.mainWaiters.splice(0).forEach((resolve) => resolve());
    }
    if (result.kind === 'unavailable') {
      result.reason = reasonFromCode(result.code);
    }
    if (quiet && result.kind !== 'ok' && prev && prev.kind === 'ok') {
      this.setRes(key, { ...prev, loading: false, offline: true });
      return;
    }
    this.setRes(key, result);
  }

  /** A panel scrolled into view: load its reports once per view. */
  want(...keys) {
    for (const key of keys) {
      this.wanted.add(key);
      if (!this.fetched.has(key) && this.state.siteId) this.load(key);
    }
  }

  retry(key) {
    if (key === 'main' || key === 'timeseries') this.refresh({ push: false });
    else this.load(key);
  }

  /** Reload everything on screen for the current view. */
  refresh({ push = false } = {}) {
    if (!this.state.siteId) return;
    if (this.abort) this.abort.abort();
    this.abort = new AbortController();
    this.gen += 1;
    this.fetched.clear();
    this.mainSettled = false;
    this.behavioral = null;
    this.reason = null;
    this.persist();
    this.syncUrl(push);
    document.title = `${this.state.siteId} · Mallard Metrics`;

    // Only the tab each group is showing is reloaded; the others were fetched
    // for a different view and would be stale if the operator switched to them.
    const active = new Set(BREAKDOWN_GROUPS.map((g) => `bd:${this.state.tabs[g.id]}`));
    this.res = Object.fromEntries(Object.entries(this.res).filter(([k]) => !k.startsWith('bd:') || active.has(k)));
    this.load('main');
    this.load('timeseries');
    for (const key of this.wanted) {
      if (!key.startsWith('bd:') || active.has(key)) this.load(key);
    }
  }

  syncUrl(push) {
    const qs = writeViewState(this.state);
    const next = `${location.pathname}${qs ? `?${qs}` : ''}`;
    if (next === `${location.pathname}${location.search}`) return;
    try {
      history[push ? 'pushState' : 'replaceState'](null, '', next);
    } catch (e) {
      /* a sandboxed frame may forbid it; the view just is not shareable */
    }
  }

  restoreFromUrl() {
    const url = readViewState(location.search, FILTERABLE_SLUGS);
    this.setState(
      {
        siteId: url.siteId || this.state.siteId,
        period: url.period || '30d',
        startDate: url.startDate || this.state.startDate,
        endDate: url.endDate || this.state.endDate,
        compare: url.compare || 'none',
        filters: url.filters || [],
        rangeOpen: false,
      },
      () => this.refresh({ push: false }),
    );
  }

  /** Change part of the view, then reload and record it in the address bar. */
  change(partial) {
    this.setState(partial, () => this.refresh({ push: true }));
  }

  addFilter(dimension, value) {
    if (!FILTERABLE_SLUGS.has(dimension)) return;
    const filters = this.state.filters.filter((f) => f.dimension !== dimension).concat([{ dimension, value, negated: false }]);
    this.change({ filters });
  }

  toggleFilter(index) {
    this.change({ filters: this.state.filters.map((f, i) => (i === index ? { ...f, negated: !f.negated } : f)) });
  }

  removeFilter(index) {
    this.change({ filters: this.state.filters.filter((_, i) => i !== index) });
  }

  clearFilters() {
    if (this.state.filters.length > 0) this.change({ filters: [] });
  }

  async loadSites() {
    const result = await api.get('/api/sites');
    if (result.kind === 'unauthorized') return this.props.onAuthExpired();
    if (result.kind !== 'ok') {
      this.setState({ sites: [], notice: { tone: 'error', text: result.message } });
      return;
    }
    const sites = result.data.sites || [];
    // `details` carries each site's events in the last 30 days, busiest first.
    // An older server sends only the IDs; then no counts are shown.
    const details = Array.isArray(result.data.details) ? result.data.details : [];
    const siteEvents = Object.fromEntries(details.map((d) => [d.site_id, d.events_30d]));
    const busiest = details.find((d) => sites.includes(d.site_id))?.site_id ?? sites[0];
    this.setState((prev) => ({
      sites,
      siteEvents,
      // A linked site that does not exist falls back to the busiest real one.
      siteId: sites.length > 0 && !sites.includes(prev.siteId) ? busiest : prev.siteId,
    }));
  }

  pollRealtime() {
    if (document.hidden || !this.state.siteId || !this.fetched.has('realtime')) return;
    const offline = Object.values(this.res).some((r) => r && r.kind === 'network');
    if (offline) this.refresh({ push: false });
    else this.load('realtime', { quiet: true });
  }

  /* Range controls ------------------------------------------------------ */

  effectiveRange() {
    const { period, startDate, endDate } = this.state;
    if (period === 'custom' && startDate && endDate) return { start: startDate, end: endDate };
    return periodRange(period);
  }

  pickRange(value) {
    if (value === 'custom') {
      if (this.state.rangeOpen) return this.setState({ rangeOpen: false });
      const { start, end } = this.effectiveRange();
      return this.setState({ rangeOpen: true, draftStart: start, draftEnd: end, rangeError: '' });
    }
    this.setState({ rangeOpen: false, rangeError: '' });
    if (value !== this.state.period) this.change({ period: value });
  }

  applyRange(start = this.state.draftStart, end = this.state.draftEnd) {
    const rangeError = validateRange(start, end);
    if (rangeError) return this.setState({ draftStart: start, draftEnd: end, rangeError });
    this.setState({ rangeError: '', rangeOpen: false });
    this.change({ period: 'custom', startDate: start, endDate: end });
  }

  presetRange(name) {
    const r = rangePreset(name);
    this.setState({ draftStart: r.start, draftEnd: r.end }, () => this.applyRange(r.start, r.end));
  }

  /* Export --------------------------------------------------------------- */

  async exportData(kind, format) {
    const label = `${kind} ${format.toUpperCase()}`;
    this.setState({ notice: { tone: 'info', text: `Preparing ${label} export…` } });
    const res = await api.download(`/api/stats/export?${this.query()}&kind=${kind}&format=${format}`);
    if (res.kind === 'unauthorized') return this.props.onAuthExpired();
    if (res.kind !== 'ok') {
      const hint = res.status === 504 ? ' Try a shorter date range or fewer filters.' : '';
      this.setState({ notice: { tone: 'error', text: `The ${label} export failed: ${res.message}${hint}` } });
      return;
    }
    const name = res.filename || `mallard-${this.state.siteId}-${kind}.${format}`;
    const url = URL.createObjectURL(res.blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = name;
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 10000);
    this.setState({ notice: { tone: 'ok', text: `Downloaded ${name}` } });
  }

  /* Panels --------------------------------------------------------------- */

  togglePanel(name, event) {
    if (this.state.panel === name) return this.closePanel();
    this.panelTrigger = event.currentTarget;
    this.setState({ panel: name });
  }

  closePanel() {
    this.setState({ panel: null }, () => {
      if (this.panelTrigger && this.panelTrigger.isConnected) this.panelTrigger.focus();
    });
  }

  setTab(group, slug, visible = false) {
    if (visible) {
      this.want(`bd:${slug}`);
      return;
    }
    this.setState((s) => ({ tabs: { ...s.tabs, [group.id]: slug } }), () => {
      this.persist();
      this.want(`bd:${slug}`);
    });
  }

  /** "Show more" / "Show top 10" on a breakdown: refetch just that list with its new row count. */
  setLimit(slug, limit) {
    this.setState((s) => ({ limits: { ...s.limits, [slug]: limit } }), () => this.load(`bd:${slug}`));
  }

  applyFunnel(event) {
    event.preventDefault();
    const steps = this.state.funnelSteps.split(',').map((s) => s.trim()).filter(Boolean);
    if (steps.length < 2) {
      this.setState({ funnelError: 'A funnel needs at least two steps, separated by commas.' });
      return;
    }
    this.setState({ funnelError: '' });
    this.persist();
    this.want('funnel');
    this.load('funnel');
  }

  applyFlow(event) {
    event.preventDefault();
    if (!this.state.flowPage.trim()) return;
    this.persist();
    this.load('flow');
  }

  applySequence(event) {
    event.preventDefault();
    this.persist();
    this.load('sequences');
  }

  /* Rendering ------------------------------------------------------------ */

  renderToolbar() {
    const { sites, siteEvents, siteId, siteDraft, period, startDate, endDate, compare, rangeOpen, draftStart, draftEnd, rangeError, updatedAt } = this.state;
    const refreshing = Object.values(this.state.res).some((r) => r && r.loading);
    const today = todayUTC();
    return html`
      <div class="toolbar" role="group" aria-label="Report settings">
        ${sites && sites.length > 0
          ? html`
              <label class="field"><span class="field-label">Site</span>
                <select value=${siteId} onChange=${(e) => this.change({ siteId: e.target.value })}>
                  ${sites.map((site) => html`<option value=${site} title=${site}>${Object.hasOwn(siteEvents, site) ? `${site} · ${num(siteEvents[site])} events (30d)` : site}</option>`)}
                </select>
              </label>`
          : html`
              <form class="field" onSubmit=${(e) => { e.preventDefault(); if (siteDraft) this.change({ siteId: siteDraft }); }}>
                <label><span class="field-label">Site ID</span>
                  <input type="text" placeholder="example.com" value=${siteDraft} onInput=${(e) => this.setState({ siteDraft: e.target.value.trim() })} />
                </label>
              </form>`}

        <div class="field"><span class="field-label" id="range-label">Date range (UTC)</span>
          <${Segmented} label="Date range" options=${RANGE_OPTIONS} value=${period}
            pressedOverride=${(v) => (v === 'custom' ? period === 'custom' : v === period && period !== 'custom')}
            onChange=${(v) => this.pickRange(v)} />
        </div>

        <div class="field"><span class="field-label">Compare with</span>
          <${Segmented} label="Compare with" options=${COMPARE_OPTIONS} value=${compare} onChange=${(v) => this.change({ compare: v })} />
        </div>

        <div class="field field-end">
          <button type="button" class="btn-secondary" onClick=${() => this.refresh({ push: false })} disabled=${refreshing}>${refreshing ? 'Loading…' : 'Refresh'}</button>
        </div>
      </div>

      ${rangeOpen && html`
        <form class="range-editor" onSubmit=${(e) => { e.preventDefault(); this.applyRange(); }} aria-label="Custom date range">
          <label class="field"><span class="field-label">From</span>
            <input type="date" value=${draftStart} min=${shiftDay(today, MAX_RANGE_DAYS + 1)} max=${today} onInput=${(e) => this.setState({ draftStart: e.target.value })} aria-invalid=${rangeError ? 'true' : 'false'} aria-describedby="range-error" />
          </label>
          <label class="field"><span class="field-label">To</span>
            <input type="date" value=${draftEnd} max=${today} onInput=${(e) => this.setState({ draftEnd: e.target.value })} aria-invalid=${rangeError ? 'true' : 'false'} aria-describedby="range-error" />
          </label>
          <button type="submit">Apply range</button>
          <span class="range-presets">
            <button type="button" class="btn-link" onClick=${() => this.presetRange('yesterday')}>Yesterday</button>
            <button type="button" class="btn-link" onClick=${() => this.presetRange('this-month')}>This month</button>
            <button type="button" class="btn-link" onClick=${() => this.presetRange('last-month')}>Last month</button>
          </span>
          <p class="range-hint muted">Dates are inclusive and in UTC, up to ${MAX_RANGE_DAYS} days.</p>
          <p id="range-error" class="field-error" role="alert">${rangeError}</p>
        </form>
      `}
      ${!rangeOpen && html`<p class="range-summary muted">${this.rangeSummary()}</p>`}
    `;
  }

  /** The exact dates on screen, so "7D" is never ambiguous about whether it includes today. */
  rangeSummary() {
    const { period } = this.state;
    const { start, end } = this.effectiveRange();
    if (period === 'day') return `Today, ${fmtDate(end)}, so far (UTC). The chart shows hours.`;
    const label = { '7d': 'Last 7 days', '30d': 'Last 30 days', '90d': 'Last 90 days', '12mo': 'Last 12 months' }[period];
    return `${label ? `${label}, including today: ` : 'Showing '}${fmtDate(start)} – ${fmtDate(end)} (UTC)`;
  }

  renderOverview() {
    const { res, filters, compare } = this.state;
    const main = res.main;
    const m = main && main.kind === 'ok' ? main.data : null;
    const empty = m && m.total_events === 0 && filters.length === 0;
    const q = this.query();
    return html`
      <${Panel} id="overview" title="Overview"
        actions=${html`
          <div class="panel-actions" role="group" aria-label="Export this view">
            <button type="button" class="btn-link" onClick=${() => this.exportData('daily', 'csv')}>Daily CSV</button>
            <button type="button" class="btn-link" onClick=${() => this.exportData('raw', 'csv')}>Raw CSV</button>
            <button type="button" class="btn-link" onClick=${() => this.exportData('raw', 'json')}>Raw JSON</button>
          </div>`}
      >
        <${Async} state=${main} onRetry=${() => this.retry('main')} skeleton=${html`<${SkelCards} />`} render=${(d) => this.renderHeadline(d)} />
        ${empty
          ? html`
              <div class="empty-site">
                <h3>No events for ${this.state.siteId} in this period</h3>
                <p>If this site is new, add the tracking snippet. Otherwise try a longer date range.</p>
                <${Snippet} site=${this.state.siteId} />
              </div>`
          : html`
              <${Async} state=${res.timeseries} onRetry=${() => this.retry('timeseries')} skeleton=${html`<${SkelChart} />`}
                render=${(data) => html`<${TimeseriesChart} data=${data} />`} />`}
      </${Panel}>
    `;
  }

  renderHeadline(m) {
    const prev = m.comparison || null;
    const unavailable = m.behavioral_available === false;
    const rounded = unavailable && this.state.reason === 'rounded';
    const hint = unavailable ? (rounded ? 'Needs exact times' : 'Unavailable') : null;
    const delta = (key, invert) => (prev ? html`<${Delta} before=${prev[key]} after=${m[key]} invert=${invert} />` : null);
    // A baseline of zero produces no percentages (see `Delta`), so the note has
    // to say why, or it reads as a comparison that silently failed.
    const baselineEmpty = prev && prev.total_pageviews === 0 && prev.unique_visitors === 0;
    return html`
      ${prev && html`<p class="comparison-note">Compared with ${fmtDate(prev.start_date)} – ${fmtDate(prev.end_date)}${baselineEmpty ? ' — no data in that period, so there is nothing to compare against' : ''}</p>`}
      ${unavailable && html`<${Unavailable} reason=${this.state.reason} />`}
      <dl class="metrics-grid metrics-grid-overview">
        <${MetricCard} label="Unique visitors" help=${HELP.visitors} value=${compact(m.unique_visitors)} full=${num(m.unique_visitors)} delta=${delta('unique_visitors')} />
        <${MetricCard} label="Pageviews" help=${HELP.pageviews} value=${compact(m.total_pageviews)} full=${num(m.total_pageviews)} delta=${delta('total_pageviews')} />
        <${MetricCard} label="Total events" help=${HELP.events} value=${compact(m.total_events)} full=${num(m.total_events)} delta=${delta('total_events')} />
        <${MetricCard} label="Views per visitor" help=${HELP.viewsPerVisitor} value=${fixed(m.views_per_visitor)} />
        <${MetricCard} label="Visits" help=${HELP.visits} value=${compact(m.total_sessions)} full=${num(m.total_sessions)} hint=${hint} muted=${unavailable} delta=${delta('total_sessions')} />
        <${MetricCard} label="Bounce rate" help=${HELP.bounce} value=${pct(m.bounce_rate)} hint=${hint} muted=${unavailable} delta=${delta('bounce_rate', true)} />
        <${MetricCard} label="Visit duration" help=${HELP.duration} value=${duration(m.avg_visit_duration_secs)} hint=${hint} muted=${unavailable} delta=${delta('avg_visit_duration_secs')} />
        <${MetricCard} label="Views per visit" help=${HELP.viewsPerVisit} value=${fixed(m.views_per_visit)} hint=${hint} muted=${unavailable} />
      </dl>
      <details class="definitions">
        <summary>How these figures are measured</summary>
        <dl>
          ${DEFINITIONS.map(([term, text]) => html`<div><dt>${term}</dt><dd>${text}</dd></div>`)}
        </dl>
      </details>
    `;
  }

  renderFunnel() {
    const { res, funnelSteps, funnelModes, funnelError } = this.state;
    const specs = funnelSteps.split(',').map((s) => s.trim()).filter(Boolean);
    return html`
      <${Panel} id="funnel" title="Funnel" subtitle="Visitors reaching at least each step, within the conversion window">
        <${Sentinel} onVisible=${() => this.want('funnel')} />
        <form class="inline-controls" onSubmit=${(e) => this.applyFunnel(e)}>
          <label>Steps (page:/path or event:name, comma separated)
            <input type="text" value=${funnelSteps} placeholder="page:/,page:/pricing,event:signup" aria-invalid=${funnelError ? 'true' : 'false'} onInput=${(e) => this.setState({ funnelSteps: e.target.value })} />
          </label>
          <label>Modes (optional)
            <input type="text" value=${funnelModes} placeholder="e.g. strict_order, strict_increase" onInput=${(e) => this.setState({ funnelModes: e.target.value })} />
          </label>
          <button type="submit">Apply</button>
        </form>
        ${funnelError && html`<p class="field-error" role="alert">${funnelError}</p>`}
        <${Async} behavioral="compact" state=${res.funnel} onRetry=${() => this.load('funnel')} render=${(steps) => html`<${FunnelChart} steps=${steps} specs=${specs} />`} />
      </${Panel}>
    `;
  }

  renderBehavioralPanels() {
    const { res, sequenceSteps, flowPage, flowDirection } = this.state;
    return html`
      ${this.renderFunnel()}

      <${Panel} id="retention" title="Retention cohorts" subtitle="Weekly cohorts by first visit">
        <${Sentinel} onVisible=${() => this.want('retention')} />
        <${Async} behavioral="compact" state=${res.retention} onRetry=${() => this.load('retention')} render=${(data) => html`<${RetentionGrid} data=${data} />`} />
      </${Panel}>

      <${Panel} id="sequence" title="Sequence" subtitle="Visitors whose events matched this ordered pattern">
        <${Sentinel} onVisible=${() => this.want('sequences')} />
        <form class="inline-controls" onSubmit=${(e) => this.applySequence(e)}>
          <label>Steps (comma separated)
            <input type="text" value=${sequenceSteps} placeholder="page:/,event:signup" onInput=${(e) => this.setState({ sequenceSteps: e.target.value })} />
          </label>
          <button type="submit">Apply</button>
        </form>
        <${Async} behavioral="compact" state=${res.sequences} onRetry=${() => this.load('sequences')} skeleton=${html`<${SkelCards} count=${4} />`}
          render=${(d) => html`
            <dl class="metrics-grid">
              <${MetricCard} label="Converting visitors" value=${compact(d.converting_visitors)} full=${num(d.converting_visitors)} />
              <${MetricCard} label="Total visitors" value=${compact(d.total_visitors)} full=${num(d.total_visitors)} />
              <${MetricCard} label="Conversion rate" value=${pct(d.conversion_rate)} />
              <${MetricCard} label="Total completions" value=${compact(d.total_matches)} full=${num(d.total_matches)} />
            </dl>`} />
      </${Panel}>

      <${Panel} id="flow" title="Flow" subtitle="Where visitors go next, or where they came from">
        <${Sentinel} onVisible=${() => this.want('flow')} />
        <form class="inline-controls" onSubmit=${(e) => this.applyFlow(e)}>
          <label>Page
            <input type="text" value=${flowPage} onInput=${(e) => this.setState({ flowPage: e.target.value })} />
          </label>
          <label>Direction
            <select value=${flowDirection} onChange=${(e) => this.setState({ flowDirection: e.target.value })}>
              <option value="forward">Next page</option>
              <option value="backward">Previous page</option>
            </select>
          </label>
          <button type="submit">Apply</button>
        </form>
        <${Async} behavioral="compact" state=${res.flow} onRetry=${() => this.load('flow')}
          render=${(rows) => rows.length === 0
            ? html`<${Empty}>No flow data for this page</${Empty}>`
            : html`<${BreakdownList} rows=${rows.map((r) => ({ value: r.next_page, visitors: r.visitors, share: r.share }))} label=${flowDirection === 'forward' ? 'Next page' : 'Previous page'} columns=${[['Visitors', 'visitors'], ['Share', 'share', pct]]} />`} />
      </${Panel}>
    `;
  }

  render() {
    const { siteId, sites, res, filters, tabs, panel, notice, theme, siteDraft, updatedAt } = this.state;
    const query = this.query();
    const offline = Object.values(res).some((r) => r && r.kind === 'network');
    const noSites = sites && sites.length === 0 && !siteId;
    return html`
      <div class="dashboard">
        <a class="skip-link" href="#main" onClick=${(e) => { e.preventDefault(); const m = document.getElementById('main'); if (m) m.focus(); }}>Skip to content</a>
        <header class="app-header">
          <h1 class="brand"><img src="/favicon.svg" alt="" width="28" height="28" /> Mallard Metrics</h1>
          <div class="header-actions">
            <button type="button" class="btn-secondary" aria-expanded=${panel === 'install'} aria-controls="install" onClick=${(e) => this.togglePanel('install', e)}>Install snippet</button>
            ${this.props.onLogout && html`<button type="button" class="btn-secondary" aria-expanded=${panel === 'keys'} aria-controls="keys" onClick=${(e) => this.togglePanel('keys', e)}>API keys</button>`}
            <label class="theme-field"><span class="sr-only">Theme</span>
              <select value=${theme} onChange=${(e) => this.setState({ theme: e.target.value }, () => { this.applyTheme(); this.persist(); })}>
                <option value="auto">Auto theme</option>
                <option value="light">Light theme</option>
                <option value="dark">Dark theme</option>
              </select>
            </label>
            ${this.props.onLogout && html`<button type="button" class="btn-secondary" onClick=${this.props.onLogout}>Sign out</button>`}
          </div>
        </header>

        <main id="main" tabindex="-1">
        ${siteId || !noSites ? this.renderToolbar() : null}
        <div class="sr-only" role="status" aria-live="polite">${notice && notice.tone !== 'error' ? notice.text : ''}</div>

          ${notice && html`<div class=${`banner banner-${notice.tone}`} role=${notice.tone === 'error' ? 'alert' : 'note'}>
            <span>${notice.text}</span>
            <button type="button" class="btn-secondary" onClick=${() => this.setState({ notice: null })}>Dismiss</button>
          </div>`}

          ${offline && html`<div class="banner banner-error banner-offline" role="alert">
            <span><strong>Cannot reach the server.</strong> Nothing on this page is being refreshed. The dashboard retries on its own, or you can try now.</span>
            <button type="button" class="btn-secondary" onClick=${() => this.refresh({ push: false })}>Retry now</button>
          </div>`}

          ${this.props.setupRequired && html`
            <div class="banner banner-warning">
              <strong>This instance has no admin password.</strong>
              <span>Anyone who can reach it can read every site's analytics. Set one before exposing it to a network.</span>
              <button type="button" onClick=${this.props.onStartSetup}>Set admin password</button>
            </div>`}

          ${panel === 'keys' && this.props.onLogout && html`<${KeysPanel} onClose=${() => this.closePanel()} onAuthExpired=${this.props.onAuthExpired} />`}
          ${panel === 'install' && html`<${InstallPanel} site=${siteId || siteDraft} onClose=${() => this.closePanel()} />`}

          ${noSites && html`<${Welcome} siteDraft=${siteDraft} onDraft=${(v) => this.setState({ siteDraft: v })} onUse=${() => siteDraft && this.change({ siteId: siteDraft })} />`}

          ${siteId && html`
            <${FilterBar} filters=${filters} onRemove=${(i) => this.removeFilter(i)} onToggle=${(i) => this.toggleFilter(i)} onClear=${() => this.clearFilters()} />
            ${this.renderOverview()}

            <${RealtimePanel} state=${res.realtime} filtered=${filters.length > 0} onRetry=${() => this.load('realtime')} onVisible=${() => this.want('realtime')} />

            <div class="panel-grid">
              ${BREAKDOWN_GROUPS.map((group) => html`
                <${BreakdownGroup} group=${group} active=${tabs[group.id]} data=${res} limits=${this.state.limits} onLimit=${(slug, limit) => this.setLimit(slug, limit)} onTab=${(g, slug, visible) => this.setTab(g, slug, visible)}
                  onFilter=${(dimension, value) => this.addFilter(dimension, value)} onRetry=${(key) => this.load(key)} />`)}
            </div>

            <${GoalsPanel} goals=${res.goals} propertyKeys=${res.properties} query=${query}
              onRetry=${(key) => this.load(key)} onVisible=${() => this.want('goals', 'properties')} />
            <${RevenuePanel} state=${res.revenue} onRetry=${() => this.load('revenue')} onVisible=${() => this.want('revenue')} />
            ${this.renderBehavioralPanels()}
          `}
        </main>
      </div>
    `;
  }
}

/* ── App shell ─────────────────────────────────────────────────────────── */

class App extends Component {
  constructor() {
    super();
    this.state = { checked: false, authenticated: false, setupRequired: false, showSetup: false, unreachable: false };
  }

  componentDidMount() {
    this.checkAuth();
  }

  async checkAuth() {
    const res = await api.get('/api/auth/status');
    if (res.kind === 'ok') {
      const { authenticated, setup_required: setupRequired } = res.data;
      this.setState({ checked: true, authenticated, setupRequired, unreachable: false });
    } else {
      this.setState({ checked: true, authenticated: false, setupRequired: false, unreachable: res.kind === 'network' || res.kind === 'error' });
    }
  }

  async logout() {
    await api.send('POST', '/api/auth/logout');
    this.setState({ authenticated: false });
  }

  render(_, { checked, authenticated, setupRequired, showSetup, unreachable }) {
    if (!checked) return html`<div class="loading-screen" role="status">Loading…</div>`;
    if (unreachable) {
      return html`
        <main class="auth-overlay">
          <div class="auth-card">
            <h1><img src="/favicon.svg" alt="" width="28" height="28" /> Mallard Metrics</h1>
            <p class="auth-error" role="alert">Cannot reach the server.</p>
            <button type="button" onClick=${() => { this.setState({ checked: false }); this.checkAuth(); }}>Try again</button>
          </div>
        </main>`;
    }
    // Before setup the API reports `authenticated: true` because reads are open,
    // a deliberate deployment mode. The setup form is reachable from the banner.
    if (!authenticated || showSetup) {
      return html`
        <${LoginForm} setupRequired=${setupRequired}
          onLogin=${() => this.setState({ showSetup: false }, () => this.checkAuth())}
          onCancel=${showSetup ? () => this.setState({ showSetup: false }) : null} />`;
    }
    return html`
      <${Boundary} fallback=${html`<main class="auth-overlay"><div class="auth-card"><h1>Something went wrong</h1><p class="auth-error" role="alert">The dashboard hit an unexpected error.</p><button type="button" onClick=${() => location.reload()}>Reload</button></div></main>`}>
      <${Dashboard} setupRequired=${setupRequired} onStartSetup=${() => this.setState({ showSetup: true })}
        onLogout=${setupRequired ? null : () => this.logout()}
        onAuthExpired=${() => this.setState({ authenticated: false })} />
      </${Boundary}>`;
  }
}

const root = document.getElementById('app');
root.replaceChildren();
render(html`<${App} />`, root);
