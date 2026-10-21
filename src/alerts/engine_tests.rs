//! The evaluator against a real (in-memory) database and an injected clock.

use super::metrics::Kind;
use super::notifier::Notifier;
use super::payload::Notification;
use super::report::Cadence;
use super::{AlertSettings, Clock, Engine, EngineConfig, ManualClock};
use crate::api::stats::QueryLimits;
use crate::ingest::handler::AppState;
use crate::test_support::state_builder;
use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

const SITE: &str = "t.example";

fn at(d: u32, h: u32, m: u32) -> DateTime<Utc> {
    // 2026-10-21 is a Wednesday.
    Utc.with_ymd_and_hms(2026, 10, d, h, m, 0).unwrap()
}

fn cfg() -> EngineConfig {
    let mut settings = AlertSettings {
        webhook_url: Some("https://hooks.example.com/x".into()),
        sites: vec![SITE.to_string()],
        renotify_secs: 3600,
        // The extension may or may not be installed where the tests run.
        behavioral_grace_secs: 10_u64.pow(9),
        ..AlertSettings::default()
    };
    settings.traffic_drop_percent = 50;
    EngineConfig::from_settings(&settings, &[], 60)
}

struct Rig {
    state: Arc<AppState>,
    engine: Engine,
    notifier: Notifier,
    clock: Arc<ManualClock>,
}

fn rig_with(cfg: EngineConfig, now: DateTime<Utc>, state: Arc<AppState>) -> Rig {
    let clock = ManualClock::new(now);
    let notifier = Notifier::new(64, Arc::clone(&state.alert_metrics));
    let engine = Engine::new(Arc::clone(&state), cfg, clock.clone(), notifier.clone());
    Rig {
        state,
        engine,
        notifier,
        clock,
    }
}

fn rig(cfg: EngineConfig, now: DateTime<Utc>) -> Rig {
    // `build_state` leaks its temporary directory, which these tests do not mind.
    rig_with(cfg, now, state_builder().build_state())
}

impl Rig {
    fn drain(&self) -> Vec<Notification> {
        std::iter::from_fn(|| self.notifier.pop()).collect()
    }

    fn healthy_flush(&self) {
        self.state.last_flush_success_unix.store(
            u64::try_from(self.clock.now().timestamp()).unwrap() - 5,
            Ordering::Relaxed,
        );
    }

    fn sql(&self, sql: &str) {
        self.state.buffer.conn().lock().execute_batch(sql).unwrap();
    }

    /// `n` pageviews on `site` in the hour starting at `hour`.
    fn pageviews(&self, site: &str, hour: DateTime<Utc>, n: u32, path: &str, source: Option<&str>) {
        let source = source.map_or_else(|| "NULL".to_string(), |s| format!("'{s}'"));
        self.sql(&format!(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname, referrer_source) \
             SELECT '{site}', 'v' || i, TIMESTAMP '{}' + to_seconds(i), 'pageview', '{path}', {source} \
             FROM range({n}) t(i)",
            hour.format("%Y-%m-%d %H:%M:%S")
        ));
    }
}

fn alert_of(n: &Notification) -> &Value {
    &n.data.as_ref().unwrap().1
}

// ── operational: cooldown and resolution against the clock ─────────────────

#[tokio::test]
async fn an_alert_notifies_once_repeats_after_the_cooldown_and_resolves() {
    let mut r = rig(cfg(), at(21, 12, 0));

    r.healthy_flush();
    r.engine.tick().await;
    assert!(r.drain().is_empty(), "a healthy server is quiet");

    // The flush loop dies.
    r.state.last_flush_success_unix.store(
        u64::try_from(at(21, 12, 0).timestamp()).unwrap() - 5,
        Ordering::Relaxed,
    );
    r.clock.set(at(21, 12, 11));
    r.engine.tick().await;
    let first = r.drain();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].kind, Kind::Alert);
    assert_eq!(alert_of(&first[0])["id"], "flush_stalled");
    assert_eq!(alert_of(&first[0])["status"], "firing");
    assert_eq!(alert_of(&first[0])["severity"], "critical");
    assert_eq!(r.engine.active_alerts(), 1);
    assert_eq!(
        r.state.alert_metrics.sent_count(Kind::Alert),
        0,
        "nothing delivered here"
    );

    // Still broken, inside the one-hour cooldown: silence.
    r.clock.set(at(21, 12, 40));
    r.engine.tick().await;
    assert!(r.drain().is_empty());

    // Cooldown over: one reminder.
    r.clock.set(at(21, 13, 12));
    r.engine.tick().await;
    let again = r.drain();
    assert_eq!(again.len(), 1);
    assert_eq!(alert_of(&again[0])["notifications"], 2);

    // The flush recovers: one resolution.
    r.clock.set(at(21, 13, 20));
    r.healthy_flush();
    r.engine.tick().await;
    let done = r.drain();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].kind, Kind::Resolved);
    assert_eq!(alert_of(&done[0])["status"], "resolved");
    assert_eq!(r.engine.active_alerts(), 0);

    r.clock.set(at(21, 13, 30));
    r.healthy_flush();
    r.engine.tick().await;
    assert!(r.drain().is_empty(), "and then quiet again");
}

