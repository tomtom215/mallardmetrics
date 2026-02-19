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
          const label = d.date.slice(5);
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

function SessionCards({ data }) {
  if (!data) return null;
  return html`
    <div class="metrics-grid session-metrics">
      <div class="metric-card">
        <div class="metric-value">${data.total_sessions}</div>
        <div class="metric-label">Total Sessions</div>
      </div>
      <div class="metric-card">
        <div class="metric-value">${data.avg_session_duration_secs.toFixed(1)}s</div>
        <div class="metric-label">Avg Duration</div>
      </div>
      <div class="metric-card">
        <div class="metric-value">${data.avg_pages_per_session.toFixed(1)}</div>
        <div class="metric-label">Pages / Session</div>
      </div>
    </div>
  `;
}

function FunnelChart({ data }) {
  if (!data || data.length === 0) {
    return html`<div class="chart-empty">No funnel data</div>`;
  }

  const maxVisitors = Math.max(1, ...data.map(d => d.visitors));
  return html`
    <div class="funnel-chart">
      ${data.map((step, i) => {
        const pct = (step.visitors / maxVisitors * 100).toFixed(1);
        const width = Math.max(10, step.visitors / maxVisitors * 100);
        return html`
          <div class="funnel-step">
            <div class="funnel-label">Step ${step.step}: ${step.visitors} visitors (${pct}%)</div>
            <div class="funnel-bar" style="width: ${width}%"></div>
          </div>
        `;
      })}
    </div>
  `;
}

function RetentionGrid({ data }) {
  if (!data || data.length === 0) {
    return html`<div class="chart-empty">No retention data</div>`;
  }

  const maxWeeks = Math.max(...data.map(d => d.retained.length));
  return html`
    <div class="retention-table-wrap">
      <table class="breakdown-table retention-table">
        <thead>
          <tr>
            <th>Cohort</th>
            ${Array.from({ length: maxWeeks }, (_, i) => html`<th>W${i}</th>`)}
          </tr>
        </thead>
        <tbody>
          ${data.map(row => html`
            <tr>
              <td>${row.cohort_date}</td>
              ${row.retained.map(r => html`
                <td class=${r ? 'retained-yes' : 'retained-no'}>${r ? 'Y' : '-'}</td>
              `)}
            </tr>
          `)}
        </tbody>
      </table>
    </div>
  `;
}

function SequenceResult({ data }) {
  if (!data) return null;
  return html`
    <div class="metrics-grid">
      <div class="metric-card">
        <div class="metric-value">${data.converting_visitors}</div>
        <div class="metric-label">Converting</div>
      </div>
      <div class="metric-card">
        <div class="metric-value">${data.total_visitors}</div>
        <div class="metric-label">Total</div>
      </div>
      <div class="metric-card">
        <div class="metric-value">${(data.conversion_rate * 100).toFixed(1)}%</div>
        <div class="metric-label">Conversion Rate</div>
      </div>
    </div>
  `;
}

function FlowTable({ data }) {
  if (!data || data.length === 0) {
    return html`<div class="chart-empty">No flow data</div>`;
  }

  return html`
    <table class="breakdown-table">
      <thead>
        <tr><th>Next Page</th><th>Visitors</th></tr>
      </thead>
      <tbody>
        ${data.map(row => html`
          <tr>
            <td>${row.next_page}</td>
            <td>${row.visitors}</td>
          </tr>
        `)}
      </tbody>
    </table>
  `;
}

// --- Authentication components ---

function LoginForm({ onLogin, setupRequired }) {
  const handleSubmit = async (e) => {
    e.preventDefault();
    const password = e.target.elements.password.value;
    const endpoint = setupRequired ? '/api/auth/setup' : '/api/auth/login';
    try {
      const res = await fetch(endpoint, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ password }),
      });
      if (res.ok) {
        onLogin();
      } else {
        const body = await res.json().catch(() => ({}));
        alert(body.error || `Error ${res.status}`);
      }
    } catch (err) {
      alert('Network error — could not reach server.');
    }
  };

  const title = setupRequired ? 'Set Admin Password' : 'Sign In';
  const hint = setupRequired
    ? 'No password is currently set. Create one to protect the dashboard.'
    : 'Enter your admin password to access the dashboard.';
  const action = setupRequired ? 'Set Password' : 'Sign In';

  return html`
    <div class="auth-overlay">
      <div class="auth-card">
        <h1>Mallard Metrics</h1>
        <h2>${title}</h2>
        <p class="auth-hint">${hint}</p>
        <form onSubmit=${handleSubmit}>
          <input
            type="password"
            name="password"
            placeholder="Password"
            minlength="8"
            required
            autofocus
          />
          <button type="submit">${action}</button>
        </form>
      </div>
    </div>
  `;
}

// --- Main Dashboard ---

class Dashboard extends Component {
  constructor() {
    super();
    this.state = {
      metrics: null,
      timeseries: null,
      breakdowns: {},
      sessions: null,
      funnel: null,
      retention: null,
      sequences: null,
      flow: null,
      period: '30d',
      siteId: '',
      loading: false,
      error: null,
      funnelSteps: 'page:/,page:/pricing,event:signup',
      sequenceSteps: 'page:/,event:signup',
      flowPage: '/',
    };
  }

