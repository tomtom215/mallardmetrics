//! Webhook alerts and scheduled reports.
//!
//! One evaluator task looks at the server every `check_interval_secs`, turns
//! what it sees into [`Notification`]s, and hands them to a bounded queue; one
//! worker task delivers them. Neither ever blocks request handling, and both
//! stop promptly at shutdown. See `docs/src/alerts-and-reports.md`.
//!
//! * [`policy`]: which destinations may be contacted (SSRF).
//! * [`client`]: the outbound HTTP/1.1 client, with the connect-time checks.
//! * [`notifier`]: queue, retries, backoff.
//! * [`sign`]: HMAC signature of each request.
//! * [`ops`], [`traffic`], [`report`]: what is evaluated.
//! * [`rules`]: cooldown and "resolved" bookkeeping.

pub mod client;
pub mod metrics;
pub mod notifier;
pub mod ops;
pub mod payload;
pub mod policy;
pub mod report;
pub mod rules;
pub mod settings;
pub mod sign;
pub mod traffic;

#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod tests;

pub use metrics::{AlertMetrics, Kind};
pub use settings::{AlertSettings, Secret};

use crate::ingest::handler::AppState;
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, NaiveDateTime, Utc};
use notifier::{Deliverer, DeliveryPolicy, Notifier};
use payload::Notification;
use report::Cadence;
use rules::{AlertState, Observation};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Where "now" comes from. The evaluator and the signer both read it, so a
/// test controls cooldowns, report schedules and signature timestamps.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that only moves when told to.
#[cfg(test)]
pub struct ManualClock(parking_lot::Mutex<DateTime<Utc>>);

#[cfg(test)]
impl ManualClock {
    pub fn new(at: DateTime<Utc>) -> Arc<Self> {
        Arc::new(Self(parking_lot::Mutex::new(at)))
    }

    pub fn set(&self, at: DateTime<Utc>) {
        *self.0.lock() = at;
    }

    pub fn advance(&self, by: ChronoDuration) {
        *self.0.lock() += by;
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock()
    }
}

/// Everything the evaluator needs, resolved from [`AlertSettings`].
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub operational: bool,
    pub ops: ops::Params,
    /// `None` when both traffic thresholds are 0.
    pub traffic: Option<traffic::Params>,
    pub baseline_weeks: u32,
    pub sites: Vec<String>,
    pub daily_report: bool,
    pub weekly_report: bool,
    pub report_hour: u32,
    pub report_weekday: chrono::Weekday,
    pub renotify: Duration,
    pub check_interval: Duration,
    /// After an hour ends, wait this long for its events to be flushed.
    pub flush_grace: ChronoDuration,
}

/// How many times a failing report is retried (one try per evaluation pass,
/// spaced by [`REPORT_RETRY`]) before that period is skipped.
const REPORT_ATTEMPTS: u32 = 5;
const REPORT_RETRY: ChronoDuration = ChronoDuration::minutes(5);

impl EngineConfig {
    pub fn from_settings(s: &AlertSettings, site_ids: &[String], flush_interval_secs: u64) -> Self {
        let secs = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        let traffic =
            (s.traffic_drop_percent > 0 || s.traffic_rise_percent > 0).then_some(traffic::Params {
                drop_percent: s.traffic_drop_percent,
                rise_percent: s.traffic_rise_percent,
                min_pageviews: s.traffic_min_pageviews,
            });
        // Events become visible at the next flush; wait two intervals, never
        // less than two minutes.
        let grace = secs(flush_interval_secs.saturating_mul(2)).max(120);
        Self {
            operational: s.operational,
            ops: ops::Params {
                flush_stalled_secs: secs(s.flush_stalled_secs),
                failure_window_secs: secs(s.failure_window_secs),
                evictions_threshold: s.evictions_spike_threshold,
                behavioral_grace_secs: secs(s.behavioral_grace_secs),
            },
            traffic,
            baseline_weeks: s.traffic_baseline_weeks,
            sites: s.effective_sites(site_ids),
            daily_report: s.daily_report,
            weekly_report: s.weekly_report,
            report_hour: s.report_hour_utc,
            report_weekday: s.report_weekday(),
            renotify: Duration::from_secs(s.renotify_secs),
            check_interval: Duration::from_secs(s.check_interval_secs),
            flush_grace: ChronoDuration::seconds(grace),
        }
    }
}

struct ReportSlot {
    /// The due date already handled (sent, skipped at startup, or given up on).
    done: NaiveDate,
    failures: u32,
    retry_after: Option<DateTime<Utc>>,
}

/// The evaluator. Owns all alert state; one pass is [`Engine::tick`].
pub struct Engine {
    state: Arc<AppState>,
    cfg: EngineConfig,
    clock: Arc<dyn Clock>,
    notifier: Notifier,
    alerts: AlertState,
    ops_memory: ops::Memory,
    traffic_done: HashMap<String, NaiveDateTime>,
    reports: HashMap<(String, Cadence), ReportSlot>,
}

