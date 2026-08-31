use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Application configuration loaded from a TOML file and/or environment variables.
///
/// `deny_unknown_fields` is deliberate: a typo such as `retention_dayz = 30`
/// would otherwise be silently ignored and the operator would believe retention
/// was configured when it was not.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "default_flush_count")]
    pub flush_event_count: usize,
    #[serde(default = "default_flush_interval_secs")]
    pub flush_interval_secs: u64,
    /// Site IDs permitted to send events. Empty = accept any site ID.
    ///
    /// When non-empty this is enforced against BOTH the request `Origin` header
    /// and the `d` (domain) field of the event payload, so an unlisted site
    /// cannot create partitions by omitting `Origin`.
    #[serde(default)]
    pub site_ids: Vec<String>,
    /// Path to a MaxMind GeoLite2 .mmdb file for IP geolocation.
    /// If not set or the file is missing, GeoIP lookups return None.
    #[serde(default)]
    pub geoip_db_path: Option<PathBuf>,
    /// Dashboard origin for CORS restrictions on stats/dashboard routes.
    /// If not set, stats routes allow all origins.
    #[serde(default)]
    pub dashboard_origin: Option<String>,
    /// Whether to filter bot traffic from analytics (default: true).
    #[serde(default = "default_filter_bots")]
    pub filter_bots: bool,
    /// Data retention period in days. 0 = unlimited (no cleanup).
    #[serde(default)]
    pub retention_days: u32,
    /// Session TTL in seconds for dashboard authentication (default: 86400 = 24h).
    #[serde(default = "default_session_ttl_secs")]
    pub session_ttl_secs: u64,
    /// Graceful shutdown timeout in seconds (default: 30).
    #[serde(default = "default_shutdown_timeout_secs")]
    pub shutdown_timeout_secs: u64,
    /// Maximum events per second per site_id for rate limiting. 0 = no limit.
    #[serde(default)]
    pub rate_limit_per_site: u32,
    /// Maximum events per second per client IP. 0 = no limit.
    ///
    /// Complements `rate_limit_per_site`: a per-site budget alone lets a single
    /// abusive client consume the whole allowance and deny service to the
    /// site's real visitors.
    #[serde(default)]
    pub rate_limit_per_ip: u32,
    /// Query cache TTL in seconds (default: 60). 0 = no caching.
    #[serde(default = "default_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
    /// Log output format: "text" (default) or "json" for structured JSON logs.
    #[serde(default = "default_log_format")]
    pub log_format: String,
    /// Maximum failed login attempts per IP before lockout. 0 = disabled (default: 5).
    #[serde(default = "default_max_login_attempts")]
    pub max_login_attempts: u32,
    /// Duration in seconds to lock out an IP after exceeding max_login_attempts (default: 300).
    #[serde(default = "default_login_lockout_secs")]
    pub login_lockout_secs: u64,
    /// Maximum number of cached query results (0 = unlimited, default: 10000).
    #[serde(default = "default_cache_max_entries")]
    pub cache_max_entries: usize,
    /// Maximum concurrent analytics queries (0 = unlimited, default: 10).
    #[serde(default = "default_max_concurrent_queries")]
    pub max_concurrent_queries: usize,
    /// Force the Secure flag on session cookies regardless of dashboard_origin.
    /// Set to true when the server is deployed behind a TLS-terminating reverse proxy.
    #[serde(default)]
    pub secure_cookies: bool,

    // ── Deployment topology ──────────────────────────────────────────────
    /// Trust `X-Forwarded-For` / `X-Real-IP` for the client address.
    ///
    /// Default false. When the server is reachable directly, these headers are
    /// attacker-controlled: trusting them lets a client forge its visitor ID,
    /// its geolocation, and its rate-limit bucket. Enable this ONLY when every
    /// request arrives through a reverse proxy that overwrites the header
    /// (the bundled Caddy config does).
    ///
    /// When false, the peer socket address is used instead.
    #[serde(default)]
    pub trust_proxy_headers: bool,

    /// Number of read-only DuckDB connections used to serve analytics queries.
    ///
    /// Reads run on their own connections so a slow dashboard query cannot block
    /// event ingestion, which needs the writer connection. 0 or 1 disables the
    /// pool and shares the writer connection (previous behaviour).
    #[serde(default = "default_read_connections")]
    pub read_connections: usize,

    /// Hard cap on events held in memory awaiting a flush. 0 = unlimited.
    ///
    /// Without a cap, a persistently failing flush (full disk, bad permissions)
    /// grows the retry buffer without bound until the process is OOM-killed.
    #[serde(default = "default_max_buffered_events")]
    pub max_buffered_events: usize,

    /// Maximum number of tracked rate-limit buckets / login-attempt records.
    ///
    /// Both maps are keyed by attacker-influenced values, so they need a cap
    /// independent of the periodic cleanup sweep.
    #[serde(default = "default_max_tracked_keys")]
    pub max_tracked_keys: usize,

    /// Maximum number of concurrent dashboard sessions retained in memory.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,

    // ── Analytics behaviour ──────────────────────────────────────────────
    /// Inactivity gap, in minutes, that ends a session (default: 30).
    ///
    /// Used by every `sessionize`-based metric: sessions, bounce rate, and
    /// average visit duration.
    #[serde(default = "default_session_window_minutes")]
    pub session_window_minutes: u32,

    /// Window, in minutes, considered "right now" by the realtime endpoint.
    #[serde(default = "default_realtime_window_minutes")]
    pub realtime_window_minutes: u32,

    /// Compact a Parquet partition once it holds at least this many files.
    /// 0 disables compaction.
    ///
    /// Each flush writes a new file, so a 60-second flush interval produces
    /// ~1440 files per site per day. Scanning thousands of tiny files is the
    /// dominant cost of every query on a long retention horizon.
    #[serde(default = "default_compact_after_files")]
    pub compact_after_files: usize,

    // ── Privacy / GDPR configuration ─────────────────────────────────────
    /// GDPR-friendly mode: convenience preset that enables the full privacy bundle.
    ///
    /// When true, the following flags are forced on regardless of their individual
    /// settings: `strip_referrer_query`, `round_timestamps`, `suppress_browser_version`,
    /// `suppress_os_version`, `suppress_screen_size`.  `geoip_precision` is reduced to
    /// at most "country" (a stricter setting such as "none" is left unchanged).
    ///
    /// `suppress_visitor_id` is intentionally NOT activated by `gdpr_mode` because
    /// it eliminates the unique-visitor metric entirely.  Set it explicitly if needed.
    #[serde(default)]
    pub gdpr_mode: bool,

    /// Strip query string and fragment from referrer URLs before storing.
    ///
    /// Prevents leaking search terms and campaign parameters embedded in referrer
    /// URLs (e.g. `https://google.com/search?q=medical+condition` → `https://google.com/search`).
    /// Default: false. Enabled automatically when `gdpr_mode = true`.
    #[serde(default)]
    pub strip_referrer_query: bool,

    /// Round event timestamps down to the start of the hour before storing.
    ///
    /// Reduces fingerprinting risk by lowering timestamp precision. Daily and
    /// hourly aggregates remain accurate; sub-hour session analysis does not.
    /// Default: false. Enabled automatically when `gdpr_mode = true`.
    #[serde(default)]
    pub round_timestamps: bool,

    /// Replace the HMAC-based visitor_id with a random UUID per request.
    ///
    /// Breaks cross-request linkability entirely. Consequence: unique-visitor
    /// counts degrade to page-load counts and every session is one page long.
    ///
    /// Default: false. NOT activated automatically by `gdpr_mode`.
    #[serde(default)]
    pub suppress_visitor_id: bool,

    /// Store browser name only, omitting browser version.
    #[serde(default)]
    pub suppress_browser_version: bool,

    /// Store OS name only, omitting OS version.
    #[serde(default)]
    pub suppress_os_version: bool,

    /// Do not store the screen_size or device_type fields.
    #[serde(default)]
    pub suppress_screen_size: bool,

    /// Geographic precision for IP geolocation. Valid values, most to least
    /// granular: `"city"` (default), `"region"`, `"country"`, `"none"`.
    ///
    /// `gdpr_mode = true` reduces anything more granular than `"country"` to
    /// `"country"`.
    #[serde(default = "default_geoip_precision")]
    pub geoip_precision: String,

    /// Number of days a visitor-ID salt remains in use before rotating (default: 1).
    ///
    /// The salt is what makes a visitor ID pseudonymous and unlinkable across
    /// time. Rotating daily is the most privacy-protective setting and is the
    /// default, but it also means a visitor seen on two different days has two
    /// unrelated IDs. That has direct analytical consequences:
    ///
    /// - "Unique visitors" over a multi-day range counts visitor-days, not people.
    /// - Weekly retention cohorts cannot work at all: nobody is ever "retained",
    ///   because the returning visitor carries a different ID.
    ///
    /// Raising this (e.g. to 7 or 30) makes those metrics meaningful at the cost
    /// of longer-lived pseudonymous identifiers. `/api/stats/retention` reports
    /// whether the configured rotation supports the requested cohort length.
    #[serde(default = "default_visitor_salt_rotation_days")]
    pub visitor_salt_rotation_days: u32,

    // ── DuckDB runtime ───────────────────────────────────────────────────
    /// Directory DuckDB installs and loads community extensions from.
    ///
    /// Defaults to `data_dir/extensions`. DuckDB's own default is
    /// `$HOME/.duckdb/extensions`, which does not work in the shipped
    /// container: the `FROM scratch` image sets no `HOME`, and the recommended
    /// compose file runs with a read-only root filesystem. The `behavioral`
    /// extension therefore failed to install, and every funnel, retention,
    /// session, sequence and flow request answered 503 — on a deployment that
    /// looked healthy in every other respect. Pointing it at the data volume,
    /// which is writable by definition, makes the install work and puts the
    /// downloaded extension somewhere an operator can find and back up.
    #[serde(default)]
    pub extension_directory: Option<PathBuf>,

    /// DuckDB `memory_limit`, e.g. `"512MB"` or `"2GB"`. Unset = DuckDB's
    /// default, which is 80% of system RAM.
    ///
    /// Worth setting on a shared host: an expensive analytics query is
    /// otherwise entitled to most of the machine, and the process that gets
    /// OOM-killed for it is the one also handling ingestion.
    #[serde(default)]
    pub duckdb_memory_limit: Option<String>,

    /// DuckDB worker threads. Unset = one per core.
    ///
    /// Capping this leaves headroom for the ingest path on a small box, where
    /// a single query would otherwise saturate every core.
    #[serde(default)]
    pub duckdb_threads: Option<u32>,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