  async fetchMetrics() {
    const { siteId, period, funnelSteps, sequenceSteps, flowPage } = this.state;
    if (!siteId) return;

    this.setState({ loading: true, error: null });
    const qs = `site_id=${encodeURIComponent(siteId)}&period=${period}`;

    try {
      const [mainRes, tsRes, pagesRes, sourcesRes, browsersRes, osRes, devicesRes, countriesRes, sessionsRes] =
        await Promise.all([
          fetch(`/api/stats/main?${qs}`),
          fetch(`/api/stats/timeseries?${qs}`),
          fetch(`/api/stats/breakdown/pages?${qs}`),
          fetch(`/api/stats/breakdown/sources?${qs}`),
          fetch(`/api/stats/breakdown/browsers?${qs}`),
          fetch(`/api/stats/breakdown/os?${qs}`),
          fetch(`/api/stats/breakdown/devices?${qs}`),
          fetch(`/api/stats/breakdown/countries?${qs}`),
          fetch(`/api/stats/sessions?${qs}`),
        ]);

      if (mainRes.status === 401) {
        // Session expired — force re-auth
        if (this.props.onAuthExpired) this.props.onAuthExpired();
        return;
      }
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
      const sessions = sessionsRes.ok ? await sessionsRes.json() : null;

      // Fetch behavioral analytics (these may fail without the extension)
      const [funnelRes, retentionRes, seqRes, flowRes] = await Promise.all([
        fetch(`/api/stats/funnel?${qs}&steps=${encodeURIComponent(funnelSteps)}&window=1 day`),
        fetch(`/api/stats/retention?${qs}&weeks=4`),
        fetch(`/api/stats/sequences?${qs}&steps=${encodeURIComponent(sequenceSteps)}`),
        fetch(`/api/stats/flow?${qs}&page=${encodeURIComponent(flowPage)}`),
      ]);

      const funnel = funnelRes.ok ? await funnelRes.json() : null;
      const retention = retentionRes.ok ? await retentionRes.json() : null;
      const sequences = seqRes.ok ? await seqRes.json() : null;
      const flow = flowRes.ok ? await flowRes.json() : null;

      this.setState({ metrics, timeseries, breakdowns, sessions, funnel, retention, sequences, flow, loading: false });
    } catch (e) {
      this.setState({ error: e.message, loading: false });
    }
  }

  render() {
    const { metrics, timeseries, breakdowns, sessions, funnel, retention, sequences, flow, period, siteId, loading, error, funnelSteps, sequenceSteps, flowPage } = this.state;
    const { onLogout } = this.props;

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
            ${onLogout && html`
              <button class="btn-logout" onClick=${onLogout}>Sign Out</button>
            `}
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
        ${sessions && html`
          <section class="section">
            <h2>Sessions</h2>
            <${SessionCards} data=${sessions} />
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
        ${metrics && html`
          <section class="section">
            <h2>Funnel Analysis</h2>
            <div class="analytics-controls">
              <input
                type="text"
                placeholder="Steps (e.g., page:/,page:/pricing,event:signup)"
                value=${funnelSteps}
                onInput=${(e) => this.setState({ funnelSteps: e.target.value })}
              />
            </div>
            <div class="analytics-card">
              <${FunnelChart} data=${funnel} />
            </div>
          </section>
        `}
        ${metrics && html`
          <section class="section">
            <h2>Retention Cohorts</h2>
            <div class="analytics-card">
              <${RetentionGrid} data=${retention} />
            </div>
          </section>
        `}
        ${metrics && html`
          <section class="section">
            <h2>Sequence Analysis</h2>
            <div class="analytics-controls">
              <input
                type="text"
                placeholder="Steps (e.g., page:/,event:signup)"
                value=${sequenceSteps}
                onInput=${(e) => this.setState({ sequenceSteps: e.target.value })}
              />
            </div>
            <div class="analytics-card">
              <${SequenceResult} data=${sequences} />
            </div>
          </section>
        `}
        ${metrics && html`
          <section class="section">
            <h2>Flow Analysis</h2>
            <div class="analytics-controls">
              <input
                type="text"
                placeholder="Page path (e.g., /)"
                value=${flowPage}
                onInput=${(e) => this.setState({ flowPage: e.target.value })}
              />
            </div>
            <div class="analytics-card">
              <${FlowTable} data=${flow} />
            </div>
          </section>
        `}
      </div>
    `;
  }
}

// --- App shell with auth gate ---

class App extends Component {
  constructor() {
    super();
    this.state = {
      authChecked: false,
      authenticated: false,
      setupRequired: false,
    };
  }

  async componentDidMount() {
    await this.checkAuth();
  }

  async checkAuth() {
    try {
      const res = await fetch('/api/auth/status');
      if (res.ok) {
        const { authenticated, setup_required } = await res.json();
        this.setState({ authChecked: true, authenticated, setupRequired: setup_required });
      } else {
        // Treat fetch errors as unauthenticated
        this.setState({ authChecked: true, authenticated: false, setupRequired: false });
      }
    } catch (_) {
      this.setState({ authChecked: true, authenticated: false, setupRequired: false });
    }
  }

  async handleLogout() {
    await fetch('/api/auth/logout', { method: 'POST' }).catch(() => {});
    this.setState({ authenticated: false });
  }

  handleLogin() {
    // After login/setup, recheck auth status to get the updated state
    this.checkAuth();
  }

  render() {
    const { authChecked, authenticated, setupRequired } = this.state;

    if (!authChecked) {
      return html`<div class="loading-screen">Loading...</div>`;
    }

    if (!authenticated) {
      return html`
        <${LoginForm}
          setupRequired=${setupRequired}
          onLogin=${() => this.handleLogin()}
        />
      `;
    }

    // authenticated=true: password not set (open access) or valid session
    const showLogout = !setupRequired; // only show logout when password is configured
    return html`
      <${Dashboard}
        onLogout=${showLogout ? () => this.handleLogout() : null}
        onAuthExpired=${() => this.setState({ authenticated: false })}
      />
    `;
  }
}

render(html`<${App} />`, document.getElementById('app'));