#[tokio::test]
async fn dropped_events_raise_buffer_failing() {
    let mut r = rig(cfg(), at(21, 12, 0));
    r.healthy_flush();
    r.engine.tick().await;
    r.clock.advance(ChronoDuration::minutes(1));
    r.healthy_flush();
    r.state
        .buffer
        .dropped_events
        .fetch_add(3, Ordering::Relaxed);
    r.engine.tick().await;
    let sent = r.drain();
    assert_eq!(sent.len(), 1);
    assert_eq!(alert_of(&sent[0])["id"], "buffer_failing");
    assert_eq!(
        alert_of(&sent[0])["details"]["failures_or_drops_in_window"],
        3
    );
}

// ── analytical, on seeded data ─────────────────────────────────────────────

/// Four earlier weeks at `baseline` pageviews in the hour 05:00 UTC on Wednesdays,
/// and `current` in this week's 05:00 hour. "Now" is 06:10, so the judged hour is
/// 05:00-06:00 (the grace is two minutes).
fn seeded(baseline: u32, weeks: u32, current: u32, mutate: impl FnOnce(&mut EngineConfig)) -> Rig {
    let mut c = cfg();
    mutate(&mut c);
    let r = rig(c, at(21, 6, 10));
    for w in 1..=weeks {
        r.pageviews(
            SITE,
            at(21, 5, 0) - ChronoDuration::weeks(i64::from(w)),
            baseline,
            "/",
            None,
        );
    }
    r.pageviews(SITE, at(21, 5, 0), current, "/", None);
    // Another site that must not leak into the counts.
    r.pageviews("other.example", at(21, 5, 0), 500, "/", None);
    r.healthy_flush();
    r
}

fn traffic_alerts(r: &Rig) -> Vec<Notification> {
    r.drain()
        .into_iter()
        .filter(|n| alert_of(n)["category"] == "analytical")
        .collect()
}

#[tokio::test]
async fn a_large_drop_on_seeded_data_alerts_with_machine_fields() {
    let mut r = seeded(100, 4, 30, |_| {});
    r.engine.tick().await;
    let sent = traffic_alerts(&r);
    assert_eq!(sent.len(), 1, "{sent:?}");
    let alert = alert_of(&sent[0]);
    assert_eq!(alert["id"], "traffic_drop:t.example");
    assert_eq!(alert["site_id"], SITE);
    let d = &alert["details"];
    assert_eq!(d["pageviews"], 30);
    assert_eq!(d["baseline_mean"], 100.0);
    assert_eq!(d["baseline_weeks_used"], 4);
    assert_eq!(d["change_percent"], -70.0);
    assert_eq!(d["hour_start"], "2026-10-21T05:00:00Z");
    assert!(sent[0].text.contains("t.example") && sent[0].text.contains("down 70%"));
}

#[tokio::test]
async fn a_smaller_drop_does_not_alert() {
    let mut r = seeded(100, 4, 60, |_| {});
    r.engine.tick().await;
    assert!(traffic_alerts(&r).is_empty());
}

#[tokio::test]
async fn the_volume_floor_keeps_a_quiet_site_silent() {
    // 10 -> 1 is -90%, under the 50 pageviews/hour floor.
    let mut r = seeded(10, 4, 1, |_| {});
    r.engine.tick().await;
    assert!(traffic_alerts(&r).is_empty());
    let mut r = seeded(10, 4, 1, |c| c.traffic.as_mut().unwrap().min_pageviews = 10);
    r.engine.tick().await;
    assert_eq!(
        traffic_alerts(&r).len(),
        1,
        "lower the floor and the same data alerts"
    );
}