const fn default_port() -> u16 {
    8000
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}

const fn default_flush_count() -> usize {
    1000
}

const fn default_flush_interval_secs() -> u64 {
    60
}

const fn default_filter_bots() -> bool {
    true
}

const fn default_session_ttl_secs() -> u64 {
    86400
}

const fn default_shutdown_timeout_secs() -> u64 {
    30
}

const fn default_cache_ttl_secs() -> u64 {
    60
}

fn default_log_format() -> String {
    "text".to_string()
}

const fn default_max_login_attempts() -> u32 {
    5
}

const fn default_login_lockout_secs() -> u64 {
    300
}

const fn default_cache_max_entries() -> usize {
    10_000
}

const fn default_max_concurrent_queries() -> usize {
    10
}

const fn default_read_connections() -> usize {
    4
}

const fn default_max_buffered_events() -> usize {
    100_000
}

const fn default_max_tracked_keys() -> usize {
    10_000
}

const fn default_max_sessions() -> usize {
    10_000
}

const fn default_session_window_minutes() -> u32 {
    30
}

const fn default_realtime_window_minutes() -> u32 {
    5
}

const fn default_compact_after_files() -> usize {
    24
}

fn default_geoip_precision() -> String {
    "city".to_string()
}

const fn default_visitor_salt_rotation_days() -> u32 {
    1
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            data_dir: default_data_dir(),
            flush_event_count: default_flush_count(),
            flush_interval_secs: default_flush_interval_secs(),
            site_ids: Vec::new(),
            geoip_db_path: None,
            dashboard_origin: None,
            filter_bots: default_filter_bots(),
            retention_days: 0,
            session_ttl_secs: default_session_ttl_secs(),
            shutdown_timeout_secs: default_shutdown_timeout_secs(),
            rate_limit_per_site: 0,
            rate_limit_per_ip: 0,
            cache_ttl_secs: default_cache_ttl_secs(),
            log_format: default_log_format(),
            max_login_attempts: default_max_login_attempts(),
            login_lockout_secs: default_login_lockout_secs(),
            cache_max_entries: default_cache_max_entries(),
            max_concurrent_queries: default_max_concurrent_queries(),
            secure_cookies: false,
            trust_proxy_headers: false,
            read_connections: default_read_connections(),
            max_buffered_events: default_max_buffered_events(),
            max_tracked_keys: default_max_tracked_keys(),
            max_sessions: default_max_sessions(),
            session_window_minutes: default_session_window_minutes(),
            realtime_window_minutes: default_realtime_window_minutes(),
            compact_after_files: default_compact_after_files(),
            gdpr_mode: false,
            strip_referrer_query: false,
            round_timestamps: false,
            suppress_visitor_id: false,
            suppress_browser_version: false,
            suppress_os_version: false,
            suppress_screen_size: false,
            geoip_precision: default_geoip_precision(),
            visitor_salt_rotation_days: default_visitor_salt_rotation_days(),
            extension_directory: None,
            duckdb_memory_limit: None,
            duckdb_threads: None,
        }
    }
}

