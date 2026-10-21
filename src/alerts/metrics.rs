//! Counters for `/metrics`. No label has more than three values.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};

/// What a notification is about, as a metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An alert started firing (or is being repeated).
    Alert,
    /// An alert cleared.
    Resolved,
    /// A scheduled daily or weekly report.
    Report,
    /// The `--test-webhook` check. Never queued.
    Test,
}

impl Kind {
    /// The value of the `kind` field and metric label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Alert => "alert",
            Self::Resolved => "resolved",
            Self::Report => "report",
            Self::Test => "test",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Alert | Self::Test => 0,
            Self::Resolved => 1,
            Self::Report => 2,
        }
    }
}

const LABELS: [&str; 3] = ["alert", "resolved", "report"];

/// Process-wide notification counters, shared through `AppState`.
#[derive(Debug, Default)]
pub struct AlertMetrics {
    sent: [AtomicU64; 3],
    failed: [AtomicU64; 3],
    dropped: [AtomicU64; 3],
    last_success_unix: AtomicU64,
    queue_depth: AtomicU64,
    active_alerts: AtomicU64,
}

impl AlertMetrics {
    /// A delivery the receiver accepted.
    pub fn sent(&self, kind: Kind, at_unix: u64) {
        self.sent[kind.index()].fetch_add(1, Ordering::Relaxed);
        self.last_success_unix.store(at_unix, Ordering::Relaxed);
    }

    /// A delivery abandoned after its retries.
    pub fn failed(&self, kind: Kind) {
        self.failed[kind.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// A notification discarded without an attempt: queue full, or shutting down.
    pub fn dropped(&self, kind: Kind) {
        self.dropped[kind.index()].fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_queue_depth(&self, depth: usize) {
        self.queue_depth.store(depth as u64, Ordering::Relaxed);
    }

    pub fn set_active_alerts(&self, count: usize) {
        self.active_alerts.store(count as u64, Ordering::Relaxed);
    }

    pub fn sent_count(&self, kind: Kind) -> u64 {
        self.sent[kind.index()].load(Ordering::Relaxed)
    }

    pub fn failed_count(&self, kind: Kind) -> u64 {
        self.failed[kind.index()].load(Ordering::Relaxed)
    }

    pub fn dropped_count(&self, kind: Kind) -> u64 {
        self.dropped[kind.index()].load(Ordering::Relaxed)
    }

    pub fn last_success_unix(&self) -> u64 {
        self.last_success_unix.load(Ordering::Relaxed)
    }

    /// Append the Prometheus exposition for these counters.
    pub fn write_prometheus(&self, out: &mut String) {
        let families: [(&str, &str, &[AtomicU64; 3]); 3] = [
            (
                "mallard_notifications_sent_total",
                "Webhook notifications the receiver accepted, by kind",
                &self.sent,
            ),
            (
                "mallard_notifications_failed_total",
                "Webhook notifications abandoned after their retries, by kind",
                &self.failed,
            ),
            (
                "mallard_notifications_dropped_total",
                "Webhook notifications discarded unsent (queue full or shutdown), by kind",
                &self.dropped,
            ),
        ];
        for (name, help, values) in families {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            for (label, value) in LABELS.iter().zip(values) {
                let _ = writeln!(
                    out,
                    "{name}{{kind=\"{label}\"}} {}",
                    value.load(Ordering::Relaxed)
                );
            }
        }
        for (name, help, kind, value) in [
            (
                "mallard_notification_last_success_timestamp_seconds",
                "Unix time of the last webhook delivery the receiver accepted (0 before the first)",
                "gauge",
                &self.last_success_unix,
            ),
            (
                "mallard_notification_queue_depth",
                "Webhook notifications waiting to be delivered",
                "gauge",
                &self.queue_depth,
            ),
            (
                "mallard_alerts_active",
                "Alerts currently firing, as seen by the alert evaluator",
                "gauge",
                &self.active_alerts,
            ),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} {kind}");
            let _ = writeln!(out, "{name} {}", value.load(Ordering::Relaxed));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposition_has_three_kinds_and_no_other_labels() {
        let m = AlertMetrics::default();
        m.sent(Kind::Alert, 42);
        m.failed(Kind::Report);
        m.dropped(Kind::Resolved);
        let mut out = String::new();
        m.write_prometheus(&mut out);
        assert!(out.contains("mallard_notifications_sent_total{kind=\"alert\"} 1"));
        assert!(out.contains("mallard_notifications_failed_total{kind=\"report\"} 1"));
        assert!(out.contains("mallard_notifications_dropped_total{kind=\"resolved\"} 1"));
        assert!(out.contains("mallard_notification_last_success_timestamp_seconds 42"));
        assert_eq!(out.matches("kind=").count(), 9);
    }
}
