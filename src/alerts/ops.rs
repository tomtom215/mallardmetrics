//! Operational alerts: is this server doing its job?
//!
//! [`evaluate`] is a pure function of a [`Snapshot`] and a little memory, so
//! every threshold is tested with synthetic numbers. [`Snapshot::capture`] is
//! the only part that touches the running server, and it reads state the
//! `/metrics` endpoint already exposes.

use super::payload::{Alert, Severity};
use super::rules::Observation;
use crate::ingest::handler::AppState;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;

/// A reading of the server at one instant.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub now_unix: i64,
    pub started_at_unix: i64,
    /// 0 before the first successful flush.
    pub last_flush_success_unix: i64,
    /// Periodic flush failures + threshold-triggered flush failures + dropped
    /// events: every way a buffered event can fail to reach disk.
    pub write_trouble_total: u64,
    /// Rate-limiter evictions, both limiters.
    pub evictions_total: u64,
    pub behavioral_loaded: bool,
    pub cold_tier_degraded: bool,
}

impl Snapshot {
    /// Read the live counters. The cold-tier check lists the data directory, so
    /// it runs on the blocking pool.
    pub async fn capture(state: &std::sync::Arc<AppState>, now_unix: i64) -> Self {
        let cold = {
            let writer = state.buffer.conn().clone();
            let events_dir = state.events_dir.clone();
            tokio::task::spawn_blocking(move || {
                crate::storage::schema::cold_tier_status(&writer.lock(), &events_dir)
            })
            .await
            // A panic in the check is not evidence about the cold tier.
            .unwrap_or(crate::storage::schema::ColdTierStatus::Healthy)
        };
        let load = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        let ratelimit = &state.rate_limiter.capacity_evictions;
        let ip_ratelimit = &state.ip_rate_limiter.capacity_evictions;
        Self {
            now_unix,
            started_at_unix: i64::try_from(state.started_at_unix).unwrap_or(0),
            last_flush_success_unix: i64::try_from(load(&state.last_flush_success_unix))
                .unwrap_or(0),
            write_trouble_total: load(&state.flush_failures_total)
                + load(&state.buffer.flush_failures)
                + load(&state.buffer.dropped_events),
            evictions_total: load(ratelimit) + load(ip_ratelimit),
            behavioral_loaded: state.behavioral.is_loaded(),
            cold_tier_degraded: cold == crate::storage::schema::ColdTierStatus::Degraded,
        }
    }
}

/// Thresholds, from [`AlertSettings`](super::AlertSettings).
#[derive(Debug, Clone, Copy)]
pub struct Params {
    pub flush_stalled_secs: i64,
    pub failure_window_secs: i64,
    /// 0 disables the eviction alert.
    pub evictions_threshold: u64,
    pub behavioral_grace_secs: i64,
}

/// A monotonic counter sampled over time, to ask "how much did it grow lately?".
#[derive(Debug, Default)]
pub struct CounterWindow {
    samples: VecDeque<(i64, u64)>,
}

impl CounterWindow {
    /// Record the counter's value at `now`.
    pub fn record(&mut self, now: i64, value: u64, window: i64) {
        self.samples.push_back((now, value));
        // Keep one sample at or before the window start as the baseline.
        while self.samples.len() > 1 && self.samples[1].0 <= now - window {
            self.samples.pop_front();
        }
    }

    /// Growth over the window ending at the latest sample.
    pub fn increase(&self) -> u64 {
        match (self.samples.front(), self.samples.back()) {
            (Some((_, first)), Some((_, last))) => last.saturating_sub(*first),
            _ => 0,
        }
    }
}

/// What the evaluator remembers between passes.
#[derive(Debug, Default)]
pub struct Memory {
    write_trouble: CounterWindow,
    evictions: CounterWindow,
}

