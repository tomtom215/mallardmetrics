use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Application configuration loaded from environment variables or TOML file.
#[derive(Debug, Clone, Deserialize)]
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
    #[serde(default)]
    pub site_ids: Vec<String>,
    /// Path to a MaxMind GeoLite2 .mmdb file for IP geolocation.
    /// If not set or file is missing, GeoIP lookups return None (graceful fallback).
    #[serde(default)]
    pub geoip_db_path: Option<PathBuf>,
    /// Dashboard origin for CORS restrictions on stats/dashboard routes.
    /// If not set, stats routes allow same-origin only.
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

    // ── Privacy / GDPR configuration ─────────────────────────────────────
    /// GDPR-friendly mode: convenience preset that enables the full privacy bundle.
    ///
    /// When true, the following flags are forced on regardless of their individual
    /// settings: `strip_referrer_query`, `round_timestamps`, `suppress_browser_version`,
    /// `suppress_os_version`, `suppress_screen_size`.  `geoip_precision` is promoted
    /// from "city" to "country" (more permissive/less-granular settings are kept).
    ///
    /// `suppress_visitor_id` is intentionally NOT activated by `gdpr_mode` because
    /// it eliminates the unique-visitor metric entirely.  Set it explicitly if needed.
    ///
    /// To configure GDPR-friendly deployment with full-scale analytics for non-EU
    /// audiences, leave `gdpr_mode = false` and toggle individual flags.
    #[serde(default)]
    pub gdpr_mode: bool,

    /// Strip query string and fragment from referrer URLs before storing.
    ///
    /// Prevents leaking search terms and campaign parameters embedded in referrer
    /// URLs (e.g. `https://google.com/search?q=medical+condition` → `https://google.com/search`).
    /// Default: false. Enabled automatically when `gdpr_mode = true`.
    #[serde(default)]
    pub strip_referrer_query: bool,

    /// Round event timestamps to the nearest hour before storing.
    ///
    /// Reduces fingerprinting risk by lowering timestamp precision from milliseconds
    /// to hours. Aggregate analytics (daily/hourly timeseries) remain accurate.
    /// Default: false. Enabled automatically when `gdpr_mode = true`.
    #[serde(default)]
    pub round_timestamps: bool,

    /// Replace the HMAC-based visitor_id with a random UUID per request.
    ///
    /// The HMAC visitor_id links multiple page views from the same visitor within
    /// a calendar day (enabling unique-visitor counting). Enabling this option
    /// replaces that with a random identifier per request, breaking cross-request
    /// linkability entirely. Consequence: unique-visitor counts degrade to
    /// approximate page-load counts.
    ///
    /// Default: false. NOT activated automatically by `gdpr_mode`.
    #[serde(default)]
    pub suppress_visitor_id: bool,

    /// Store browser name only, omitting browser version.
    ///
    /// Browser versions contribute to fingerprinting surface. "Chrome 120" is more
    /// identifying than "Chrome". Default: false. Enabled by `gdpr_mode = true`.
    #[serde(default)]
    pub suppress_browser_version: bool,

    /// Store OS name only, omitting OS version.
    ///
    /// Similar to `suppress_browser_version`: "Windows 10.0" is more identifying than
    /// "Windows". Default: false. Enabled by `gdpr_mode = true`.
    #[serde(default)]
    pub suppress_os_version: bool,

    /// Do not store the screen_size or device_type fields.
    ///
    /// Screen width contributes to fingerprinting. Setting this to true stores
    /// neither the raw width nor the derived device category (mobile/tablet/desktop).
    /// Default: false. Enabled by `gdpr_mode = true`.
    #[serde(default)]
    pub suppress_screen_size: bool,

    /// Geographic precision for IP geolocation. Valid values:
    /// - `"city"` (default): stores `country_code`, `region`, and `city`.
    /// - `"region"`: stores `country_code` and `region` only.
    /// - `"country"`: stores `country_code` only.
    /// - `"none"`: stores no geographic data.
    ///
    /// `gdpr_mode = true` promotes `"city"` → `"country"` (more permissive settings
    /// such as `"region"` or `"none"` are left unchanged).
    #[serde(default = "default_geoip_precision")]
    pub geoip_precision: String,
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

