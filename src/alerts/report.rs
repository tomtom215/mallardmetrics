//! Scheduled reports: a daily and/or weekly summary per site.
//!
//! The figures come from the same query functions the dashboard uses, run
//! through the API's query runner (concurrency cap, tier read lock, deadline).
//! Only aggregates leave the process: counts, page paths and referrer sources.

use crate::ingest::handler::AppState;
use crate::query::breakdowns::{self, Dimension};
use crate::query::metrics;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Timelike, Utc, Weekday};
use serde_json::{Value, json};
use std::sync::Arc;

/// How many pages and sources a report lists.
pub const TOP_N: usize = 5;

/// Which report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cadence {
    Daily,
    Weekly,
}

impl Cadence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
        }
    }

    const fn days(self) -> i64 {
        match self {
            Self::Daily => 1,
            Self::Weekly => 7,
        }
    }
}

/// The most recent date this report was due on, as of `now`.
///
/// A daily report is due each day at `hour` UTC. A weekly one is due on
/// `weekday` at `hour`. Before `hour` on a due day the previous one is still the
/// latest.
pub fn latest_due(cadence: Cadence, now: DateTime<Utc>, hour: u32, weekday: Weekday) -> NaiveDate {
    let mut date = now.date_naive();
    if now.hour() < hour {
        date -= Duration::days(1);
    }
    if cadence == Cadence::Weekly {
        while date.weekday() != weekday {
            date -= Duration::days(1);
        }
    }
    date
}

/// The period a report due on `due` covers: the whole days before it. Returns
/// `(start, end_exclusive, previous_start)`.
pub fn window(cadence: Cadence, due: NaiveDate) -> (NaiveDate, NaiveDate, NaiveDate) {
    let span = Duration::days(cadence.days());
    (due - span, due, due - span - span)
}

/// Percent change from `previous` to `current`, or `None` when there is nothing
/// to compare with.
#[allow(clippy::cast_precision_loss)]
pub fn change_percent(current: u64, previous: u64) -> Option<f64> {
    (previous > 0).then(|| {
        let pct = (current as f64 - previous as f64) / previous as f64 * 100.0;
        (pct * 10.0).round() / 10.0
    })
}

/// One site's figures for one period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub cadence: Cadence,
    pub site_id: String,
    pub start: NaiveDate,
    /// Inclusive last day.
    pub end: NaiveDate,
    pub visitors: u64,
    pub pageviews: u64,
    pub previous_visitors: u64,
    pub previous_pageviews: u64,
    /// `(path, pageviews, visitors)`.
    pub top_pages: Vec<(String, u64, u64)>,
    /// `(source, pageviews, visitors)`.
    pub top_sources: Vec<(String, u64, u64)>,
}

fn signed(pct: Option<f64>) -> String {
    pct.map_or_else(|| "n/a".to_string(), |p| format!("{p:+.1}%"))
}

impl Report {
    /// The generic machine-readable part.
    pub fn to_json(&self) -> Value {
        let list = |rows: &[(String, u64, u64)], key: &str| -> Vec<Value> {
            rows.iter()
                .map(|(value, pageviews, visitors)| {
                    json!({ key: value, "pageviews": pageviews, "visitors": visitors })
                })
                .collect()
        };
        json!({
            "period": self.cadence.as_str(),
            "site_id": self.site_id,
            "start_date": self.start.to_string(),
            "end_date": self.end.to_string(),
            "timezone": "UTC",
            "visitors": self.visitors,
            "pageviews": self.pageviews,
            "previous": {
                "visitors": self.previous_visitors,
                "pageviews": self.previous_pageviews,
            },
            "change_percent": {
                "visitors": change_percent(self.visitors, self.previous_visitors),
                "pageviews": change_percent(self.pageviews, self.previous_pageviews),
            },
            "top_pages": list(&self.top_pages, "path"),
            "top_sources": list(&self.top_sources, "source"),
        })
    }