/// A loaded configuration plus any non-fatal warnings raised while loading it.
///
/// Warnings are returned rather than logged directly because configuration is
/// resolved *before* the tracing subscriber is initialised — the subscriber's
/// output format is itself a configuration value.
#[derive(Debug)]
pub struct LoadedConfig {
    pub config: Config,
    pub warnings: Vec<String>,
}

/// Parse a boolean environment variable.
///
/// Accepts `1/true/yes/on` and `0/false/no/off`, case-insensitively. Anything
/// else returns `None` so the caller can warn instead of silently choosing a
/// value — the previous `val != "0" && val != "false"` rule quietly read
/// `MALLARD_FILTER_BOTS=no` as `true`.
fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Is `authority` a bare `host[:port]`, as an `Origin` header must be?
///
/// Deliberately strict: an origin is not a URL. It carries no userinfo, no
/// path, no query and no fragment, and its host is a registered name or an IP
/// literal — never arbitrary text. A `HeaderValue` check alone is far too weak
/// here, because a space is a legal header-value byte.
fn is_valid_authority(authority: &str) -> bool {
    // Split the port off first. An IPv6 literal is bracketed and contains
    // colons of its own, so the port separator is the colon after the ']'.
    let (host, port) = match authority.rfind(']') {
        Some(close) if close + 1 == authority.len() => (authority, None),
        Some(close) => match authority[close + 1..].strip_prefix(':') {
            Some(port) => (&authority[..=close], Some(port)),
            None => return false,
        },
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };

    // Port 0 is never a real listener, so an origin naming it is a typo.
    if let Some(port) = port
        && !port.parse::<u16>().is_ok_and(|p| p > 0)
    {
        return false;
    }

    if let Some(rest) = host.strip_prefix('[') {
        return rest
            .strip_suffix(']')
            .is_some_and(|inner| inner.parse::<std::net::Ipv6Addr>().is_ok());
    }

    // A registered name: ASCII letters, digits, '-' and '.' only. Rejecting
    // empty labels rules out "host..name" and a leading or trailing dot along
    // with the empty host itself.
    !host.is_empty()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// Is `value` a DuckDB memory size such as `512MB`, `2GB` or `1.5 GiB`?
///
/// The value is interpolated into a `SET memory_limit` statement, so it is
/// checked here rather than trusted. DuckDB would reject a malformed size at
/// startup anyway; validating first means the operator gets a message naming
/// the setting instead of a raw binder error.
fn is_valid_memory_limit(value: &str) -> bool {
    let trimmed = value.trim();
    let digits_end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(digits_end);
    if number.is_empty() || number.parse::<f64>().is_err() {
        return false;
    }
    matches!(
        unit.trim().to_ascii_uppercase().as_str(),
        "" | "B" | "KB" | "KIB" | "MB" | "MIB" | "GB" | "GIB" | "TB" | "TIB"
    )
}

/// Ordering of `geoip_precision` values from most to least granular.
fn geoip_precision_rank(value: &str) -> Option<u8> {
    match value {
        "city" => Some(3),
        "region" => Some(2),
        "country" => Some(1),
        "none" => Some(0),
        _ => None,
    }
}

impl Config {
    /// Load configuration from an optional TOML file, then apply environment overrides.
    ///
    /// # Errors
    ///
    /// Returns an error when `config_path` is `Some` and the file cannot be read
    /// or parsed. Silently falling back to defaults in that case was dangerous:
    /// a single typo could disable retention, drop the site allowlist, or open
    /// the dashboard, with only a log line to say so.
    #[allow(clippy::too_many_lines)]
    pub fn load(config_path: Option<&Path>) -> Result<LoadedConfig, String> {
        let mut warnings: Vec<String> = Vec::new();

        let mut config = match config_path {
            Some(path) => {
                let contents = std::fs::read_to_string(path)
                    .map_err(|e| format!("failed to read config file {}: {e}", path.display()))?;
                toml::from_str(&contents)
                    .map_err(|e| format!("failed to parse config file {}: {e}", path.display()))?
            }
            None => Self::default(),
        };

        macro_rules! parse_env_num {
            ($var:literal, $field:expr, $ty:ty) => {
                if let Ok(raw) = std::env::var($var) {
                    match raw.trim().parse::<$ty>() {
                        Ok(v) => $field = v,
                        Err(_) => warnings.push(format!(
                            "{}={raw:?} is not a valid {}; keeping {}",
                            $var,
                            stringify!($ty),
                            $field
                        )),
                    }
                }
            };
        }

        macro_rules! parse_env_bool {
            ($var:literal, $field:expr) => {
                if let Ok(raw) = std::env::var($var) {
                    match parse_bool(&raw) {
                        Some(v) => $field = v,
                        None => warnings.push(format!(
                            "{}={raw:?} is not a boolean (use true/false); keeping {}",
                            $var, $field
                        )),
                    }
                }
            };
        }

        if let Ok(host) = std::env::var("MALLARD_HOST") {
            config.host = host;
        }
        parse_env_num!("MALLARD_PORT", config.port, u16);
        if let Ok(data_dir) = std::env::var("MALLARD_DATA_DIR") {
            config.data_dir = PathBuf::from(data_dir);
        }
        parse_env_num!("MALLARD_FLUSH_COUNT", config.flush_event_count, usize);
        parse_env_num!("MALLARD_FLUSH_INTERVAL", config.flush_interval_secs, u64);
        if let Ok(geoip) = std::env::var("MALLARD_GEOIP_DB") {
            config.geoip_db_path = Some(PathBuf::from(geoip));
        }
        if let Ok(origin) = std::env::var("MALLARD_DASHBOARD_ORIGIN") {
            if origin.trim().is_empty() {
                config.dashboard_origin = None;
            } else {
                config.dashboard_origin = Some(origin);
            }
        }
        // Comma-separated allowlist; the previous release could only set this
        // from TOML, which forced container deployments to mount a config file
        // just to restrict ingestion.
        if let Ok(sites) = std::env::var("MALLARD_SITE_IDS") {
            config.site_ids = sites
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        parse_env_bool!("MALLARD_FILTER_BOTS", config.filter_bots);
        parse_env_num!("MALLARD_RETENTION_DAYS", config.retention_days, u32);
        parse_env_num!("MALLARD_SESSION_TTL", config.session_ttl_secs, u64);
        parse_env_num!(
            "MALLARD_SHUTDOWN_TIMEOUT",
            config.shutdown_timeout_secs,
            u64
        );
        parse_env_num!("MALLARD_RATE_LIMIT", config.rate_limit_per_site, u32);
        parse_env_num!("MALLARD_RATE_LIMIT_PER_IP", config.rate_limit_per_ip, u32);
        parse_env_num!("MALLARD_CACHE_TTL", config.cache_ttl_secs, u64);
        if let Ok(val) = std::env::var("MALLARD_LOG_FORMAT") {
            config.log_format = val;
        }
        parse_env_num!("MALLARD_MAX_LOGIN_ATTEMPTS", config.max_login_attempts, u32);
        parse_env_num!("MALLARD_LOGIN_LOCKOUT", config.login_lockout_secs, u64);
        parse_env_num!("MALLARD_CACHE_MAX_ENTRIES", config.cache_max_entries, usize);
        parse_env_num!(
            "MALLARD_MAX_CONCURRENT_QUERIES",
            config.max_concurrent_queries,
            usize
        );
        parse_env_bool!("MALLARD_SECURE_COOKIES", config.secure_cookies);
        parse_env_bool!("MALLARD_TRUST_PROXY_HEADERS", config.trust_proxy_headers);
        parse_env_num!("MALLARD_READ_CONNECTIONS", config.read_connections, usize);
        parse_env_num!(
            "MALLARD_MAX_BUFFERED_EVENTS",
            config.max_buffered_events,
            usize
        );
        parse_env_num!("MALLARD_MAX_TRACKED_KEYS", config.max_tracked_keys, usize);
        parse_env_num!("MALLARD_MAX_SESSIONS", config.max_sessions, usize);
        parse_env_num!(
            "MALLARD_SESSION_WINDOW_MINUTES",
            config.session_window_minutes,
            u32
        );
        parse_env_num!(
            "MALLARD_REALTIME_WINDOW_MINUTES",
            config.realtime_window_minutes,
            u32
        );
        parse_env_num!(
            "MALLARD_COMPACT_AFTER_FILES",
            config.compact_after_files,
            usize
        );

        // Privacy / GDPR configuration env vars
        parse_env_bool!("MALLARD_GDPR_MODE", config.gdpr_mode);
        parse_env_bool!("MALLARD_STRIP_REFERRER_QUERY", config.strip_referrer_query);
        parse_env_bool!("MALLARD_ROUND_TIMESTAMPS", config.round_timestamps);
        parse_env_bool!("MALLARD_SUPPRESS_VISITOR_ID", config.suppress_visitor_id);
        parse_env_bool!(
            "MALLARD_SUPPRESS_BROWSER_VERSION",
            config.suppress_browser_version
        );
        parse_env_bool!("MALLARD_SUPPRESS_OS_VERSION", config.suppress_os_version);
        parse_env_bool!("MALLARD_SUPPRESS_SCREEN_SIZE", config.suppress_screen_size);
        if let Ok(val) = std::env::var("MALLARD_GEOIP_PRECISION") {
            config.geoip_precision = val;
        }
        parse_env_num!(
            "MALLARD_VISITOR_SALT_ROTATION_DAYS",
            config.visitor_salt_rotation_days,
            u32
        );

        // DuckDB runtime settings
        if let Ok(dir) = std::env::var("MALLARD_EXTENSION_DIR") {
            config.extension_directory =
                Some(PathBuf::from(dir)).filter(|p| !p.as_os_str().is_empty());
        }
        if let Ok(limit) = std::env::var("MALLARD_DUCKDB_MEMORY_LIMIT") {
            let limit = limit.trim().to_string();
            config.duckdb_memory_limit = (!limit.is_empty()).then_some(limit);
        }
        if let Ok(raw) = std::env::var("MALLARD_DUCKDB_THREADS") {
            match raw.trim().parse::<u32>() {
                Ok(v) if v > 0 => config.duckdb_threads = Some(v),
                _ => warnings.push(format!(
                    "MALLARD_DUCKDB_THREADS={raw:?} is not a positive integer; keeping the DuckDB default"
                )),
            }
        }

        config.apply_gdpr_mode();

        Ok(LoadedConfig { config, warnings })
    }

    /// Apply the `gdpr_mode` preset. Idempotent, and a no-op when disabled.
    ///
    /// Runs after every other source has been resolved so it always wins.
    pub fn apply_gdpr_mode(&mut self) {
        if !self.gdpr_mode {
            return;
        }
        self.strip_referrer_query = true;
        self.round_timestamps = true;
        self.suppress_browser_version = true;
        self.suppress_os_version = true;
        self.suppress_screen_size = true;

        // Reduce anything more granular than "country" down to "country".
        // "region" is MORE granular than "country" (it is country + subdivision),
        // so it must be reduced too; an earlier version left it untouched on the
        // mistaken belief that it was already stricter.
        if geoip_precision_rank(&self.geoip_precision).is_some_and(|rank| rank > 1) {
            self.geoip_precision = "country".to_string();
        }
    }

    /// Returns the path to the events directory.
    pub fn events_dir(&self) -> PathBuf {
        self.data_dir.join("events")
    }

    /// Returns the path to the DuckDB database file.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("mallard.duckdb")
    }

    /// Directory DuckDB installs community extensions into.
    pub fn extension_dir(&self) -> PathBuf {
        self.extension_directory
            .clone()
            .unwrap_or_else(|| self.data_dir.join("extensions"))
    }

    /// Session inactivity window as a DuckDB interval literal, e.g. `30 minutes`.
    pub fn session_window_interval(&self) -> String {
        format!("{} minutes", self.session_window_minutes.max(1))
    }

    /// Validate that configuration values are internally consistent.
    ///
    /// Called at startup so misconfiguration is reported before the server binds
    /// rather than as a panic on the first request.
    ///
    /// # Errors
    ///
    /// Returns a human-readable description of the first problem found.
    pub fn validate(&self) -> Result<(), String> {
        if self.flush_event_count == 0 {
            return Err(
                "flush_event_count must be > 0; 0 would mean events never auto-flush".to_string(),
            );
        }
        if self.flush_interval_secs == 0 {
            return Err(
                "flush_interval_secs must be > 0; 0 would spin the flush timer at maximum CPU"
                    .to_string(),
            );
        }
        if self.session_ttl_secs == 0 {
            return Err(
                "session_ttl_secs must be > 0; 0 would expire every session immediately"
                    .to_string(),
            );
        }
        if self.session_window_minutes == 0 {
            return Err("session_window_minutes must be > 0".to_string());
        }
        if self.realtime_window_minutes == 0 {
            return Err("realtime_window_minutes must be > 0".to_string());
        }
        if self.visitor_salt_rotation_days == 0 {
            return Err(
                "visitor_salt_rotation_days must be >= 1; 0 has no meaningful rotation period"
                    .to_string(),
            );
        }
        if self.max_buffered_events > 0 && self.max_buffered_events < self.flush_event_count {
            return Err(format!(
                "max_buffered_events ({}) must be >= flush_event_count ({}), \
                 otherwise the buffer would drop events before a flush could ever trigger",
                self.max_buffered_events, self.flush_event_count
            ));
        }
        if geoip_precision_rank(&self.geoip_precision).is_none() {
            return Err(format!(
                "geoip_precision must be one of: city, region, country, none (got {:?})",
                self.geoip_precision
            ));
        }
        if let Some(limit) = &self.duckdb_memory_limit
            && !is_valid_memory_limit(limit)
        {
            return Err(format!(
                "duckdb_memory_limit must be a size such as \"512MB\" or \"2GB\" (got {limit:?})"
            ));
        }
        if self.duckdb_threads == Some(0) {
            return Err("duckdb_threads must be >= 1 when set".to_string());
        }
        if !matches!(self.log_format.as_str(), "text" | "json") {
            return Err(format!(
                "log_format must be \"text\" or \"json\" (got {:?})",
                self.log_format
            ));
        }
        for site in &self.site_ids {
            crate::api::stats::validate_site_id(site).map_err(|_| {
                format!(
                    "site_ids entry {site:?} is not a valid site ID \
                     (allowed: alphanumeric plus '.', '-', '_', ':')"
                )
            })?;
        }
        // An unparsable dashboard_origin previously fell back to "*", which
        // tower-http rejects when combined with Allow-Credentials — turning a
        // config typo into a panic on the first cross-origin request.
        if let Some(origin) = &self.dashboard_origin {
            if !(origin.starts_with("http://") || origin.starts_with("https://")) {
                return Err(format!(
                    "dashboard_origin must be a full origin including scheme, \
                     e.g. \"https://analytics.example.com\" (got {origin:?})"
                ));
            }
            // Everything after the scheme must be a bare authority. Checking
            // the authority rather than the whole string matters twice over:
            // "https://host" carries two slashes of its own, so counting
            // slashes across the string rejected every well-formed origin; and
            // a HeaderValue check alone is too weak, because a space is a legal
            // header-value byte and "https://exa mple.com" is not an origin.
            let authority = origin
                .strip_prefix("https://")
                .or_else(|| origin.strip_prefix("http://"))
                .unwrap_or(origin);
            if authority.contains('/') {
                return Err(format!(
                    "dashboard_origin must not include a path or trailing slash \
                     (got {origin:?})"
                ));
            }
            if !is_valid_authority(authority) {
                return Err(format!(
                    "dashboard_origin must be scheme://host[:port] with no \
                     credentials, path or whitespace (got {origin:?})"
                ));
            }
            if origin.parse::<axum::http::HeaderValue>().is_err() {
                return Err(format!(
                    "dashboard_origin is not a valid HTTP header value (got {origin:?})"
                ));
            }
        }
        Ok(())
    }

    /// Warnings about configurations that are valid but likely to surprise.
    ///
    /// Emitted at startup once logging is initialised.
    pub fn advisories(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.gdpr_mode && self.retention_days == 0 {
            out.push(
                "gdpr_mode is enabled but retention_days is 0 (unlimited). Consider \
                 MALLARD_RETENTION_DAYS=30 for GDPR Art. 5(1)(e) storage limitation."
                    .to_string(),
            );
        }
        if self.visitor_salt_rotation_days == 1 {
            out.push(
                "visitor_salt_rotation_days=1: visitor IDs rotate every UTC day. \
                 Unique-visitor counts over multi-day ranges count visitor-days, and \
                 weekly retention cohorts cannot be computed. Raise it to enable \
                 cross-day analysis (at the cost of longer-lived pseudonyms)."
                    .to_string(),
            );
        }
        if self.trust_proxy_headers {
            out.push(
                "trust_proxy_headers is enabled: X-Forwarded-For and X-Real-IP are trusted. \
                 Ensure every request reaches this server through a proxy that overwrites \
                 them, or clients can forge their visitor ID, geolocation and rate-limit bucket."
                    .to_string(),
            );
        }
        if self.suppress_visitor_id {
            out.push(
                "suppress_visitor_id is enabled: unique-visitor counts degrade to page-load \
                 counts and session metrics become meaningless."
                    .to_string(),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    /// Serialises tests that read process-wide environment variables.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Acquire [`ENV_LOCK`], recovering if a previous test panicked while
    /// holding it.
    ///
    /// Without this, one genuine assertion failure poisons the mutex and every
    /// other environment test fails with `PoisonError` — hiding the one real
    /// failure behind a dozen fake ones.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Set an environment variable for the duration of a test.
    ///
    /// `std::env::set_var` is `unsafe` in edition 2024 because it is not
    /// thread-safe; every caller holds ENV_LOCK, and the test harness never
    /// touches the environment from other threads.
    fn set_env(key: &str, value: &str) {
        unsafe { std::env::set_var(key, value) };
    }

    fn remove_env(key: &str) {
        unsafe { std::env::remove_var(key) };
    }

    /// Restore an environment variable to a previously captured value.
    fn restore_env(key: &str, original: Option<String>) {
        match original {
            Some(v) => set_env(key, &v),
            None => remove_env(key),
        }
    }

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 8000);
        assert_eq!(config.data_dir, PathBuf::from("data"));
        assert_eq!(config.flush_event_count, 1000);
        assert_eq!(config.flush_interval_secs, 60);
        assert!(config.site_ids.is_empty());
        assert!(config.geoip_db_path.is_none());
        assert!(config.dashboard_origin.is_none());
        assert!(config.filter_bots);
        assert_eq!(config.retention_days, 0);
        assert_eq!(config.session_ttl_secs, 86400);
        assert_eq!(config.shutdown_timeout_secs, 30);
        assert_eq!(config.rate_limit_per_site, 0);
        assert_eq!(config.rate_limit_per_ip, 0);
        assert_eq!(config.cache_ttl_secs, 60);
        assert_eq!(config.log_format, "text");
        assert_eq!(config.max_login_attempts, 5);
        assert_eq!(config.login_lockout_secs, 300);
        assert!(!config.trust_proxy_headers);
        assert_eq!(config.session_window_minutes, 30);
        assert_eq!(config.visitor_salt_rotation_days, 1);
    }

    #[test]
    fn test_validate_valid_config() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn test_validate_zero_flush_count() {
        let config = Config {
            flush_event_count: 0,
            ..Config::default()
        };
        assert!(config.validate().unwrap_err().contains("flush_event_count"));
    }

    #[test]
    fn test_validate_zero_flush_interval() {
        let config = Config {
            flush_interval_secs: 0,
            ..Config::default()
        };
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("flush_interval_secs")
        );
    }

    #[test]
    fn test_validate_zero_session_ttl() {
        let config = Config {
            session_ttl_secs: 0,
            ..Config::default()
        };
        assert!(config.validate().unwrap_err().contains("session_ttl_secs"));
    }

    #[test]
    fn test_validate_zero_session_window() {
        let config = Config {
            session_window_minutes: 0,
            ..Config::default()
        };
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("session_window_minutes")
        );
    }

    #[test]
    fn test_validate_zero_salt_rotation() {
        let config = Config {
            visitor_salt_rotation_days: 0,
            ..Config::default()
        };
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("visitor_salt_rotation_days")
        );
    }

    #[test]
    fn test_validate_buffer_cap_below_flush_threshold() {
        let config = Config {
            flush_event_count: 1000,
            max_buffered_events: 100,
            ..Config::default()
        };
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("max_buffered_events")
        );
    }

    #[test]
    fn test_validate_rejects_malformed_dashboard_origin() {
        for bad in [
            // No scheme.
            "analytics.example.com",
            "//analytics.example.com",
            // A path, which an Origin header never carries.
            "https://analytics.example.com/dashboard",
            "https://analytics.example.com/",
            // Whitespace: a legal header-value byte, but not a legal host.
            "https://exa mple.com",
            "https://analytics.example.com\t",
            // No host at all.
            "https://",
            "https://:8000",
            // Malformed hosts.
            "https://analytics..example.com",
            "https://.example.com",
            "https://example.com.",
            "https://user:pass@example.com",
            // Malformed ports.
            "https://example.com:0",
            "https://example.com:99999",
            "https://example.com:http",
            "https://example.com:",
            // An unbracketed IPv6 literal is ambiguous with host:port.
            "https://::1",
        ] {
            let config = Config {
                dashboard_origin: Some(bad.to_string()),
                ..Config::default()
            };
            assert!(
                config.validate().is_err(),
                "dashboard_origin {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn test_validate_accepts_well_formed_dashboard_origin() {
        for good in [
            "https://analytics.example.com",
            "http://localhost",
            "http://localhost:8000",
            "https://analytics.example.com:8443",
            "http://127.0.0.1:3000",
            "http://[::1]:8000",
            "https://[2001:db8::1]",
            "https://xn--80ak6aa92e.com",
            "http://my-host-1.internal:65535",
        ] {
            let config = Config {
                dashboard_origin: Some(good.to_string()),
                ..Config::default()
            };
            assert!(
                config.validate().is_ok(),
                "dashboard_origin {good:?} must be accepted"
            );
        }
    }

    #[test]
    fn test_validate_rejects_bad_log_format() {
        let config = Config {
            log_format: "xml".to_string(),
            ..Config::default()
        };
        assert!(config.validate().unwrap_err().contains("log_format"));
    }

    #[test]
    fn test_validate_rejects_bad_site_id() {
        let config = Config {
            site_ids: vec!["good.com".to_string(), "bad site/id".to_string()],
            ..Config::default()
        };
        assert!(config.validate().unwrap_err().contains("site_ids"));
    }

    #[test]
    fn test_load_from_toml() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mut file = std::fs::File::create(&config_path).unwrap();
        write!(
            file,
            r#"
host = "127.0.0.1"
port = 9000
data_dir = "/tmp/mallard"
flush_event_count = 500
flush_interval_secs = 30
site_ids = ["example.com", "other.org"]
geoip_db_path = "/data/GeoLite2-City.mmdb"
dashboard_origin = "https://analytics.example.com"
filter_bots = false
retention_days = 90
session_ttl_secs = 3600
trust_proxy_headers = true
session_window_minutes = 45
visitor_salt_rotation_days = 30
"#
        )
        .unwrap();

        let config = Config::load(Some(&config_path)).unwrap().config;
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 9000);
        assert_eq!(config.data_dir, PathBuf::from("/tmp/mallard"));
        assert_eq!(config.flush_event_count, 500);
        assert_eq!(config.flush_interval_secs, 30);
        assert_eq!(config.site_ids, vec!["example.com", "other.org"]);
        assert_eq!(
            config.geoip_db_path,
            Some(PathBuf::from("/data/GeoLite2-City.mmdb"))
        );
        assert_eq!(
            config.dashboard_origin.as_deref(),
            Some("https://analytics.example.com")
        );
        assert!(!config.filter_bots);
        assert_eq!(config.retention_days, 90);
        assert_eq!(config.session_ttl_secs, 3600);
        assert!(config.trust_proxy_headers);
        assert_eq!(config.session_window_minutes, 45);
        assert_eq!(config.visitor_salt_rotation_days, 30);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_load_missing_file_is_an_error() {
        let _guard = env_lock();
        let err = Config::load(Some(Path::new("/nonexistent/config.toml"))).unwrap_err();
        assert!(err.contains("failed to read config file"), "got: {err}");
    }

    #[test]
    fn test_load_invalid_toml_is_an_error() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "this is not valid toml {{{").unwrap();

        let err = Config::load(Some(&config_path)).unwrap_err();
        assert!(err.contains("failed to parse config file"), "got: {err}");
    }

    #[test]
    fn test_load_rejects_unknown_key() {
        // A typo must be reported, not silently ignored.
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "retention_dayz = 30\n").unwrap();

        let err = Config::load(Some(&config_path)).unwrap_err();
        assert!(err.contains("retention_dayz"), "got: {err}");
    }

    #[test]
    fn test_load_no_path_uses_defaults() {
        let _guard = env_lock();
        let config = Config::load(None).unwrap().config;
        assert_eq!(config.port, 8000);
        assert_eq!(config.host, "0.0.0.0");
    }

    #[test]
    fn test_events_dir() {
        let config = Config {
            data_dir: PathBuf::from("/var/mallard"),
            ..Config::default()
        };
        assert_eq!(config.events_dir(), PathBuf::from("/var/mallard/events"));
    }

    #[test]
    fn test_db_path() {
        let config = Config {
            data_dir: PathBuf::from("/var/mallard"),
            ..Config::default()
        };
        assert_eq!(
            config.db_path(),
            PathBuf::from("/var/mallard/mallard.duckdb")
        );
    }

    #[test]
    fn test_session_window_interval() {
        let config = Config {
            session_window_minutes: 45,
            ..Config::default()
        };
        assert_eq!(config.session_window_interval(), "45 minutes");
    }

    #[test]
    fn test_secure_cookies_default_false() {
        assert!(!Config::default().secure_cookies);
    }

    #[test]
    fn test_invalid_numeric_env_var_warns_and_keeps_value() {
        let _guard = env_lock();
        let orig = std::env::var("MALLARD_PORT").ok();
        set_env("MALLARD_PORT", "not_a_number");

        let loaded = Config::load(None).unwrap();
        assert_eq!(loaded.config.port, 8000);
        assert!(
            loaded.warnings.iter().any(|w| w.contains("MALLARD_PORT")),
            "expected a warning about MALLARD_PORT, got {:?}",
            loaded.warnings
        );

        restore_env("MALLARD_PORT", orig);
    }

    #[test]
    fn test_env_var_overrides() {
        let _guard = env_lock();
        let orig = std::env::var("MALLARD_PORT").ok();
        set_env("MALLARD_PORT", "3000");
        assert_eq!(Config::load(None).unwrap().config.port, 3000);
        restore_env("MALLARD_PORT", orig);
    }

    #[test]
    fn test_site_ids_env_override() {
        let _guard = env_lock();
        let orig = std::env::var("MALLARD_SITE_IDS").ok();
        set_env("MALLARD_SITE_IDS", " a.com , b.com ,, ");
        let config = Config::load(None).unwrap().config;
        assert_eq!(config.site_ids, vec!["a.com", "b.com"]);
        restore_env("MALLARD_SITE_IDS", orig);
    }

    #[test]
    fn test_bool_env_parsing_is_strict() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("TRUE"), Some(true));
        assert_eq!(parse_bool("1"), Some(true));
        assert_eq!(parse_bool("on"), Some(true));
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("0"), Some(false));
        // The previous rule read all of these as `true`.
        assert_eq!(parse_bool("no"), Some(false));
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool(""), None);
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn test_bool_env_var_rejects_garbage_with_warning() {
        let _guard = env_lock();
        let orig = std::env::var("MALLARD_FILTER_BOTS").ok();
        set_env("MALLARD_FILTER_BOTS", "maybe");
        let loaded = Config::load(None).unwrap();
        assert!(loaded.config.filter_bots, "value must be left unchanged");
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("MALLARD_FILTER_BOTS"))
        );
        restore_env("MALLARD_FILTER_BOTS", orig);
    }

    #[test]
    fn test_filter_bots_env_no_is_false() {
        let _guard = env_lock();
        let orig = std::env::var("MALLARD_FILTER_BOTS").ok();
        set_env("MALLARD_FILTER_BOTS", "no");
        assert!(!Config::load(None).unwrap().config.filter_bots);
        restore_env("MALLARD_FILTER_BOTS", orig);
    }

    #[test]
    fn test_default_gdpr_flags() {
        let config = Config::default();
        assert!(!config.gdpr_mode);
        assert!(!config.strip_referrer_query);
        assert!(!config.round_timestamps);
        assert!(!config.suppress_visitor_id);
        assert!(!config.suppress_browser_version);
        assert!(!config.suppress_os_version);
        assert!(!config.suppress_screen_size);
        assert_eq!(config.geoip_precision, "city");
    }

    #[test]
    fn test_gdpr_mode_enables_privacy_bundle() {
        let mut config = Config {
            gdpr_mode: true,
            ..Config::default()
        };
        config.apply_gdpr_mode();
        assert!(config.strip_referrer_query);
        assert!(config.round_timestamps);
        assert!(config.suppress_browser_version);
        assert!(config.suppress_os_version);
        assert!(config.suppress_screen_size);
        assert_eq!(config.geoip_precision, "country");
        // suppress_visitor_id is NOT forced by gdpr_mode.
        assert!(!config.suppress_visitor_id);
    }

    #[test]
    fn test_gdpr_mode_applied_through_load() {
        // The preset must be exercised through the real load() path, not a
        // reimplementation of it inside the test.
        let _guard = env_lock();
        let orig = std::env::var("MALLARD_GDPR_MODE").ok();
        set_env("MALLARD_GDPR_MODE", "true");
        let config = Config::load(None).unwrap().config;
        assert!(config.gdpr_mode);
        assert!(config.strip_referrer_query);
        assert!(config.round_timestamps);
        assert_eq!(config.geoip_precision, "country");
        restore_env("MALLARD_GDPR_MODE", orig);
    }

    #[test]
    fn test_gdpr_mode_keeps_stricter_geoip_precision() {
        let mut config = Config {
            gdpr_mode: true,
            geoip_precision: "none".to_string(),
            ..Config::default()
        };
        config.apply_gdpr_mode();
        assert_eq!(config.geoip_precision, "none");
    }

    #[test]
    fn test_gdpr_mode_reduces_region_precision() {
        // "region" is country + subdivision, i.e. MORE granular than "country",
        // so gdpr_mode must reduce it. It was previously left untouched.
        let mut config = Config {
            gdpr_mode: true,
            geoip_precision: "region".to_string(),
            ..Config::default()
        };
        config.apply_gdpr_mode();
        assert_eq!(config.geoip_precision, "country");
    }

    #[test]
    fn test_gdpr_mode_is_idempotent() {
        let mut config = Config {
            gdpr_mode: true,
            ..Config::default()
        };
        config.apply_gdpr_mode();
        let once = config.clone();
        config.apply_gdpr_mode();
        assert_eq!(config.geoip_precision, once.geoip_precision);
        assert_eq!(config.round_timestamps, once.round_timestamps);
    }

    #[test]
    fn test_geoip_precision_ranking() {
        assert!(geoip_precision_rank("city") > geoip_precision_rank("region"));
        assert!(geoip_precision_rank("region") > geoip_precision_rank("country"));
        assert!(geoip_precision_rank("country") > geoip_precision_rank("none"));
        assert!(geoip_precision_rank("district").is_none());
    }

    #[test]
    fn test_validate_invalid_geoip_precision() {
        let config = Config {
            geoip_precision: "district".to_string(),
            ..Config::default()
        };
        assert!(config.validate().unwrap_err().contains("geoip_precision"));
    }

    #[test]
    fn test_validate_valid_geoip_precisions() {
        for precision in ["city", "region", "country", "none"] {
            let config = Config {
                geoip_precision: precision.to_string(),
                ..Config::default()
            };
            assert!(config.validate().is_ok(), "expected valid: {precision}");
        }
    }

    #[test]
    fn test_shipped_example_config_parses_and_validates() {
        // The example file documents every field; if a field is renamed without
        // updating it, `deny_unknown_fields` makes this fail rather than letting
        // operators copy a file that silently ignores half its settings.
        let _guard = env_lock();
        let example =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("mallard-metrics.toml.example");
        let loaded = Config::load(Some(&example))
            .unwrap_or_else(|e| panic!("the shipped example config must parse: {e}"));
        loaded
            .config
            .validate()
            .unwrap_or_else(|e| panic!("the shipped example config must validate: {e}"));
    }

    #[test]
    fn test_example_config_documents_every_environment_variable() {
        // Each MALLARD_* variable read by load() must appear in the example, or
        // operators have no way to discover it.
        let source = include_str!("config.rs");
        let example = include_str!("../mallard-metrics.toml.example");
        // Only the non-test half of the file: the tests below discuss variable
        // names in prose, and prose is not a code path operators depend on.
        let source = source
            .split_once("\nmod tests {")
            .map_or(source, |(before, _)| before);

        let mut missing = Vec::new();
        let mut rest = source;
        while let Some(at) = rest.find("MALLARD_") {
            let tail = &rest[at + "MALLARD_".len()..];
            let len = tail
                .find(|c: char| !c.is_ascii_uppercase() && !c.is_ascii_digit() && c != '_')
                .unwrap_or(tail.len());
            // "MALLARD_*" in a doc comment names no variable; a real one always
            // has at least one character after the prefix.
            if len > 0 {
                let name = &rest[at..at + "MALLARD_".len() + len];
                if !example.contains(name) {
                    missing.push(name.to_string());
                }
            }
            rest = &tail[len..];
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "these environment variables are undocumented in \
             mallard-metrics.toml.example: {missing:?}"
        );
    }

    #[test]
    fn test_advisory_warns_about_daily_salt_rotation() {
        let config = Config::default();
        assert!(
            config
                .advisories()
                .iter()
                .any(|a| a.contains("visitor_salt_rotation_days=1")),
            "the default rotation has analytical consequences and must be surfaced"
        );
    }

    #[test]
    fn test_advisory_warns_about_trusted_proxy_headers() {
        let config = Config {
            trust_proxy_headers: true,
            ..Config::default()
        };
        assert!(
            config
                .advisories()
                .iter()
                .any(|a| a.contains("trust_proxy_headers"))
        );
    }

    #[test]
    fn test_advisory_warns_about_gdpr_without_retention() {
        let config = Config {
            gdpr_mode: true,
            retention_days: 0,
            ..Config::default()
        };
        assert!(config.advisories().iter().any(|a| a.contains("gdpr_mode")));
    }
}
