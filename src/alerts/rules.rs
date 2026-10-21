//! Alert life cycle: new, repeated, resolved.
//!
//! Pure bookkeeping over a caller-supplied clock, so cooldown and resolution are
//! exact assertions in tests. State is in memory: a restart forgets which
//! alerts were firing, so a condition that is still true is announced again once
//! (documented in alerts-and-reports.md). Persisting it would mean a state file
//! whose only benefit is suppressing one repeat message after a restart, against
//! a new failure mode (a stale or unwritable file), so it is not persisted.

use super::payload::{Alert, AlertStatus, Notification};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::time::Duration;

/// What an evaluator concluded about one alert id this pass.
///
/// An evaluator that could not decide (a failed query) reports nothing at all,
/// which leaves the alert as it was: a failed check must neither fire nor
/// resolve anything.
#[derive(Debug, Clone)]
pub enum Observation {
    Firing(Alert),
    Clear,
}

struct Entry {
    alert: Alert,
    since: DateTime<Utc>,
    last_notified: DateTime<Utc>,
    notifications: u32,
}

/// The set of alerts currently firing.
pub struct AlertState {
    firing: HashMap<String, Entry>,
    renotify: Duration,
}

impl AlertState {
    pub fn new(renotify: Duration) -> Self {
        Self {
            firing: HashMap::new(),
            renotify,
        }
    }

    /// Record an observation and return the notification it calls for, if any.
    ///
    /// * newly firing: notify at once;
    /// * still firing: notify again only when `renotify` has passed since the
    ///   last message, so a condition that persists is a reminder, not a stream;
    /// * cleared while firing: one `resolved` message;
    /// * cleared while not firing: nothing.
    pub fn observe(
        &mut self,
        id: &str,
        observation: Observation,
        now: DateTime<Utc>,
    ) -> Option<Notification> {
        match observation {
            Observation::Firing(alert) => {
                if let Some(entry) = self.firing.get_mut(id) {
                    let due = now
                        .signed_duration_since(entry.last_notified)
                        .to_std()
                        .is_ok_and(|elapsed| elapsed >= self.renotify);
                    entry.alert = alert;
                    if !due {
                        return None;
                    }
                    entry.last_notified = now;
                    entry.notifications += 1;
                    return Some(Notification::for_alert(
                        &entry.alert,
                        AlertStatus::Firing,
                        entry.since,
                        entry.notifications,
                        now,
                    ));
                }
                let notification =
                    Notification::for_alert(&alert, AlertStatus::Firing, now, 1, now);
                self.firing.insert(
                    id.to_string(),
                    Entry {
                        alert,
                        since: now,
                        last_notified: now,
                        notifications: 1,
                    },
                );
                Some(notification)
            }
            Observation::Clear => {
                let entry = self.firing.remove(id)?;
                Some(Notification::for_alert(
                    &entry.alert,
                    AlertStatus::Resolved,
                    entry.since,
                    entry.notifications,
                    now,
                ))
            }
        }
    }

    /// How many alerts are firing.
    pub fn active(&self) -> usize {
        self.firing.len()
    }

    /// Whether `id` is firing.
    pub fn is_firing(&self, id: &str) -> bool {
        self.firing.contains_key(id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::metrics::Kind;
    use super::super::payload::Severity;
    use super::*;
    use chrono::TimeZone;

    fn alert(id: &str) -> Alert {
        Alert {
            id: id.into(),
            severity: Severity::Warning,
            category: "operational",
            summary: "x".into(),
            site_id: None,
            details: serde_json::json!({}),
        }
    }

    fn t(minutes: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 21, 0, 0, 0).unwrap() + chrono::Duration::minutes(minutes)
    }

    #[test]
    fn fires_once_repeats_after_the_cooldown_and_resolves_once() {
        let mut state = AlertState::new(Duration::from_mins(60));
        let fire = |s: &mut AlertState, at| s.observe("a", Observation::Firing(alert("a")), at);

        let first = fire(&mut state, t(0)).expect("new alert notifies");
        assert_eq!(first.kind, Kind::Alert);
        assert_eq!(state.active(), 1);

        assert!(fire(&mut state, t(1)).is_none(), "inside the cooldown");
        assert!(fire(&mut state, t(59)).is_none(), "one minute short");
        let again = fire(&mut state, t(60)).expect("cooldown elapsed");
        assert_eq!(again.data.as_ref().unwrap().1["notifications"], 2);
        assert!(fire(&mut state, t(61)).is_none(), "cooldown restarts");

        let resolved = state
            .observe("a", Observation::Clear, t(70))
            .expect("resolved");
        assert_eq!(resolved.kind, Kind::Resolved);
        assert_eq!(resolved.data.as_ref().unwrap().1["status"], "resolved");
        assert_eq!(state.active(), 0);
        assert!(
            state.observe("a", Observation::Clear, t(71)).is_none(),
            "clearing what is not firing is silent"
        );
    }

    #[test]
    fn a_resolved_alert_that_returns_is_new() {
        let mut state = AlertState::new(Duration::from_mins(60));
        state.observe("a", Observation::Firing(alert("a")), t(0));
        state.observe("a", Observation::Clear, t(5));
        let again = state.observe("a", Observation::Firing(alert("a")), t(6));
        assert!(again.is_some(), "no cooldown across a resolution");
        assert_eq!(again.unwrap().data.unwrap().1["notifications"], 1);
    }

    #[test]
    fn alerts_are_independent() {
        let mut state = AlertState::new(Duration::from_mins(60));
        assert!(
            state
                .observe("a", Observation::Firing(alert("a")), t(0))
                .is_some()
        );
        assert!(
            state
                .observe("b", Observation::Firing(alert("b")), t(1))
                .is_some()
        );
        assert!(state.observe("a", Observation::Clear, t(2)).is_some());
        assert!(state.is_firing("b"));
    }
}