#[tokio::test]
async fn too_little_history_is_not_judged_and_missing_weeks_are_not_zeros() {
    let mut r = seeded(100, 1, 0, |_| {});
    r.engine.tick().await;
    assert!(
        traffic_alerts(&r).is_empty(),
        "one week of history is not a baseline"
    );

    // Two weeks of history out of four requested: judged against those two, not
    // averaged with two weeks in which the site did not exist yet.
    let mut r = seeded(100, 2, 30, |_| {});
    r.engine.tick().await;
    let sent = traffic_alerts(&r);
    assert_eq!(sent.len(), 1);
    assert_eq!(alert_of(&sent[0])["details"]["baseline_mean"], 100.0);
    assert_eq!(alert_of(&sent[0])["details"]["baseline_weeks_used"], 2);
}

#[tokio::test]
async fn a_rise_alerts_only_when_enabled_and_each_hour_is_judged_once() {
    let mut r = seeded(100, 4, 300, |c| {
        c.traffic.as_mut().unwrap().rise_percent = 150;
    });
    r.engine.tick().await;
    let sent = traffic_alerts(&r);
    assert_eq!(sent.len(), 1);
    assert_eq!(alert_of(&sent[0])["id"], "traffic_rise:t.example");

    // The same hour again, a minute later: no second query, no second message.
    r.clock.advance(ChronoDuration::minutes(1));
    r.engine.tick().await;
    assert!(traffic_alerts(&r).is_empty());

    // The next hour is normal: the rise resolves.
    r.pageviews(SITE, at(21, 6, 0), 100, "/", None);
    for w in 1..=4_u32 {
        r.pageviews(
            SITE,
            at(21, 6, 0) - ChronoDuration::weeks(i64::from(w)),
            100,
            "/",
            None,
        );
    }
    r.clock.set(at(21, 7, 10));
    r.healthy_flush();
    r.engine.tick().await;
    let resolved = r.drain();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].kind, Kind::Resolved);
}

#[tokio::test]
async fn traffic_queries_go_through_the_query_semaphore() {
    // No query slots at all, and a short queue wait: the check cannot run, so it
    // neither fires nor clears anything, and is retried on the next pass.
    let state = state_builder()
        .query_permits(0)
        .query_limits(QueryLimits {
            queue_wait: Duration::from_millis(50),
            deadline: Duration::from_secs(5),
        })
        .build_state();
    let mut r = rig_with(cfg(), at(21, 6, 10), state);
    r.pageviews(SITE, at(21, 5, 0), 0, "/", None);
    r.healthy_flush();
    r.engine.tick().await;
    assert!(traffic_alerts(&r).is_empty());
    assert!(
        r.engine.traffic_done.is_empty(),
        "not marked done: it will be retried"
    );
}

// ── reports ────────────────────────────────────────────────────────────────

fn report_cfg() -> EngineConfig {
    let mut c = cfg();
    c.traffic = None;
    c.operational = false;
    c.daily_report = true;
    c.weekly_report = true;
    c
}