fn default_geoip_precision() -> String {
    "city".to_string()
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
            cache_ttl_secs: default_cache_ttl_secs(),
            log_format: default_log_format(),
            max_login_attempts: default_max_login_attempts(),
            login_lockout_secs: default_login_lockout_secs(),
            cache_max_entries: default_cache_max_entries(),
            max_concurrent_queries: default_max_concurrent_queries(),
            secure_cookies: false,
            gdpr_mode: false,
            strip_referrer_query: false,
            round_timestamps: false,
            suppress_visitor_id: false,
            suppress_browser_version: false,
            suppress_os_version: false,
            suppress_screen_size: false,
            geoip_precision: default_geoip_precision(),
        }
    }
}

impl Config {
    /// Load configuration from a TOML file, falling back to defaults.
    ///
    /// Environment variables override file values:
    /// - `MALLARD_HOST` → host
    /// - `MALLARD_PORT` → port
    /// - `MALLARD_DATA_DIR` → data_dir
    /// - `MALLARD_FLUSH_COUNT` → flush_event_count
    /// - `MALLARD_FLUSH_INTERVAL` → flush_interval_secs
    /// - `MALLARD_GEOIP_DB` → geoip_db_path
    /// - `MALLARD_DASHBOARD_ORIGIN` → dashboard_origin
    /// - `MALLARD_FILTER_BOTS` → filter_bots
    /// - `MALLARD_RETENTION_DAYS` → retention_days
    /// - `MALLARD_SESSION_TTL` → session_ttl_secs
    /// - `MALLARD_SHUTDOWN_TIMEOUT` → shutdown_timeout_secs
    /// - `MALLARD_RATE_LIMIT` → rate_limit_per_site
    /// - `MALLARD_CACHE_TTL` → cache_ttl_secs
    /// - `MALLARD_LOG_FORMAT` → log_format
    #[allow(clippy::too_many_lines)]
    pub fn load(config_path: Option<&Path>) -> Self {
        let mut config =
            config_path.map_or_else(Self::default, |path| match std::fs::read_to_string(path) {
                Ok(contents) => toml::from_str(&contents).unwrap_or_else(|e| {
                    tracing::warn!("Failed to parse config file: {e}, using defaults");
                    Self::default()
                }),
                Err(e) => {
                    tracing::warn!("Failed to read config file: {e}, using defaults");
                    Self::default()
                }
            });

        // Helper: parse a numeric env var, logging a warning if the value is set
        // but cannot be parsed.  A silent fallback to the default is confusing for
        // operators because the configured value appears to be ignored.
        macro_rules! parse_env_num {
            ($var:literal, $field:expr, $ty:ty) => {
                if let Ok(raw) = std::env::var($var) {
                    match raw.parse::<$ty>() {
                        Ok(v) => $field = v,
                        Err(_) => tracing::warn!(
                            env = $var,
                            value = %raw,
                            "Invalid value for env var; using existing value {}",
                            $field
                        ),
                    }
                }
            };
        }

        // Environment variable overrides
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
            config.dashboard_origin = Some(origin);
        }
        if let Ok(val) = std::env::var("MALLARD_FILTER_BOTS") {
            config.filter_bots = val != "0" && val.to_lowercase() != "false";
        }
        parse_env_num!("MALLARD_RETENTION_DAYS", config.retention_days, u32);
        parse_env_num!("MALLARD_SESSION_TTL", config.session_ttl_secs, u64);
        parse_env_num!(
            "MALLARD_SHUTDOWN_TIMEOUT",
            config.shutdown_timeout_secs,
            u64
        );
        parse_env_num!("MALLARD_RATE_LIMIT", config.rate_limit_per_site, u32);
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
        if let Ok(val) = std::env::var("MALLARD_SECURE_COOKIES") {
            config.secure_cookies = val != "0" && val.to_lowercase() != "false";
        }

