//! Configuration for webhook alerts and scheduled reports.
//!
//! Lives in its own table, `[alerts]`, with `MALLARD_ALERTS_*` variables. The
//! URL and the signing secret are read from configuration only: no API request
//! can set or change where notifications go.

use super::payload::PayloadFormat;
use super::policy::Target;
use serde::Deserialize;
use std::path::PathBuf;

/// Shortest accepted signing secret, matching `MALLARD_SECRET`.
pub const MIN_SECRET_CHARS: usize = 16;

/// The signing secret. `Debug` never shows it.
#[derive(Clone)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(value: &str) -> Self {
        Self(value.as_bytes().to_vec())
    }

    /// The key bytes, for HMAC only.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// Weekday a weekly report goes out on.
fn parse_weekday(raw: &str) -> Option<chrono::Weekday> {
    raw.trim().to_ascii_lowercase().parse().ok()
}

/// Settings from the `[alerts]` table and `MALLARD_ALERTS_*`.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(clippy::struct_excessive_bools)]
pub struct AlertSettings {
    /// Where notifications are POSTed. Unset disables the whole feature.
    /// Treated as a secret (Slack and Discord URLs are bearer tokens).
    pub webhook_url: Option<String>,
    /// Environment only (`MALLARD_ALERTS_WEBHOOK_SECRET`): never read from TOML.
    #[serde(skip)]
    pub webhook_secret: Option<Secret>,
    /// Permit `http://`. Off by default.
    pub allow_http_webhook: bool,
    /// Permit loopback, private, link-local and other internal destinations.
    pub allow_private_webhook_targets: bool,
    /// PEM file of extra CA certificates to trust (a private CA).
    pub webhook_ca_file: Option<PathBuf>,
    /// `generic`, `slack` or `discord`.
    pub payload_format: String,
    /// Total time allowed for one delivery attempt, in seconds.
    pub timeout_secs: u64,
    /// Attempts per notification, including the first.
    pub max_attempts: u32,
    /// Sites the analytical alerts and reports cover. Default: `site_ids`.
    pub sites: Vec<String>,
    /// How often conditions are evaluated, in seconds.
    pub check_interval_secs: u64,
    /// Minimum seconds between repeats of an alert that stays firing.
    pub renotify_secs: u64,
    /// Operational alerts on or off.
    pub operational: bool,
    /// No successful flush for this long is `flush_stalled`.
    pub flush_stalled_secs: u64,
    /// Window for "buffer failing" and "rate limiter evictions spiking".
    pub failure_window_secs: u64,
    /// Rate-limiter evictions within the window that count as a spike. 0 disables.
    pub evictions_spike_threshold: u64,
    /// Seconds after start before a missing behavioral extension is an alert.
    pub behavioral_grace_secs: u64,
    /// Alert when an hour's pageviews fall at least this many percent below
    /// the same hour of the previous weeks. 0 disables.
    pub traffic_drop_percent: u32,
    /// Alert when they rise at least this many percent above. 0 disables.
    pub traffic_rise_percent: u32,
    /// Weeks of history averaged for the baseline.
    pub traffic_baseline_weeks: u32,
    /// The baseline hour must average at least this many pageviews, or the
    /// site is too quiet to judge.
    pub traffic_min_pageviews: u64,
    /// Send a daily report.
    pub daily_report: bool,
    /// Send a weekly report.
    pub weekly_report: bool,
    /// UTC hour (0-23) reports are sent after.
    pub report_hour_utc: u32,
    /// Weekday of the weekly report.
    pub weekly_report_day: String,
}