#[tokio::test]
async fn the_daily_report_summarises_yesterday_against_the_day_before() {
    // 07:00: the 08:00 report has not come due yet, so nothing is sent at start.
    let mut r = rig(report_cfg(), at(21, 7, 0));
    // Yesterday (20th): 3 pageviews on /a from Google, 2 on /b direct, 5 visitors.
    r.pageviews(SITE, at(20, 10, 0), 3, "/a", Some("Google"));
    r.pageviews(SITE, at(20, 11, 0), 2, "/b", None);
    // The day before (19th): 4 views.
    r.pageviews(SITE, at(19, 10, 0), 4, "/a", Some("Google"));
    // A neighbouring site must not appear.
    r.pageviews("other.example", at(20, 10, 0), 50, "/zzz", Some("Evil"));

    r.engine.tick().await;
    assert!(r.drain().is_empty(), "nothing due yet");

    r.clock.set(at(21, 8, 1));
    r.engine.tick().await;
    let sent = r.drain();
    assert_eq!(sent.len(), 1, "daily only; the weekly one is due Mondays");
    assert_eq!(sent[0].kind, Kind::Report);
    let (key, report) = sent[0].data.as_ref().unwrap();
    assert_eq!(*key, "report");
    assert_eq!(report["period"], "daily");
    assert_eq!(report["site_id"], SITE);
    assert_eq!(report["start_date"], "2026-10-20");
    assert_eq!(report["end_date"], "2026-10-20");
    assert_eq!(report["pageviews"], 5);
    assert_eq!(report["previous"]["pageviews"], 4);
    assert_eq!(report["change_percent"]["pageviews"], 25.0);
    assert_eq!(report["top_pages"][0]["path"], "/a");
    assert_eq!(report["top_pages"][0]["pageviews"], 3);
    assert_eq!(report["top_sources"][0]["source"], "Google");
    let text = &sent[0].text;
    assert!(
        text.contains("daily report for t.example") && text.contains("+25.0%"),
        "{text}"
    );
    assert!(!text.contains("/zzz") && !text.contains("Evil"));

    // Not sent twice.
    r.clock.set(at(21, 9, 0));
    r.engine.tick().await;
    assert!(r.drain().is_empty());

    // Tomorrow's is.
    r.clock.set(at(22, 8, 5));
    r.engine.tick().await;
    assert_eq!(r.drain().len(), 1);
}

#[tokio::test]
async fn a_restart_after_the_due_time_does_not_resend() {
    let mut r = rig(report_cfg(), at(21, 9, 0));
    r.engine.tick().await;
    assert!(
        r.drain().is_empty(),
        "started after 08:00: today's report is assumed sent"
    );
}

#[tokio::test]
async fn the_weekly_report_goes_out_on_its_weekday_covering_the_week_before() {
    let mut r = rig(report_cfg(), at(25, 12, 0)); // Sunday
    for d in 19..26_u32 {
        r.pageviews(SITE, at(d, 9, 0), 10, "/w", Some("Bing"));
    }
    r.clock.set(at(26, 8, 2)); // Monday
    r.engine.tick().await;
    let sent: Vec<_> = r
        .drain()
        .into_iter()
        .filter(|n| alert_of(n)["period"] == "weekly")
        .collect();
    assert_eq!(sent.len(), 1);
    let report = alert_of(&sent[0]);
    assert_eq!(report["start_date"], "2026-10-19");
    assert_eq!(report["end_date"], "2026-10-25");
    assert_eq!(report["pageviews"], 70);
    assert_eq!(report["top_sources"][0]["source"], "Bing");
    assert_eq!(Cadence::Weekly.as_str(), "weekly");
}

#[tokio::test]
async fn an_unbuildable_report_is_retried_then_given_up_on() {
    let state = state_builder()
        .query_permits(0)
        .query_limits(QueryLimits {
            queue_wait: Duration::from_millis(20),
            deadline: Duration::from_secs(5),
        })
        .build_state();
    let mut c = report_cfg();
    c.weekly_report = false;
    let mut r = rig_with(c, at(21, 7, 0), state);
    r.clock.set(at(21, 8, 1));
    for attempt in 1..=6 {
        r.engine.tick().await;
        assert!(r.drain().is_empty(), "attempt {attempt}");
        r.clock.advance(ChronoDuration::minutes(6));
    }
    // After the fifth failure the period is skipped, not retried forever.
    let slot = &r.engine.reports[&(SITE.to_string(), Cadence::Daily)];
    assert_eq!(slot.failures, 0);
    assert_eq!(slot.done, at(21, 8, 1).date_naive());
}

// ── /metrics ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn metrics_expose_the_notification_counters() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let state = state_builder().build_state();
    state.alert_metrics.sent(Kind::Report, 1_700_000_000);
    state.alert_metrics.dropped(Kind::Alert);
    let response = crate::server::build_router(state)
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        text.contains("mallard_notifications_sent_total{kind=\"report\"} 1"),
        "{text}"
    );
    assert!(text.contains("mallard_notifications_dropped_total{kind=\"alert\"} 1"));
    assert!(text.contains("mallard_notifications_failed_total{kind=\"resolved\"} 0"));
    assert!(text.contains("mallard_notification_last_success_timestamp_seconds 1700000000"));
    assert!(text.contains("mallard_alerts_active 0"));
}
