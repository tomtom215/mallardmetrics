//! What is sent: the notification model and its JSON encodings.
//!
//! The generic payload is a superset of what Slack and Discord incoming
//! webhooks read, so one body serves all three. Slack documents the plain-text
//! field as `text`, Discord as `content` (up to 2000 characters); the generic
//! body carries both, each filled with the same short sentence. Whether either
//! product tolerates the other's field is *not* stated in their documentation,
//! so `payload_format = "slack"` and `"discord"` send exactly one field.
//!
//! Nothing here can hold visitor-level data or an IP address: the only inputs
//! are site IDs from configuration, aggregate counts, page paths and referrer
//! sources already aggregated by the query layer.

use super::metrics::Kind;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Value, json};

/// Version of the generic payload. Bumped only for an incompatible change.
pub const SCHEMA: &str = "mallard.notification/v1";

/// Discord's limit is 2000 characters; stay under it.
const MAX_TEXT_CHARS: usize = 1900;

/// How loud an alert is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Warning,
    Info,
}

impl Severity {
    const fn label(self) -> &'static str {
        match self {
            Self::Critical => "CRITICAL",
            Self::Warning => "WARNING",
            Self::Info => "INFO",
        }
    }
}

/// The body encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PayloadFormat {
    /// Structured JSON with `text` and `content` conveniences.
    #[default]
    Generic,
    /// `{"text": ...}` only.
    Slack,
    /// `{"content": ...}` only.
    Discord,
}

impl PayloadFormat {
    /// Parse the configuration value.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "generic" => Some(Self::Generic),
            "slack" => Some(Self::Slack),
            "discord" => Some(Self::Discord),
            _ => None,
        }
    }
}

/// A condition worth telling someone about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// Stable across occurrences: `flush_stalled`, `traffic_drop:example.com`.
    pub id: String,
    pub severity: Severity,
    /// `operational` or `analytical`.
    pub category: &'static str,
    /// One sentence a person can act on.
    pub summary: String,
    pub site_id: Option<String>,
    /// Machine-readable measurements (observed value, threshold, ...).
    pub details: Value,
}

/// Where an alert is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertStatus {
    Firing,
    Resolved,
}

/// One message for the receiver.
#[derive(Debug, Clone)]
pub struct Notification {
    pub kind: Kind,
    /// Unique per notification; identical across retries so a receiver can
    /// de-duplicate.
    pub delivery_id: String,
    pub created: DateTime<Utc>,
    pub text: String,
    /// The structured part: `("alert", {...})` or `("report", {...})`.
    pub data: Option<(&'static str, Value)>,
}

impl Notification {
    /// A notification for an alert changing state.
    pub fn for_alert(
        alert: &Alert,
        status: AlertStatus,
        since: DateTime<Utc>,
        notifications: u32,
        now: DateTime<Utc>,
    ) -> Self {
        let (kind, tag) = match status {
            AlertStatus::Firing => (Kind::Alert, alert.severity.label()),
            AlertStatus::Resolved => (Kind::Resolved, "RESOLVED"),
        };
        let data = json!({
            "id": alert.id,
            "status": match status { AlertStatus::Firing => "firing", AlertStatus::Resolved => "resolved" },
            "severity": alert.severity,
            "category": alert.category,
            "summary": alert.summary,
            "site_id": alert.site_id,
            "since": since.to_rfc3339_opts(SecondsFormat::Secs, true),
            "notifications": notifications,
            "details": alert.details,
        });
        Self {
            kind,
            delivery_id: uuid::Uuid::new_v4().to_string(),
            created: now,
            text: format!("[{tag}] {}", alert.summary),
            data: Some(("alert", data)),
        }
    }

    /// A report notification.
    pub fn for_report(text: String, report: Value, now: DateTime<Utc>) -> Self {
        Self {
            kind: Kind::Report,
            delivery_id: uuid::Uuid::new_v4().to_string(),
            created: now,
            text,
            data: Some(("report", report)),
        }
    }