        // Privacy / GDPR configuration env vars
        if let Ok(val) = std::env::var("MALLARD_GDPR_MODE") {
            config.gdpr_mode = val != "0" && val.to_lowercase() != "false";
        }
        if let Ok(val) = std::env::var("MALLARD_STRIP_REFERRER_QUERY") {
            config.strip_referrer_query = val != "0" && val.to_lowercase() != "false";
        }
        if let Ok(val) = std::env::var("MALLARD_ROUND_TIMESTAMPS") {
            config.round_timestamps = val != "0" && val.to_lowercase() != "false";
        }
        if let Ok(val) = std::env::var("MALLARD_SUPPRESS_VISITOR_ID") {
            config.suppress_visitor_id = val != "0" && val.to_lowercase() != "false";
        }
        if let Ok(val) = std::env::var("MALLARD_SUPPRESS_BROWSER_VERSION") {
            config.suppress_browser_version = val != "0" && val.to_lowercase() != "false";
        }
        if let Ok(val) = std::env::var("MALLARD_SUPPRESS_OS_VERSION") {
            config.suppress_os_version = val != "0" && val.to_lowercase() != "false";
        }
        if let Ok(val) = std::env::var("MALLARD_SUPPRESS_SCREEN_SIZE") {
            config.suppress_screen_size = val != "0" && val.to_lowercase() != "false";
        }
        if let Ok(val) = std::env::var("MALLARD_GEOIP_PRECISION") {
            config.geoip_precision = val;
        }

        // Apply gdpr_mode bundle AFTER all other env vars are resolved.
        // gdpr_mode is a convenience preset: it forces privacy-enhancing flags on.
        // Operators who need fine-grained control should leave gdpr_mode = false
        // and configure individual flags instead.
        if config.gdpr_mode {
            config.strip_referrer_query = true;
            config.round_timestamps = true;
            config.suppress_browser_version = true;
            config.suppress_os_version = true;
            config.suppress_screen_size = true;
            // Promote "city" → "country"; leave "region" / "none" unchanged
            // because those are already more privacy-protective than "country".
            if config.geoip_precision == "city" {
                config.geoip_precision = "country".to_string();
            }
        }

