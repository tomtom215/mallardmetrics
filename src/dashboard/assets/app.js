import { h, render, Component } from 'https://esm.sh/preact@10.19.3';
import htm from 'https://esm.sh/htm@3.1.1';

const html = htm.bind(h);

class Dashboard extends Component {
  constructor() {
    super();
    this.state = {
      metrics: null,
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
    try {
      const res = await fetch(`/api/stats/main?site_id=${encodeURIComponent(siteId)}&period=${period}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = await res.json();
      this.setState({ metrics: data, loading: false });
    } catch (e) {
      this.setState({ error: e.message, loading: false });
    }
  }

  render() {
    const { metrics, period, siteId, loading, error } = this.state;

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
      </div>
    `;
  }
}

render(html`<${Dashboard} />`, document.getElementById('app'));
