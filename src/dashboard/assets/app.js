import { h, render, Component } from 'https://esm.sh/preact@10.19.3';
import htm from 'https://esm.sh/htm@3.1.1';

const html = htm.bind(h);

function TimeseriesChart({ data }) {
  if (!data || data.length === 0) {
    return html`<div class="chart-empty">No data for this period</div>`;
  }

  const W = 800, H = 200, PAD = { top: 20, right: 20, bottom: 40, left: 50 };
  const cw = W - PAD.left - PAD.right;
  const ch = H - PAD.top - PAD.bottom;

  const maxV = Math.max(1, ...data.map(d => d.visitors));
  const maxP = Math.max(1, ...data.map(d => d.pageviews));
  const maxY = Math.max(maxV, maxP);

  const xStep = data.length > 1 ? cw / (data.length - 1) : cw / 2;

  function points(key) {
    return data.map((d, i) => {
      const x = PAD.left + (data.length > 1 ? i * xStep : cw / 2);
      const y = PAD.top + ch - (d[key] / maxY) * ch;
      return `${x},${y}`;
    }).join(' ');
  }

  // Y-axis ticks
  const yTicks = [0, Math.round(maxY / 2), maxY];

  // X-axis labels (show subset to avoid crowding)
  const labelCount = Math.min(data.length, 7);
  const labelStep = Math.max(1, Math.floor((data.length - 1) / (labelCount - 1)));

  return html`
    <div class="chart-container">
      <svg viewBox="0 0 ${W} ${H}" class="chart-svg">
        <!-- Y axis -->
        ${yTicks.map(v => {
          const y = PAD.top + ch - (v / maxY) * ch;
          return html`
            <line x1=${PAD.left} y1=${y} x2=${W - PAD.right} y2=${y} stroke="#e0e0e0" stroke-width="1" />
            <text x=${PAD.left - 8} y=${y + 4} text-anchor="end" fill="#999" font-size="11">${v}</text>
          `;
        })}
        <!-- Visitors line -->
        <polyline fill="none" stroke="#4a90d9" stroke-width="2" points=${points('visitors')} />
        <!-- Pageviews line -->
        <polyline fill="none" stroke="#50c878" stroke-width="2" stroke-dasharray="4,2" points=${points('pageviews')} />
        <!-- X labels -->
        ${data.map((d, i) => {
          if (i % labelStep !== 0 && i !== data.length - 1) return null;
          const x = PAD.left + (data.length > 1 ? i * xStep : cw / 2);
          const label = d.date.length > 10 ? d.date.slice(5) : d.date.slice(5);
          return html`<text x=${x} y=${H - 5} text-anchor="middle" fill="#999" font-size="10">${label}</text>`;
        })}
      </svg>
      <div class="chart-legend">
        <span class="legend-item"><span class="legend-line visitors-line"></span> Visitors</span>
        <span class="legend-item"><span class="legend-line pageviews-line"></span> Pageviews</span>
      </div>
    </div>
  `;
}

function BreakdownTable({ title, data }) {
  if (!data || data.length === 0) {
    return html`
      <div class="breakdown-card">
        <h3>${title}</h3>
        <div class="breakdown-empty">No data</div>
      </div>
    `;
  }

  return html`
    <div class="breakdown-card">
      <h3>${title}</h3>
      <table class="breakdown-table">
        <thead>
          <tr>
            <th>${title}</th>
            <th>Visitors</th>
            <th>Pageviews</th>
          </tr>
        </thead>
        <tbody>
          ${data.map(row => html`
            <tr>
              <td>${row.value || '(unknown)'}</td>
              <td>${row.visitors}</td>
              <td>${row.pageviews}</td>
            </tr>
          `)}
        </tbody>
      </table>
    </div>
  `;
}

class Dashboard extends Component {
  constructor() {
    super();
    this.state = {
      metrics: null,
      timeseries: null,
      breakdowns: {},
      period: '30d',
      siteId: '',
      loading: false,
      error: null,
    };
  }