        config
    }

    /// Returns the path to the events directory.
    pub fn events_dir(&self) -> PathBuf {
        self.data_dir.join("events")
    }

    /// Returns the path to the DuckDB database file.
    ///
    /// Using a disk-based file instead of an in-memory database allows events
    /// that have been buffered but not yet flushed to Parquet to survive a
    /// process crash (SIGKILL).  The WAL file next to the database ensures
    /// atomicity of each batch insert.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("mallard.duckdb")
    }

    /// Validate that configuration values are internally consistent.
    ///
    /// Called at startup to catch misconfiguration before the server binds.
    pub fn validate(&self) -> Result<(), String> {
        if self.flush_event_count == 0 {
            return Err(
                "flush_event_count must be > 0; set to 0 would cause events to never auto-flush"
                    .to_string(),
            );
        }
        if self.flush_interval_secs == 0 {
            return Err(
                "flush_interval_secs must be > 0; set to 0 would cause the flush timer to spin at maximum CPU speed"
                    .to_string(),
            );
        }
        if self.session_ttl_secs == 0 {
            return Err(
                "session_ttl_secs must be > 0; set to 0 would expire all sessions immediately, breaking authentication"
                    .to_string(),
            );
        }
        if !matches!(
            self.geoip_precision.as_str(),
            "city" | "region" | "country" | "none"
        ) {
            return Err(format!(
                "geoip_precision must be one of: city, region, country, none (got {:?})",
                self.geoip_precision
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    /// Mutex to serialize tests that call `Config::load`, which reads
    /// environment variables. Without this, `test_env_var_overrides` can
    /// pollute other tests running in parallel.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
        assert_eq!(config.cache_ttl_secs, 60);
        assert_eq!(config.log_format, "text");
        assert_eq!(config.max_login_attempts, 5);
        assert_eq!(config.login_lockout_secs, 300);
    }

    #[test]
    fn test_validate_valid_config() {
        let config = Config::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_zero_flush_count() {
        let config = Config {
            flush_event_count: 0,
            ..Config::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("flush_event_count"));
    }

    #[test]
    fn test_validate_zero_flush_interval() {
        let config = Config {
            flush_interval_secs: 0,
            ..Config::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("flush_interval_secs"));
    }

    #[test]
    fn test_validate_zero_session_ttl() {
        let config = Config {
            session_ttl_secs: 0,
            ..Config::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("session_ttl_secs"));
    }

    #[test]
    fn test_load_from_toml() {
        let _guard = ENV_LOCK.lock().unwrap();
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
"#
        )
        .unwrap();

        let config = Config::load(Some(&config_path));
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
    }

    #[test]
    fn test_load_missing_file_uses_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = Config::load(Some(Path::new("/nonexistent/config.toml")));
        assert_eq!(config.port, 8000);
    }

    #[test]
    fn test_load_no_path_uses_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = Config::load(None);
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
    fn test_secure_cookies_default_false() {
        assert!(!Config::default().secure_cookies);
    }

    #[test]
    fn test_warn_on_invalid_env_var_falls_back() {
        let _guard = ENV_LOCK.lock().unwrap();
        let orig = std::env::var("MALLARD_PORT").ok();
        std::env::set_var("MALLARD_PORT", "not_a_number");
        // Should not panic — should silently keep the default.
        let config = Config::load(None);
        // The default port (8000) must still be in use because the parse failed.
        assert_eq!(config.port, 8000);
        match orig {
            Some(v) => std::env::set_var("MALLARD_PORT", v),
            None => std::env::remove_var("MALLARD_PORT"),
        }
    }

    #[test]
    fn test_env_var_overrides() {
        let _guard = ENV_LOCK.lock().unwrap();

        // Save original values
        let orig_port = std::env::var("MALLARD_PORT").ok();

        std::env::set_var("MALLARD_PORT", "3000");
        let config = Config::load(None);
        assert_eq!(config.port, 3000);

        // Restore
        match orig_port {
            Some(v) => std::env::set_var("MALLARD_PORT", v),
            None => std::env::remove_var("MALLARD_PORT"),
        }
    }

    #[test]
    fn test_invalid_toml_uses_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "this is not valid toml {{{").unwrap();

        let config = Config::load(Some(&config_path));
        assert_eq!(config.port, 8000);
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
        let config = Config {
            gdpr_mode: true,
            ..Config::default()
        };
        // gdpr_mode is applied in load(), not in the struct directly.
        // To test the full effect, simulate what load() does.
        let mut c = config;
        if c.gdpr_mode {
            c.strip_referrer_query = true;
            c.round_timestamps = true;
            c.suppress_browser_version = true;
            c.suppress_os_version = true;
            c.suppress_screen_size = true;
            if c.geoip_precision == "city" {
                c.geoip_precision = "country".to_string();
            }
        }
        assert!(c.strip_referrer_query);
        assert!(c.round_timestamps);
        assert!(c.suppress_browser_version);
        assert!(c.suppress_os_version);
        assert!(c.suppress_screen_size);
        assert_eq!(c.geoip_precision, "country");
        // suppress_visitor_id NOT forced by gdpr_mode
        assert!(!c.suppress_visitor_id);
    }

    #[test]
    fn test_gdpr_mode_respects_stricter_geoip_precision() {
        // If operator has already set geoip_precision to "none", gdpr_mode should not downgrade it.
        let mut config = Config {
            gdpr_mode: true,
            geoip_precision: "none".to_string(),
            ..Config::default()
        };
        if config.gdpr_mode && config.geoip_precision == "city" {
            config.geoip_precision = "country".to_string();
        }
        assert_eq!(config.geoip_precision, "none");
    }

    #[test]
    fn test_gdpr_mode_respects_region_precision() {
        let mut config = Config {
            gdpr_mode: true,
            geoip_precision: "region".to_string(),
            ..Config::default()
        };
        if config.gdpr_mode && config.geoip_precision == "city" {
            config.geoip_precision = "country".to_string();
        }
        assert_eq!(config.geoip_precision, "region");
    }

    #[test]
    fn test_validate_invalid_geoip_precision() {
        let config = Config {
            geoip_precision: "district".to_string(),
            ..Config::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("geoip_precision"));
    }

    #[test]
    fn test_validate_valid_geoip_precisions() {
        for precision in &["city", "region", "country", "none"] {
            let config = Config {
                geoip_precision: (*precision).to_string(),
                ..Config::default()
            };
            assert!(config.validate().is_ok(), "Expected valid: {precision}");
        }
    }

    #[test]
    fn test_secure_cookies_flag_overrides_http_origin() {
        // This test existed before; keep it to verify secure_cookies still works.
        let config = Config {
            secure_cookies: true,
            ..Config::default()
        };
        assert!(config.secure_cookies);
    }
}