    /// The short human summary used for the `text` / `content` fields.
    pub fn text(&self) -> String {
        let period = if self.start == self.end {
            self.start.to_string()
        } else {
            format!("{} to {}", self.start, self.end)
        };
        let ranked = |rows: &[(String, u64, u64)]| {
            if rows.is_empty() {
                "none".to_string()
            } else {
                rows.iter()
                    .map(|(v, pv, _)| format!("{v} ({pv})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        format!(
            "Mallard {} report for {} ({period}, UTC)\nVisitors: {} ({} vs previous period)\n\
             Pageviews: {} ({})\nTop pages: {}\nTop sources: {}",
            self.cadence.as_str(),
            self.site_id,
            self.visitors,
            signed(change_percent(self.visitors, self.previous_visitors)),
            self.pageviews,
            signed(change_percent(self.pageviews, self.previous_pageviews)),
            ranked(&self.top_pages),
            ranked(&self.top_sources),
        )
    }
}

/// Gather a report. Four statements, each through the query runner, so each
/// takes its own slot and has its own deadline.
///
/// # Errors
///
/// The runner's error text (busy, timed out, or a database error).
pub async fn fetch(
    state: &Arc<AppState>,
    site: &str,
    cadence: Cadence,
    due: NaiveDate,
) -> Result<Report, String> {
    let (start, end, previous_start) = window(cadence, due);
    let counts = |from: NaiveDate, to: NaiveDate| {
        let scope = state.scope(site, &from.to_string(), &to.to_string());
        async move {
            let m = crate::api::stats::run_scoped(state, move |conn, cold| {
                metrics::query_event_metrics(conn, &cold.scope(scope, 0))
            })
            .await
            .map_err(|e| e.to_string())?;
            Ok::<_, String>((m.unique_visitors, m.total_pageviews))
        }
    };
    let (visitors, pageviews) = counts(start, end).await?;
    let (previous_visitors, previous_pageviews) = counts(previous_start, start).await?;

    let top = |dimension: Dimension| {
        let scope = state.scope(site, &start.to_string(), &end.to_string());
        async move {
            let rows = crate::api::stats::run_scoped(state, move |conn, cold| {
                breakdowns::query_breakdown(conn, &cold.scope(scope, 0), dimension, TOP_N)
            })
            .await
            .map_err(|e| e.to_string())?;
            Ok::<_, String>(
                rows.into_iter()
                    .map(|r| (r.value, r.pageviews, r.visitors))
                    .collect::<Vec<_>>(),
            )
        }
    };
    let top_pages = top(Dimension::Page).await?;
    let top_sources = top(Dimension::ReferrerSource).await?;

    Ok(Report {
        cadence,
        site_id: site.to_string(),
        start,
        end: end - Duration::days(1),
        visitors,
        pageviews,
        previous_visitors,
        previous_pageviews,
        top_pages,
        top_sources,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(d: u32, h: u32, m: u32) -> DateTime<Utc> {
        // 2026-10-21 is a Wednesday.
        Utc.with_ymd_and_hms(2026, 10, d, h, m, 0).unwrap()
    }

    fn date(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    #[test]
    fn daily_is_due_at_the_hour_and_covers_the_day_before() {
        assert_eq!(
            latest_due(Cadence::Daily, at(21, 8, 0), 8, Weekday::Mon),
            date(21)
        );
        assert_eq!(
            latest_due(Cadence::Daily, at(21, 7, 59), 8, Weekday::Mon),
            date(20)
        );
        let (start, end, prev) = window(Cadence::Daily, date(21));
        assert_eq!((start, end, prev), (date(20), date(21), date(19)));
    }

    #[test]
    fn weekly_is_due_on_its_weekday() {
        // Monday 2026-10-19 is the latest Monday on Wednesday the 21st.
        assert_eq!(
            latest_due(Cadence::Weekly, at(21, 12, 0), 8, Weekday::Mon),
            date(19)
        );
        assert_eq!(
            latest_due(Cadence::Weekly, at(19, 7, 0), 8, Weekday::Mon),
            date(12)
        );
        assert_eq!(
            latest_due(Cadence::Weekly, at(19, 8, 0), 8, Weekday::Mon),
            date(19)
        );
        let (start, end, prev) = window(Cadence::Weekly, date(19));
        assert_eq!((start, end, prev), (date(12), date(19), date(5)));
    }

    #[test]
    fn change_is_absent_without_a_previous_period() {
        assert_eq!(change_percent(10, 0), None);
        assert_eq!(change_percent(150, 100), Some(50.0));
        assert_eq!(change_percent(75, 100), Some(-25.0));
        assert_eq!(change_percent(0, 100), Some(-100.0));
    }

    fn sample() -> Report {
        Report {
            cadence: Cadence::Daily,
            site_id: "example.com".into(),
            start: date(20),
            end: date(20),
            visitors: 120,
            pageviews: 300,
            previous_visitors: 100,
            previous_pageviews: 400,
            top_pages: vec![("/".into(), 200, 90), ("/pricing".into(), 50, 30)],
            top_sources: vec![("Google".into(), 60, 40)],
        }
    }

    #[test]
    fn json_and_text_agree() {
        let r = sample();
        let v = r.to_json();
        assert_eq!(v["period"], "daily");
        assert_eq!(v["visitors"], 120);
        assert_eq!(v["change_percent"]["visitors"], 20.0);
        assert_eq!(v["change_percent"]["pageviews"], -25.0);
        assert_eq!(v["top_pages"][1]["path"], "/pricing");
        assert_eq!(v["top_sources"][0]["source"], "Google");
        let text = r.text();
        assert!(text.contains("example.com"));
        assert!(text.contains("+20.0%"));
        assert!(text.contains("-25.0%"));
        assert!(text.contains("/pricing (50)"));
    }
}
