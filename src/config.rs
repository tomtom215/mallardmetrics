use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Application configuration loaded from environment variables or TOML file.
#[derive(Debug, Clone, Deserialize)]
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

        // Environment variable overrides
        if let Ok(host) = std::env::var("MALLARD_HOST") {
            config.host = host;
        }
        if let Ok(port) = std::env::var("MALLARD_PORT") {
            if let Ok(p) = port.parse() {
                config.port = p;
            }
        }
        if let Ok(data_dir) = std::env::var("MALLARD_DATA_DIR") {
            config.data_dir = PathBuf::from(data_dir);
        }
        if let Ok(count) = std::env::var("MALLARD_FLUSH_COUNT") {
            if let Ok(c) = count.parse() {
                config.flush_event_count = c;
            }
        }
        if let Ok(interval) = std::env::var("MALLARD_FLUSH_INTERVAL") {
            if let Ok(i) = interval.parse() {
                config.flush_interval_secs = i;
            }
        }
        if let Ok(geoip) = std::env::var("MALLARD_GEOIP_DB") {
            config.geoip_db_path = Some(PathBuf::from(geoip));
        }
        if let Ok(origin) = std::env::var("MALLARD_DASHBOARD_ORIGIN") {
            config.dashboard_origin = Some(origin);
        }
        if let Ok(val) = std::env::var("MALLARD_FILTER_BOTS") {
            config.filter_bots = val != "0" && val.to_lowercase() != "false";
        }
        if let Ok(val) = std::env::var("MALLARD_RETENTION_DAYS") {
            if let Ok(d) = val.parse() {
                config.retention_days = d;
            }
        }
        if let Ok(val) = std::env::var("MALLARD_SESSION_TTL") {
            if let Ok(t) = val.parse() {
                config.session_ttl_secs = t;
            }
        }
        if let Ok(val) = std::env::var("MALLARD_SHUTDOWN_TIMEOUT") {
            if let Ok(t) = val.parse() {
                config.shutdown_timeout_secs = t;
            }
        }
        if let Ok(val) = std::env::var("MALLARD_RATE_LIMIT") {
            if let Ok(r) = val.parse() {
                config.rate_limit_per_site = r;
            }
        }
        if let Ok(val) = std::env::var("MALLARD_CACHE_TTL") {
            if let Ok(t) = val.parse() {
                config.cache_ttl_secs = t;
            }
        }

        config
    }

    /// Returns the path to the events directory.
    pub fn events_dir(&self) -> PathBuf {
        self.data_dir.join("events")
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
}