impl Engine {
    pub fn new(
        state: Arc<AppState>,
        cfg: EngineConfig,
        clock: Arc<dyn Clock>,
        notifier: Notifier,
    ) -> Self {
        // A report whose due time has already passed when the process starts is
        // not sent: it was probably sent before the restart, and a duplicate is
        // worse than a late one. State is not persisted (see `rules`).
        let now = clock.now();
        let mut reports = HashMap::new();
        for site in &cfg.sites {
            for (enabled, cadence) in [
                (cfg.daily_report, Cadence::Daily),
                (cfg.weekly_report, Cadence::Weekly),
            ] {
                if enabled {
                    reports.insert(
                        (site.clone(), cadence),
                        ReportSlot {
                            done: report::latest_due(
                                cadence,
                                now,
                                cfg.report_hour,
                                cfg.report_weekday,
                            ),
                            failures: 0,
                            retry_after: None,
                        },
                    );
                }
            }
        }
        let alerts = AlertState::new(cfg.renotify);
        Self {
            state,
            cfg,
            clock,
            notifier,
            alerts,
            ops_memory: ops::Memory::default(),
            traffic_done: HashMap::new(),
            reports,
        }
    }

    /// Alerts currently firing.
    pub fn active_alerts(&self) -> usize {
        self.alerts.active()
    }

    fn apply(&mut self, id: &str, observation: Observation, now: DateTime<Utc>) {
        if let Some(notification) = self.alerts.observe(id, observation, now) {
            self.notifier.enqueue(notification);
        }
    }

    /// One evaluation pass at the clock's current time.
    pub async fn tick(&mut self) {
        let now = self.clock.now();
        if self.cfg.operational {
            let snapshot = ops::Snapshot::capture(&self.state, now.timestamp()).await;
            for (id, observation) in ops::evaluate(&mut self.ops_memory, &snapshot, &self.cfg.ops) {
                self.apply(&id, observation, now);
            }
        }
        if let Some(params) = self.cfg.traffic {
            for site in self.cfg.sites.clone() {
                self.check_traffic(&site, params, now).await;
            }
        }
        self.send_due_reports(now).await;
        self.state
            .alert_metrics
            .set_active_alerts(self.alerts.active());
    }

    async fn check_traffic(&mut self, site: &str, params: traffic::Params, now: DateTime<Utc>) {
        let hour = traffic::target_hour(now, self.cfg.flush_grace);
        if self.traffic_done.get(site) == Some(&hour) {
            return;
        }
        match traffic::fetch(&self.state, site, hour, self.cfg.baseline_weeks).await {
            Ok(sample) => {
                let firing = traffic::Firing {
                    drop: self.alerts.is_firing(&traffic::drop_id(site)),
                    rise: self.alerts.is_firing(&traffic::rise_id(site)),
                };
                for (id, observation) in traffic::judge(site, &sample, &params, firing) {
                    self.apply(&id, observation, now);
                }
                self.traffic_done.insert(site.to_string(), hour);
            }
            // Not marked done: tried again next pass. Nothing fires or clears.
            Err(e) => tracing::warn!(site, error = %e, "Traffic check could not run"),
        }
    }

    async fn send_due_reports(&mut self, now: DateTime<Utc>) {
        let keys: Vec<(String, Cadence)> = self.reports.keys().cloned().collect();
        for (site, cadence) in keys {
            let due =
                report::latest_due(cadence, now, self.cfg.report_hour, self.cfg.report_weekday);
            let Some(slot) = self.reports.get(&(site.clone(), cadence)) else {
                continue;
            };
            if slot.done >= due || slot.retry_after.is_some_and(|t| now < t) {
                continue;
            }
            let result = report::fetch(&self.state, &site, cadence, due).await;
            let Some(slot) = self.reports.get_mut(&(site.clone(), cadence)) else {
                continue;
            };
            match result {
                Ok(r) => {
                    slot.done = due;
                    slot.failures = 0;
                    slot.retry_after = None;
                    self.notifier
                        .enqueue(Notification::for_report(r.text(), r.to_json(), now));
                }
                Err(e) => {
                    slot.failures += 1;
                    if slot.failures >= REPORT_ATTEMPTS {
                        tracing::error!(
                            site, cadence = cadence.as_str(), error = %e,
                            "Giving up on a scheduled report after repeated failures"
                        );
                        slot.done = due;
                        slot.failures = 0;
                        slot.retry_after = None;
                    } else {
                        tracing::warn!(
                            site, cadence = cadence.as_str(), error = %e,
                            "Scheduled report could not be built; will retry"
                        );
                        slot.retry_after = Some(now + REPORT_RETRY);
                    }
                }
            }
        }
    }
}