    /// The `--test-webhook` message.
    pub fn test(now: DateTime<Utc>) -> Self {
        Self {
            kind: Kind::Test,
            delivery_id: uuid::Uuid::new_v4().to_string(),
            created: now,
            text: "Mallard Metrics webhook test: if you can read this, delivery works.".to_string(),
            data: None,
        }
    }

    /// Serialise for sending.
    pub fn body(&self, format: PayloadFormat) -> Vec<u8> {
        let text: String = self.text.chars().take(MAX_TEXT_CHARS).collect();
        let value = match format {
            PayloadFormat::Slack => json!({ "text": text }),
            // `allowed_mentions.parse = []` tells Discord to ping nobody whatever
            // the text contains, a second line of defence behind the sanitising
            // of visitor-supplied values in the report text.
            PayloadFormat::Discord => {
                json!({ "content": text, "allowed_mentions": { "parse": [] } })
            }
            PayloadFormat::Generic => {
                let mut obj = json!({
                    "schema": SCHEMA,
                    "delivery_id": self.delivery_id,
                    "kind": self.kind.as_str(),
                    "sent_at": self.created.to_rfc3339_opts(SecondsFormat::Secs, true),
                    "source": "mallard-metrics",
                    "version": env!("CARGO_PKG_VERSION"),
                    "text": text,
                    "content": text,
                });
                if let (Some((key, data)), Some(map)) = (&self.data, obj.as_object_mut()) {
                    map.insert((*key).to_string(), data.clone());
                }
                obj
            }
        };
        serde_json::to_vec(&value).expect("a JSON value always serialises")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Alert {
        Alert {
            id: "flush_stalled".into(),
            severity: Severity::Critical,
            category: "operational",
            summary: "No successful flush for 12 minutes".into(),
            site_id: None,
            details: json!({"seconds_since_last_flush": 720}),
        }
    }

    #[test]
    fn generic_body_has_machine_fields_and_both_chat_fields() {
        let now = Utc::now();
        let n = Notification::for_alert(&sample(), AlertStatus::Firing, now, 1, now);
        let v: Value = serde_json::from_slice(&n.body(PayloadFormat::Generic)).unwrap();
        assert_eq!(v["schema"], SCHEMA);
        assert_eq!(v["kind"], "alert");
        assert_eq!(v["text"], v["content"]);
        assert_eq!(v["text"], "[CRITICAL] No successful flush for 12 minutes");
        assert_eq!(v["alert"]["id"], "flush_stalled");
        assert_eq!(v["alert"]["status"], "firing");
        assert_eq!(v["alert"]["severity"], "critical");
        assert_eq!(v["alert"]["details"]["seconds_since_last_flush"], 720);
    }

    #[test]
    fn chat_formats_send_only_the_fields_their_service_defines() {
        let now = Utc::now();
        let n = Notification::for_alert(&sample(), AlertStatus::Resolved, now, 2, now);
        let slack: Value = serde_json::from_slice(&n.body(PayloadFormat::Slack)).unwrap();
        assert_eq!(slack.as_object().unwrap().len(), 1);
        assert!(slack["text"].as_str().unwrap().starts_with("[RESOLVED]"));
        let discord: Value = serde_json::from_slice(&n.body(PayloadFormat::Discord)).unwrap();
        // `content` plus the documented `allowed_mentions` guard, nothing else.
        let mut keys: Vec<&str> = discord
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["allowed_mentions", "content"]);
        assert!(discord["content"].is_string());
        assert_eq!(
            discord["allowed_mentions"]["parse"],
            serde_json::json!([]),
            "Discord must be told to parse no mentions"
        );
    }

    #[test]
    fn text_is_cut_to_the_discord_limit() {
        let now = Utc::now();
        let n = Notification::for_report("é".repeat(5000), json!({}), now);
        let v: Value = serde_json::from_slice(&n.body(PayloadFormat::Discord)).unwrap();
        assert_eq!(
            v["content"].as_str().unwrap().chars().count(),
            MAX_TEXT_CHARS
        );
    }
}