impl Default for AlertSettings {
    fn default() -> Self {
        Self {
            webhook_url: None,
            webhook_secret: None,
            allow_http_webhook: false,
            allow_private_webhook_targets: false,
            webhook_ca_file: None,
            payload_format: "generic".to_string(),
            timeout_secs: 10,
            max_attempts: 4,
            sites: Vec::new(),
            check_interval_secs: 60,
            renotify_secs: 4 * 3600,
            operational: true,
            flush_stalled_secs: 600,
            failure_window_secs: 300,
            evictions_spike_threshold: 100,
            behavioral_grace_secs: 600,
            traffic_drop_percent: 50,
            traffic_rise_percent: 0,
            traffic_baseline_weeks: 4,
            traffic_min_pageviews: 50,
            daily_report: false,
            weekly_report: false,
            report_hour_utc: 8,
            weekly_report_day: "monday".to_string(),
        }
    }
}

impl std::fmt::Debug for AlertSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let url = self.webhook_url.as_deref().map_or_else(
            || "unset".to_string(),
            |u| Target::parse(u).map_or_else(|_| "<invalid>".to_string(), |t| t.display()),
        );
        f.debug_struct("AlertSettings")
            .field("webhook_url", &url)
            .field(
                "webhook_secret",
                &self.webhook_secret.as_ref().map(|_| "[redacted]"),
            )
            .field("allow_http_webhook", &self.allow_http_webhook)
            .field(
                "allow_private_webhook_targets",
                &self.allow_private_webhook_targets,
            )
            .finish_non_exhaustive()
    }
}

impl AlertSettings {
    /// Whether the feature is on.
    pub const fn enabled(&self) -> bool {
        self.webhook_url.is_some()
    }

    /// The parsed destination.
    ///
    /// # Errors
    ///
    /// Does not repeat the URL.
    pub fn target(&self) -> Result<Target, String> {
        let url = self
            .webhook_url
            .as_deref()
            .ok_or_else(|| "no webhook URL is configured".to_string())?;
        Target::parse(url)
    }

    /// The body encoding, defaulting to generic for an unparsable value
    /// (`validate` has already refused one).
    pub fn format(&self) -> PayloadFormat {
        PayloadFormat::parse(&self.payload_format).unwrap_or_default()
    }

    /// The weekday for weekly reports.
    pub fn report_weekday(&self) -> chrono::Weekday {
        parse_weekday(&self.weekly_report_day).unwrap_or(chrono::Weekday::Mon)
    }