/// Handle to the running alert tasks.
pub struct AlertsHandle {
    notifier: Notifier,
    closing: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl AlertsHandle {
    /// Stop evaluating, let the delivery in flight finish for up to `grace`,
    /// discard anything still queued (counted as dropped) and return.
    ///
    /// Never waits longer than `grace` plus scheduling: a receiver that is
    /// hanging cannot hold up process exit.
    pub async fn shutdown(self, grace: Duration) {
        let _ = self.closing.send(true);
        self.notifier.close();
        let mut tasks = self.tasks;
        let waiting = async {
            for task in &mut tasks {
                let _ = task.await;
            }
        };
        if tokio::time::timeout(grace, waiting).await.is_err() {
            tracing::warn!(
                grace_secs = grace.as_secs(),
                "A webhook delivery was still in flight at shutdown and was abandoned"
            );
            for task in &tasks {
                task.abort();
            }
        }
    }
}

/// Start the evaluator and the delivery worker. Returns `None` when no webhook
/// URL is configured.
///
/// # Errors
///
/// A message when the destination or CA file cannot be used. Validation has
/// normally caught these first.
pub fn start(
    state: &Arc<AppState>,
    settings: &AlertSettings,
    site_ids: &[String],
    flush_interval_secs: u64,
) -> Result<Option<AlertsHandle>, String> {
    if !settings.enabled() {
        return Ok(None);
    }
    let deliverer = build_deliverer(
        settings,
        Arc::clone(&state.alert_metrics),
        Arc::new(SystemClock),
    )?;
    let cfg = EngineConfig::from_settings(settings, site_ids, flush_interval_secs);
    tracing::info!(
        target = %deliverer.target_display(),
        signed = settings.webhook_secret.is_some(),
        operational = cfg.operational,
        traffic_alerts = cfg.traffic.is_some(),
        sites = cfg.sites.len(),
        daily_report = cfg.daily_report,
        weekly_report = cfg.weekly_report,
        "Webhook alerts enabled"
    );
    Ok(Some(spawn(state, cfg, deliverer, Arc::new(SystemClock))))
}

/// Spawn the tasks around an already-built deliverer.
pub fn spawn(
    state: &Arc<AppState>,
    cfg: EngineConfig,
    deliverer: Deliverer,
    clock: Arc<dyn Clock>,
) -> AlertsHandle {
    let notifier = Notifier::new(notifier::QUEUE_CAPACITY, Arc::clone(&state.alert_metrics));
    let (closing, closing_rx) = watch::channel(false);
    let interval = cfg.check_interval;
    let mut engine = Engine::new(Arc::clone(state), cfg, clock, notifier.clone());

    let worker = tokio::spawn(notifier.clone().run_worker(deliverer, closing_rx.clone()));
    let mut stop = closing_rx;
    let evaluator = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = stop.changed() => return,
            }
            // A pass abandoned at shutdown leaves only a blocking query that
            // the query deadline will interrupt.
            tokio::select! {
                () = engine.tick() => {}
                _ = stop.changed() => return,
            }
        }
    });
    AlertsHandle {
        notifier,
        closing,
        tasks: vec![worker, evaluator],
    }
}

/// Build the production deliverer from settings.
///
/// # Errors
///
/// The destination or CA file is unusable.
pub fn build_deliverer(
    settings: &AlertSettings,
    metrics: Arc<AlertMetrics>,
    clock: Arc<dyn Clock>,
) -> Result<Deliverer, String> {
    let target = settings.target()?;
    let tls = client::build_tls_config(settings.webhook_ca_file.as_deref())?;
    let allow_private = settings.allow_private_webhook_targets;
    let http = client::Client::new(
        Arc::new(client::SystemResolver),
        Arc::new(client::TcpDialer { allow_private }),
        tls,
        allow_private,
        Duration::from_secs(settings.timeout_secs),
    );
    Ok(Deliverer::new(
        http,
        DeliveryPolicy {
            target,
            secret: settings.webhook_secret.clone(),
            format: settings.format(),
            max_attempts: settings.max_attempts,
            backoff_base: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(30),
        },
        clock,
        metrics,
    ))
}

/// Send one test notification and report the outcome (for `--test-webhook`).
///
/// # Errors
///
/// A one-line description that names the host but never the full URL.
pub async fn send_test(settings: &AlertSettings) -> Result<String, String> {
    if !settings.enabled() {
        return Err("MALLARD_ALERTS_WEBHOOK_URL is not set".to_string());
    }
    let mut single = settings.clone();
    single.max_attempts = 1;
    let deliverer = build_deliverer(&single, Arc::default(), Arc::new(SystemClock))?;
    let (_keep, mut closing) = watch::channel(false);
    let note = Notification::test(Utc::now());
    deliverer
        .deliver(&note, &mut closing)
        .await
        .map(|_| {
            format!(
                "delivered a test notification to {}",
                deliverer.target_display()
            )
        })
        .map_err(|e| format!("delivery to {} failed: {e}", deliverer.target_display()))
}