  async fetchMetrics() {
    const { siteId, period } = this.state;
    if (!siteId) return;

    this.setState({ loading: true, error: null });
    const qs = `site_id=${encodeURIComponent(siteId)}&period=${period}`;

    try {
      const [mainRes, tsRes, pagesRes, sourcesRes, browsersRes, osRes, devicesRes, countriesRes] =
        await Promise.all([
          fetch(`/api/stats/main?${qs}`),
          fetch(`/api/stats/timeseries?${qs}`),
          fetch(`/api/stats/breakdown/pages?${qs}`),
          fetch(`/api/stats/breakdown/sources?${qs}`),
          fetch(`/api/stats/breakdown/browsers?${qs}`),
          fetch(`/api/stats/breakdown/os?${qs}`),
          fetch(`/api/stats/breakdown/devices?${qs}`),
          fetch(`/api/stats/breakdown/countries?${qs}`),
        ]);

      if (!mainRes.ok) throw new Error(`HTTP ${mainRes.status}`);

      const metrics = await mainRes.json();
      const timeseries = tsRes.ok ? await tsRes.json() : [];
      const breakdowns = {
        pages: pagesRes.ok ? await pagesRes.json() : [],
        sources: sourcesRes.ok ? await sourcesRes.json() : [],
        browsers: browsersRes.ok ? await browsersRes.json() : [],
        os: osRes.ok ? await osRes.json() : [],
        devices: devicesRes.ok ? await devicesRes.json() : [],
        countries: countriesRes.ok ? await countriesRes.json() : [],
      };

      this.setState({ metrics, timeseries, breakdowns, loading: false });
    } catch (e) {
      this.setState({ error: e.message, loading: false });
    }
  }

  render() {
    const { metrics, timeseries, breakdowns, period, siteId, loading, error } = this.state;

    return html`
      <div class="dashboard">
        <header>
          <h1>Mallard Metrics</h1>
          <div class="controls">
            <input
              type="text"
              placeholder="Site ID (e.g., example.com)"
              value=${siteId}
              onInput=${(e) => this.setState({ siteId: e.target.value })}
            />
            <select
              value=${period}
              onChange=${(e) => this.setState({ period: e.target.value }, () => this.fetchMetrics())}
            >
              <option value="day">Today</option>
              <option value="7d">Last 7 days</option>
              <option value="30d">Last 30 days</option>
              <option value="90d">Last 90 days</option>
            </select>
            <button onClick=${() => this.fetchMetrics()} disabled=${loading}>
              ${loading ? 'Loading...' : 'Load'}
            </button>
          </div>
        </header>
        ${error && html`<div class="error">${error}</div>`}
        ${metrics && html`
          <div class="metrics-grid">
            <div class="metric-card">
              <div class="metric-value">${metrics.unique_visitors}</div>
              <div class="metric-label">Unique Visitors</div>
            </div>
            <div class="metric-card">
              <div class="metric-value">${metrics.total_pageviews}</div>
              <div class="metric-label">Total Pageviews</div>
            </div>
            <div class="metric-card">
              <div class="metric-value">${(metrics.bounce_rate * 100).toFixed(1)}%</div>
              <div class="metric-label">Bounce Rate</div>
            </div>
            <div class="metric-card">
              <div class="metric-value">${metrics.pages_per_visit.toFixed(1)}</div>
              <div class="metric-label">Pages / Visit</div>
            </div>
          </div>
        `}
        ${timeseries && html`
          <section class="section">
            <h2>Visitors & Pageviews</h2>
            <${TimeseriesChart} data=${timeseries} />
          </section>
        `}
        ${metrics && html`
          <section class="section">
            <h2>Breakdowns</h2>
            <div class="breakdowns-grid">
              <${BreakdownTable} title="Pages" data=${breakdowns.pages} />
              <${BreakdownTable} title="Sources" data=${breakdowns.sources} />
              <${BreakdownTable} title="Browsers" data=${breakdowns.browsers} />
              <${BreakdownTable} title="OS" data=${breakdowns.os} />
              <${BreakdownTable} title="Devices" data=${breakdowns.devices} />
              <${BreakdownTable} title="Countries" data=${breakdowns.countries} />
            </div>
          </section>
        `}
      </div>
    `;
  }
}

render(html`<${Dashboard} />`, document.getElementById('app'));