/// Judge one snapshot. Every alert id gets an observation each pass.
#[allow(clippy::too_many_lines)]
pub fn evaluate(mem: &mut Memory, snap: &Snapshot, p: &Params) -> Vec<(String, Observation)> {
    let mut out = Vec::with_capacity(5);
    let mut push = |id: &str, firing: Option<Alert>| {
        out.push((
            id.to_string(),
            firing.map_or(Observation::Clear, Observation::Firing),
        ));
    };

    // Flush stalled. A process that has never flushed is measured from its
    // start, so a fresh process gets one full threshold before it is blamed.
    let reference = snap.last_flush_success_unix.max(snap.started_at_unix);
    let silent = snap.now_unix - reference;
    push(
        "flush_stalled",
        (silent > p.flush_stalled_secs).then(|| Alert {
            id: "flush_stalled".to_string(),
            severity: Severity::Critical,
            category: "operational",
            summary: format!(
                "No successful flush for {} min (threshold {} min); buffered events are not \
                 reaching disk",
                silent / 60,
                p.flush_stalled_secs / 60
            ),
            site_id: None,
            details: json!({
                "seconds_since_last_flush": silent,
                "threshold_seconds": p.flush_stalled_secs,
                "ever_flushed": snap.last_flush_success_unix > 0,
            }),
        }),
    );

    mem.write_trouble.record(
        snap.now_unix,
        snap.write_trouble_total,
        p.failure_window_secs,
    );
    let trouble = mem.write_trouble.increase();
    push(
        "buffer_failing",
        (trouble > 0).then(|| Alert {
            id: "buffer_failing".to_string(),
            severity: Severity::Critical,
            category: "operational",
            summary: format!(
                "{trouble} flush failure(s) or dropped event(s) in the last {} min; events may \
                 be lost",
                p.failure_window_secs / 60
            ),
            site_id: None,
            details: json!({
                "failures_or_drops_in_window": trouble,
                "window_seconds": p.failure_window_secs,
            }),
        }),
    );

    let missing =
        !snap.behavioral_loaded && snap.now_unix - snap.started_at_unix > p.behavioral_grace_secs;
    push(
        "behavioral_extension_missing",
        missing.then(|| Alert {
            id: "behavioral_extension_missing".to_string(),
            severity: Severity::Warning,
            category: "operational",
            summary: "The behavioral extension is not loaded: funnels, retention, sessions, \
                      sequences and flow answer 503"
                .to_string(),
            site_id: None,
            details: json!({ "grace_seconds": p.behavioral_grace_secs }),
        }),
    );

    push(
        "cold_tier_degraded",
        snap.cold_tier_degraded.then(|| Alert {
            id: "cold_tier_degraded".to_string(),
            severity: Severity::Critical,
            category: "operational",
            summary: "The Parquet cold tier is not being read by queries: reports omit \
                      everything already flushed"
                .to_string(),
            site_id: None,
            details: json!({}),
        }),
    );

    mem.evictions
        .record(snap.now_unix, snap.evictions_total, p.failure_window_secs);
    let evictions = mem.evictions.increase();
    push(
        "rate_limiter_evictions_spike",
        (p.evictions_threshold > 0 && evictions >= p.evictions_threshold).then(|| Alert {
            id: "rate_limiter_evictions_spike".to_string(),
            severity: Severity::Warning,
            category: "operational",
            summary: format!(
                "{evictions} rate-limiter buckets evicted in the last {} min (threshold {}); \
                 many distinct clients are hitting ingestion",
                p.failure_window_secs / 60,
                p.evictions_threshold
            ),
            site_id: None,
            details: json!({
                "evictions_in_window": evictions,
                "threshold": p.evictions_threshold,
                "window_seconds": p.failure_window_secs,
            }),
        }),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: Params = Params {
        flush_stalled_secs: 600,
        failure_window_secs: 300,
        evictions_threshold: 100,
        behavioral_grace_secs: 600,
    };

    fn healthy(now: i64) -> Snapshot {
        Snapshot {
            now_unix: now,
            started_at_unix: 0,
            last_flush_success_unix: now - 30,
            behavioral_loaded: true,
            ..Snapshot::default()
        }
    }

    fn firing(out: &[(String, Observation)]) -> Vec<&str> {
        out.iter()
            .filter(|(_, o)| matches!(o, Observation::Firing(_)))
            .map(|(id, _)| id.as_str())
            .collect()
    }

    #[test]
    fn a_healthy_server_fires_nothing() {
        let mut mem = Memory::default();
        let out = evaluate(&mut mem, &healthy(10_000), &P);
        assert_eq!(out.len(), 5, "every id is observed every pass");
        assert!(firing(&out).is_empty());
    }

    #[test]
    fn flush_stalled_fires_after_the_threshold_only() {
        let mut mem = Memory::default();
        let mut snap = healthy(10_000);
        snap.last_flush_success_unix = 10_000 - 600;
        assert!(
            firing(&evaluate(&mut mem, &snap, &P)).is_empty(),
            "at the threshold"
        );
        snap.last_flush_success_unix = 10_000 - 601;
        assert_eq!(firing(&evaluate(&mut mem, &snap, &P)), ["flush_stalled"]);
    }

    #[test]
    fn a_new_process_that_never_flushed_gets_a_grace_period() {
        let mut mem = Memory::default();
        let snap = Snapshot {
            now_unix: 1_000,
            started_at_unix: 700,
            last_flush_success_unix: 0,
            behavioral_loaded: true,
            ..Snapshot::default()
        };
        assert!(firing(&evaluate(&mut mem, &snap, &P)).is_empty());
        let later = Snapshot {
            now_unix: 1_400,
            ..snap
        };
        assert_eq!(firing(&evaluate(&mut mem, &later, &P)), ["flush_stalled"]);
    }

    #[test]
    fn buffer_failing_tracks_growth_inside_the_window_and_then_clears() {
        let mut mem = Memory::default();
        let mut snap = healthy(10_000);
        snap.write_trouble_total = 5;
        assert!(
            firing(&evaluate(&mut mem, &snap, &P)).is_empty(),
            "first sight is a baseline"
        );
        snap.now_unix = 10_060;
        snap.last_flush_success_unix = 10_050;
        snap.write_trouble_total = 7;
        assert_eq!(firing(&evaluate(&mut mem, &snap, &P)), ["buffer_failing"]);
        // No further growth: the window slides past the increase and it clears.
        for step in 2..=8 {
            snap.now_unix = 10_000 + step * 60;
            snap.last_flush_success_unix = snap.now_unix - 10;
            let out = evaluate(&mut mem, &snap, &P);
            if step >= 7 {
                assert!(firing(&out).is_empty(), "step {step}: window passed");
            }
        }
    }

    #[test]
    fn the_extension_is_forgiven_during_startup_then_alerts() {
        let mut mem = Memory::default();
        let mut snap = healthy(1_000);
        snap.started_at_unix = 500;
        snap.behavioral_loaded = false;
        assert!(
            firing(&evaluate(&mut mem, &snap, &P)).is_empty(),
            "500 s < 600 s grace"
        );
        snap.now_unix = 1_101;
        snap.last_flush_success_unix = 1_090;
        assert_eq!(
            firing(&evaluate(&mut mem, &snap, &P)),
            ["behavioral_extension_missing"]
        );
        snap.behavioral_loaded = true;
        assert!(
            firing(&evaluate(&mut mem, &snap, &P)).is_empty(),
            "loaded later: clears"
        );
    }

    #[test]
    fn cold_tier_degraded_follows_the_flag() {
        let mut mem = Memory::default();
        let mut snap = healthy(10_000);
        snap.cold_tier_degraded = true;
        assert_eq!(
            firing(&evaluate(&mut mem, &snap, &P)),
            ["cold_tier_degraded"]
        );
    }

    #[test]
    fn eviction_spike_needs_the_threshold_and_can_be_disabled() {
        let mut mem = Memory::default();
        let mut snap = healthy(10_000);
        snap.evictions_total = 1_000;
        evaluate(&mut mem, &snap, &P);
        snap.now_unix = 10_060;
        snap.last_flush_success_unix = 10_050;
        snap.evictions_total = 1_099;
        assert!(
            firing(&evaluate(&mut mem, &snap, &P)).is_empty(),
            "99 < 100"
        );
        snap.now_unix = 10_120;
        snap.last_flush_success_unix = 10_110;
        snap.evictions_total = 1_100;
        assert_eq!(
            firing(&evaluate(&mut mem, &snap, &P)),
            ["rate_limiter_evictions_spike"]
        );
        let off = Params {
            evictions_threshold: 0,
            ..P
        };
        let mut mem = Memory::default();
        snap.evictions_total = 0;
        evaluate(&mut mem, &snap, &off);
        snap.evictions_total = 1_000_000;
        snap.now_unix += 60;
        snap.last_flush_success_unix = snap.now_unix - 5;
        assert!(firing(&evaluate(&mut mem, &snap, &off)).is_empty());
    }

    #[test]
    fn counter_window_reports_growth_since_the_window_start() {
        let mut w = CounterWindow::default();
        w.record(0, 10, 100);
        w.record(50, 15, 100);
        assert_eq!(w.increase(), 5);
        w.record(120, 15, 100);
        assert_eq!(
            w.increase(),
            5,
            "growth between t=0 and t=50 is not yet provably old"
        );
        w.record(160, 15, 100);
        assert_eq!(
            w.increase(),
            0,
            "the sample at 0 fell out; baseline is 15 at t=50"
        );
    }
}