    /// Apply `MALLARD_ALERTS_*` over the TOML values. Malformed values are
    /// warnings and leave the previous value, like every other setting.
    #[allow(clippy::too_many_lines)]
    pub fn apply_env(&mut self, warnings: &mut Vec<String>) {
        macro_rules! num {
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
        macro_rules! flag {
            ($var:literal, $field:expr) => {
                if let Ok(raw) = std::env::var($var) {
                    match raw.trim().to_ascii_lowercase().as_str() {
                        "1" | "true" | "yes" | "on" => $field = true,
                        "0" | "false" | "no" | "off" => $field = false,
                        _ => warnings.push(format!(
                            "{}={raw:?} is not a boolean (use true/false); keeping {}",
                            $var, $field
                        )),
                    }
                }
            };
        }
        // An empty value means "not set", as compose files pass `${VAR:-}`.
        if let Ok(url) = std::env::var("MALLARD_ALERTS_WEBHOOK_URL") {
            let url = url.trim().to_string();
            self.webhook_url = (!url.is_empty()).then_some(url);
        }
        if let Ok(secret) = std::env::var("MALLARD_ALERTS_WEBHOOK_SECRET") {
            let secret = secret.trim();
            self.webhook_secret = (!secret.is_empty()).then(|| Secret::new(secret));
        }
        flag!("MALLARD_ALERTS_ALLOW_HTTP_WEBHOOK", self.allow_http_webhook);
        flag!(
            "MALLARD_ALERTS_ALLOW_PRIVATE_WEBHOOK_TARGETS",
            self.allow_private_webhook_targets
        );
        if let Ok(path) = std::env::var("MALLARD_ALERTS_WEBHOOK_CA_FILE") {
            self.webhook_ca_file = Some(PathBuf::from(path)).filter(|p| !p.as_os_str().is_empty());
        }
        if let Ok(v) = std::env::var("MALLARD_ALERTS_PAYLOAD_FORMAT") {
            self.payload_format = v.trim().to_string();
        }
        num!("MALLARD_ALERTS_TIMEOUT_SECS", self.timeout_secs, u64);
        num!("MALLARD_ALERTS_MAX_ATTEMPTS", self.max_attempts, u32);
        if let Ok(sites) = std::env::var("MALLARD_ALERTS_SITES") {
            self.sites = sites
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        num!(
            "MALLARD_ALERTS_CHECK_INTERVAL_SECS",
            self.check_interval_secs,
            u64
        );
        num!("MALLARD_ALERTS_RENOTIFY_SECS", self.renotify_secs, u64);
        flag!("MALLARD_ALERTS_OPERATIONAL", self.operational);
        num!(
            "MALLARD_ALERTS_FLUSH_STALLED_SECS",
            self.flush_stalled_secs,
            u64
        );
        num!(
            "MALLARD_ALERTS_FAILURE_WINDOW_SECS",
            self.failure_window_secs,
            u64
        );
        num!(
            "MALLARD_ALERTS_EVICTIONS_SPIKE_THRESHOLD",
            self.evictions_spike_threshold,
            u64
        );
        num!(
            "MALLARD_ALERTS_BEHAVIORAL_GRACE_SECS",
            self.behavioral_grace_secs,
            u64
        );
        num!(
            "MALLARD_ALERTS_TRAFFIC_DROP_PERCENT",
            self.traffic_drop_percent,
            u32
        );
        num!(
            "MALLARD_ALERTS_TRAFFIC_RISE_PERCENT",
            self.traffic_rise_percent,
            u32
        );
        num!(
            "MALLARD_ALERTS_TRAFFIC_BASELINE_WEEKS",
            self.traffic_baseline_weeks,
            u32
        );
        num!(
            "MALLARD_ALERTS_TRAFFIC_MIN_PAGEVIEWS",
            self.traffic_min_pageviews,
            u64
        );
        flag!("MALLARD_ALERTS_DAILY_REPORT", self.daily_report);
        flag!("MALLARD_ALERTS_WEEKLY_REPORT", self.weekly_report);
        num!("MALLARD_ALERTS_REPORT_HOUR_UTC", self.report_hour_utc, u32);
        if let Ok(v) = std::env::var("MALLARD_ALERTS_WEEKLY_REPORT_DAY") {
            self.weekly_report_day = v.trim().to_string();
        }
    }

    /// Check the settings for consistency and for insecure combinations.
    ///
    /// Nothing is validated when no URL is set, so a stray tuning variable
    /// cannot stop a deployment that does not use the feature.
    ///
    /// # Errors
    ///
    /// A message naming the setting; it never contains the URL or the secret.
    pub fn validate(&self) -> Result<(), String> {
        if self.webhook_url.is_none() {
            return Ok(());
        }
        let target = self
            .target()
            .map_err(|e| format!("alerts.webhook_url: {e}"))?;
        if !target.https && !self.allow_http_webhook {
            return Err(
                "alerts.webhook_url uses http://, which sends alerts and report \
                        contents unencrypted. Use https://, or set \
                        allow_http_webhook = true (MALLARD_ALERTS_ALLOW_HTTP_WEBHOOK) if the \
                        path to the receiver is trusted"
                    .to_string(),
            );
        }
        if !self.allow_private_webhook_targets
            && let Some(why) = target.static_refusal()
        {
            return Err(format!(
                "alerts.webhook_url: {why}. Outbound webhooks may not reach internal \
                 addresses unless allow_private_webhook_targets = true \
                 (MALLARD_ALERTS_ALLOW_PRIVATE_WEBHOOK_TARGETS) is set; that is only \
                 appropriate when the receiver really is on your own network"
            ));
        }
        if let Some(secret) = &self.webhook_secret
            && String::from_utf8_lossy(secret.expose()).chars().count() < MIN_SECRET_CHARS
        {
            return Err(format!(
                "MALLARD_ALERTS_WEBHOOK_SECRET is shorter than {MIN_SECRET_CHARS} characters. \
                 Use a random value, e.g. `openssl rand -hex 32`"
            ));
        }
        if PayloadFormat::parse(&self.payload_format).is_none() {
            return Err(format!(
                "alerts.payload_format must be generic, slack or discord (got {:?})",
                self.payload_format
            ));
        }
        range("alerts.timeout_secs", self.timeout_secs, 1, 60)?;
        range("alerts.max_attempts", u64::from(self.max_attempts), 1, 10)?;
        range(
            "alerts.check_interval_secs",
            self.check_interval_secs,
            10,
            3600,
        )?;
        range("alerts.renotify_secs", self.renotify_secs, 60, 30 * 86_400)?;
        range(
            "alerts.flush_stalled_secs",
            self.flush_stalled_secs,
            60,
            86_400,
        )?;
        range(
            "alerts.failure_window_secs",
            self.failure_window_secs,
            60,
            86_400,
        )?;
        range(
            "alerts.traffic_baseline_weeks",
            u64::from(self.traffic_baseline_weeks),
            1,
            12,
        )?;
        range(
            "alerts.traffic_drop_percent",
            u64::from(self.traffic_drop_percent),
            0,
            100,
        )?;
        range(
            "alerts.traffic_rise_percent",
            u64::from(self.traffic_rise_percent),
            0,
            100_000,
        )?;
        range(
            "alerts.report_hour_utc",
            u64::from(self.report_hour_utc),
            0,
            23,
        )?;
        if parse_weekday(&self.weekly_report_day).is_none() {
            return Err(format!(
                "alerts.weekly_report_day must be a weekday name such as \"monday\" (got {:?})",
                self.weekly_report_day
            ));
        }
        for site in &self.sites {
            crate::api::stats::validate_site_id(site)
                .map_err(|_| format!("alerts.sites entry {site:?} is not a valid site ID"))?;
        }
        if let Some(path) = &self.webhook_ca_file {
            super::client::build_tls_config(Some(path))?;
        }
        Ok(())
    }

    /// Warnings for valid but surprising combinations.
    pub fn advisories(&self, site_ids: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        if self.webhook_url.is_none() {
            if self.webhook_secret.is_some() || self.daily_report || self.weekly_report {
                out.push(
                    "alerts: webhook settings are present but MALLARD_ALERTS_WEBHOOK_URL is not \
                     set, so no notifications will be sent"
                        .to_string(),
                );
            }
            return out;
        }
        if self.webhook_secret.is_none() {
            out.push(
                "alerts: MALLARD_ALERTS_WEBHOOK_SECRET is not set, so notifications are \
                 unsigned and the receiver cannot tell them from forged ones"
                    .to_string(),
            );
        }
        if self.allow_private_webhook_targets {
            out.push(
                "alerts: allow_private_webhook_targets is on; the webhook may point at \
                 loopback, private and link-local addresses, including cloud metadata \
                 endpoints. Anyone who can change this configuration can aim this server at \
                 your internal network"
                    .to_string(),
            );
        }
        if self.allow_http_webhook {
            out.push(
                "alerts: allow_http_webhook is on; notifications travel unencrypted".to_string(),
            );
        }
        let sites = self.effective_sites(site_ids);
        if sites.is_empty()
            && (self.traffic_drop_percent > 0
                || self.traffic_rise_percent > 0
                || self.daily_report
                || self.weekly_report)
        {
            out.push(
                "alerts: no sites are configured (alerts.sites and site_ids are both empty), \
                 so traffic alerts and reports have nothing to cover; only operational alerts \
                 will be sent"
                    .to_string(),
            );
        }
        out
    }

    /// The sites analytical alerts and reports cover.
    pub fn effective_sites(&self, site_ids: &[String]) -> Vec<String> {
        if self.sites.is_empty() {
            site_ids.to_vec()
        } else {
            self.sites.clone()
        }
    }
}

fn range(name: &str, value: u64, lo: u64, hi: u64) -> Result<(), String> {
    if (lo..=hi).contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "{name} must be between {lo} and {hi} (got {value})"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_url(url: &str) -> AlertSettings {
        AlertSettings {
            webhook_url: Some(url.to_string()),
            ..AlertSettings::default()
        }
    }

    #[test]
    fn disabled_settings_validate_whatever_else_is_set() {
        let s = AlertSettings {
            timeout_secs: 0,
            ..AlertSettings::default()
        };
        assert!(s.validate().is_ok());
        assert!(!s.enabled());
    }

    #[test]
    fn https_public_url_is_valid() {
        assert!(with_url("https://hooks.example.com/x").validate().is_ok());
    }

    #[test]
    fn http_needs_the_flag() {
        let err = with_url("http://hooks.example.com/x")
            .validate()
            .unwrap_err();
        assert!(err.contains("allow_http_webhook"), "{err}");
        let mut s = with_url("http://hooks.example.com/x");
        s.allow_http_webhook = true;
        assert!(s.validate().is_ok());
    }

    #[test]
    fn internal_literals_and_localhost_need_the_flag() {
        for url in [
            "https://127.0.0.1/x",
            "https://10.0.0.5/x",
            "https://169.254.169.254/latest/meta-data",
            "https://[fd00:ec2::254]/",
            "https://100.100.100.200/",
            "https://localhost:8443/x",
        ] {
            let err = with_url(url).validate().unwrap_err();
            assert!(
                err.contains("allow_private_webhook_targets"),
                "{url}: {err}"
            );
            assert!(
                !err.contains("169.254.169.254/latest"),
                "echoes the URL: {err}"
            );
            let mut s = with_url(url);
            s.allow_private_webhook_targets = true;
            assert!(s.validate().is_ok(), "{url}");
        }
    }

    #[test]
    fn credentials_in_the_url_are_refused_without_echo() {
        let err = with_url("https://user:hunter2hunter2@hooks.example.com/")
            .validate()
            .unwrap_err();
        assert!(!err.contains("hunter2"), "{err}");
    }

    #[test]
    fn a_short_secret_is_refused_without_echo() {
        let mut s = with_url("https://hooks.example.com/x");
        s.webhook_secret = Some(Secret::new("short-secret"));
        let err = s.validate().unwrap_err();
        assert!(err.contains("MALLARD_ALERTS_WEBHOOK_SECRET"), "{err}");
        assert!(!err.contains("short-secret"));
    }

    #[test]
    fn ranges_are_enforced() {
        for tweak in [
            |s: &mut AlertSettings| s.timeout_secs = 0,
            |s: &mut AlertSettings| s.timeout_secs = 61,
            |s: &mut AlertSettings| s.max_attempts = 0,
            |s: &mut AlertSettings| s.check_interval_secs = 5,
            |s: &mut AlertSettings| s.renotify_secs = 10,
            |s: &mut AlertSettings| s.traffic_baseline_weeks = 0,
            |s: &mut AlertSettings| s.traffic_drop_percent = 101,
            |s: &mut AlertSettings| s.report_hour_utc = 24,
            |s: &mut AlertSettings| s.weekly_report_day = "someday".into(),
            |s: &mut AlertSettings| s.payload_format = "teams".into(),
            |s: &mut AlertSettings| s.sites = vec!["bad site".into()],
        ] {
            let mut s = with_url("https://hooks.example.com/x");
            tweak(&mut s);
            assert!(s.validate().is_err());
        }
    }

    #[test]
    fn debug_shows_neither_url_path_nor_secret() {
        let mut s = with_url("https://hooks.example.com/services/TOKENTOKEN");
        s.webhook_secret = Some(Secret::new("0123456789abcdef-secret"));
        let shown = format!("{s:?}");
        assert!(shown.contains("hooks.example.com"));
        assert!(!shown.contains("TOKENTOKEN"));
        assert!(!shown.contains("0123456789abcdef"));
    }

    #[test]
    fn config_validation_runs_the_alerts_checks_and_the_toml_table_parses() {
        let config: crate::config::Config = toml::from_str(
            "[alerts]\nwebhook_url = \"http://hooks.example.com/x\"\ndaily_report = true\n",
        )
        .unwrap();
        assert!(config.alerts.daily_report);
        let err = config.validate().unwrap_err();
        assert!(err.contains("allow_http_webhook"), "{err}");
        let mut ok = config;
        ok.alerts.allow_http_webhook = true;
        assert!(ok.validate().is_ok());
        assert!(ok.advisories().iter().any(|a| a.contains("unsigned")));
        assert!(format!("{ok:?}").contains("hooks.example.com"));
        assert!(
            !format!("{ok:?}").contains("/x\""),
            "path must not appear in Config's Debug"
        );
    }

    #[test]
    fn toml_rejects_a_secret_key() {
        let toml = "webhook_secret = \"0123456789abcdef0123\"";
        assert!(toml::from_str::<AlertSettings>(toml).is_err());
    }

    #[test]
    fn environment_overrides_and_warns() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsafe {
            std::env::set_var("MALLARD_ALERTS_WEBHOOK_URL", "https://hooks.example.com/a");
            std::env::set_var("MALLARD_ALERTS_WEBHOOK_SECRET", "0123456789abcdef0123");
            std::env::set_var("MALLARD_ALERTS_TIMEOUT_SECS", "7");
            std::env::set_var("MALLARD_ALERTS_MAX_ATTEMPTS", "many");
            std::env::set_var("MALLARD_ALERTS_DAILY_REPORT", "yes");
            std::env::set_var("MALLARD_ALERTS_SITES", "a.example, b.example,");
        }
        let mut s = AlertSettings::default();
        let mut warnings = Vec::new();
        s.apply_env(&mut warnings);
        for var in [
            "MALLARD_ALERTS_WEBHOOK_URL",
            "MALLARD_ALERTS_WEBHOOK_SECRET",
            "MALLARD_ALERTS_TIMEOUT_SECS",
            "MALLARD_ALERTS_MAX_ATTEMPTS",
            "MALLARD_ALERTS_DAILY_REPORT",
            "MALLARD_ALERTS_SITES",
        ] {
            unsafe { std::env::remove_var(var) };
        }
        assert!(s.enabled());
        assert!(s.webhook_secret.is_some());
        assert_eq!(s.timeout_secs, 7);
        assert_eq!(s.max_attempts, 4, "bad value keeps the default");
        assert!(s.daily_report);
        assert_eq!(s.sites, vec!["a.example", "b.example"]);
        assert_eq!(warnings.len(), 1);
        assert!(!warnings[0].contains("hooks.example"));
    }

    #[test]
    fn example_config_documents_every_alerts_variable() {
        let source = include_str!("settings.rs");
        let source = source
            .split_once("\nmod tests {")
            .map_or(source, |(a, _)| a);
        let example = include_str!("../../mallard-metrics.toml.example");
        let mut missing = Vec::new();
        let mut rest = source;
        while let Some(at) = rest.find("MALLARD_ALERTS_") {
            let tail = &rest[at..];
            let end = tail
                .find(|c: char| !(c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit()))
                .unwrap_or(tail.len());
            let name = &tail[..end];
            if name.len() > "MALLARD_ALERTS_".len() && !example.contains(name) {
                missing.push(name.to_string());
            }
            rest = &tail[end..];
        }
        assert!(
            missing.is_empty(),
            "missing from mallard-metrics.toml.example: {missing:?}"
        );
    }
}
