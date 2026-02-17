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
    #[allow(dead_code)]
    pub site_ids: Vec<String>,
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

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            data_dir: default_data_dir(),
            flush_event_count: default_flush_count(),
            flush_interval_secs: default_flush_interval_secs(),
            site_ids: Vec::new(),
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
