//! Analytical alerts: did this site's traffic change abnormally?
//!
//! The question asked is about one clock hour. Once an hour has finished (plus a
//! grace for the flush interval), its pageview count is compared with the mean
//! of the same hour-of-week over the previous N weeks. Aligned whole hours,
//! rather than a sliding window, because with `round_timestamps` every event is
//! stamped at the top of its hour and a sliding window would see all or nothing.
//!
//! [`judge`] is pure; [`fetch`] runs the one statement through the same query
//! runner the API uses, so it queues behind the concurrency cap, holds the tier
//! read lock and is interrupted at the same deadline.

use super::payload::{Alert, Severity};
use super::rules::Observation;
use crate::ingest::handler::AppState;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, SecondsFormat, Timelike, Utc};
use serde_json::json;
use std::sync::Arc;

/// Fewest baseline weeks (with the site alive) needed to judge at all.
pub const MIN_BASELINE_WEEKS: usize = 2;

/// An alert clears only once the change falls below this fraction of its
/// threshold, so a value hovering at the threshold does not flap.
pub const CLEAR_FRACTION: f64 = 0.8;

/// The hour to judge at `now`: the last whole hour that ended at least `grace`
/// ago.
pub fn target_hour(now: DateTime<Utc>, grace: Duration) -> NaiveDateTime {
    let settled = now - grace;
    let top = settled
        .with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(settled);
    (top - Duration::hours(1)).naive_utc()
}

/// Counts for the judged hour and its predecessors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub hour: NaiveDateTime,
    pub current: u64,
    /// Same hour-of-week, 1..=N weeks earlier, only for weeks in which the site
    /// had any pageview that day (a site that did not exist yet is not "zero").
    pub baseline: Vec<u64>,
    pub weeks_requested: usize,
}

/// Thresholds.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// 0 disables.
    pub drop_percent: u32,
    /// 0 disables.
    pub rise_percent: u32,
    /// The baseline mean must reach this, or the site is too quiet to judge.
    pub min_pageviews: u64,
}

/// Which alerts are firing now, so the verdict can apply hysteresis.
#[derive(Debug, Clone, Copy, Default)]
pub struct Firing {
    pub drop: bool,
    pub rise: bool,
}

fn id(kind: &str, site: &str) -> String {
    format!("{kind}:{site}")
}

/// Alert ids for a site.
pub fn drop_id(site: &str) -> String {
    id("traffic_drop", site)
}

pub fn rise_id(site: &str) -> String {
    id("traffic_rise", site)
}

/// Judge one sample. Returns observations only for alerts it can decide.
#[allow(clippy::cast_precision_loss)]
pub fn judge(site: &str, s: &Sample, p: &Params, firing: Firing) -> Vec<(String, Observation)> {
    let mut out = Vec::new();
    if s.baseline.len() < MIN_BASELINE_WEEKS {
        // Not enough history to know what normal is: say nothing either way.
        return out;
    }
    let mean = s.baseline.iter().sum::<u64>() as f64 / s.baseline.len() as f64;
    let quiet = mean < p.min_pageviews as f64;
    // Percent change of this hour against the baseline mean; negative is a drop.
    let change = if mean > 0.0 {
        (s.current as f64 - mean) / mean * 100.0
    } else {
        0.0
    };
    let hour = s.hour.and_utc().to_rfc3339_opts(SecondsFormat::Secs, true);
    let details = |threshold: u32| {
        json!({
            "hour_start": hour,
            "pageviews": s.current,
            "baseline_mean": (mean * 10.0).round() / 10.0,
            "baseline_weeks_used": s.baseline.len(),
            "baseline_weeks_requested": s.weeks_requested,
            "change_percent": (change * 10.0).round() / 10.0,
            "threshold_percent": threshold,
            "min_pageviews": p.min_pageviews,
        })
    };
    let when = s.hour.format("%Y-%m-%d %H:00 UTC");

    if p.drop_percent > 0 {
        let threshold = f64::from(p.drop_percent);
        let id = drop_id(site);
        if quiet {
            out.push((id, Observation::Clear));
        } else if -change >= threshold {
            out.push((
                id.clone(),
                Observation::Firing(Alert {
                    id,
                    severity: Severity::Warning,
                    category: "analytical",
                    summary: format!(
                        "{site}: {} pageviews in the hour from {when}, down {:.0}% from the \
                         {mean:.0} average of that hour over the previous {} week(s)",
                        s.current,
                        -change,
                        s.baseline.len()
                    ),
                    site_id: Some(site.to_string()),
                    details: details(p.drop_percent),
                }),
            ));
        } else if firing.drop && -change < threshold * CLEAR_FRACTION {
            out.push((id, Observation::Clear));
        }
    }
    if p.rise_percent > 0 {
        let threshold = f64::from(p.rise_percent);
        let id = rise_id(site);
        if quiet {
            out.push((id, Observation::Clear));
        } else if change >= threshold {
            out.push((
                id.clone(),
                Observation::Firing(Alert {
                    id,
                    severity: Severity::Info,
                    category: "analytical",
                    summary: format!(
                        "{site}: {} pageviews in the hour from {when}, up {change:.0}% on the \
                         {mean:.0} average of that hour over the previous {} week(s)",
                        s.current,
                        s.baseline.len()
                    ),
                    site_id: Some(site.to_string()),
                    details: details(p.rise_percent),
                }),
            ));
        } else if firing.rise && change < threshold * CLEAR_FRACTION {
            out.push((id, Observation::Clear));
        }
    }
    out
}

/// Count pageviews for `hour` and the same hour in each of the previous `weeks`
/// weeks, with one statement over only the days involved.
///
/// # Errors
///
/// The query runner's error text (busy, timed out, or a database error).
pub async fn fetch(
    state: &Arc<AppState>,
    site: &str,
    hour: NaiveDateTime,
    weeks: u32,
) -> Result<Sample, String> {
    let site_id = site.to_string();
    let n = weeks as usize;
    let counts = crate::api::stats::run_scoped(state, move |conn, cold| {
        let starts: Vec<NaiveDateTime> = (0..=n)
            .map(|w| hour - Duration::weeks(i64::try_from(w).unwrap_or(0)))
            .collect();
        let days: Vec<NaiveDate> = starts.iter().map(NaiveDateTime::date).collect();
        let source = cold.source_for_days(&site_id, &days);
        let ts = |t: NaiveDateTime| t.format("%Y-%m-%d %H:%M:%S").to_string();

        // Column 0 is the judged hour; columns 1..=n the same hour in earlier
        // weeks; columns n+1..=2n the whole of those days, which say whether the
        // site existed. Dates are bound parameters like everywhere else.
        let mut select =
            vec!["COUNT(*) FILTER (WHERE timestamp >= ? AND timestamp < ?)".to_string()];
        let mut params: Vec<String> = Vec::new();
        let window = |from: NaiveDateTime, to: NaiveDateTime, params: &mut Vec<String>| {
            params.push(ts(from));
            params.push(ts(to));
        };
        window(starts[0], starts[0] + Duration::hours(1), &mut params);
        for start in &starts[1..] {
            select.push("COUNT(*) FILTER (WHERE timestamp >= ? AND timestamp < ?)".to_string());
            window(*start, *start + Duration::hours(1), &mut params);
        }
        for start in &starts[1..] {
            select.push("COUNT(*) FILTER (WHERE timestamp >= ? AND timestamp < ?)".to_string());
            let day = start.date().and_hms_opt(0, 0, 0).unwrap_or(*start);
            window(day, day + Duration::days(1), &mut params);
        }
        let mut wheres = Vec::new();
        params.push(site_id.clone());
        for start in &starts[1..] {
            wheres.push("(timestamp >= ? AND timestamp < ?)");
            let day = start.date().and_hms_opt(0, 0, 0).unwrap_or(*start);
            window(day, day + Duration::days(1), &mut params);
        }
        wheres.push("(timestamp >= ? AND timestamp < ?)");
        window(starts[0], starts[0] + Duration::hours(1), &mut params);

        let sql = format!(
            "SELECT {} FROM {source} WHERE site_id = ? AND event_name = 'pageview' AND ({})",
            select.join(", "),
            wheres.join(" OR ")
        );
        let mut stmt = conn.prepare(&sql)?;
        stmt.query_row(duckdb::params_from_iter(params.iter()), |row| {
            (0..=2 * n)
                .map(|i| row.get::<_, i64>(i))
                .collect::<Result<Vec<_>, _>>()
        })
    })
    .await
    .map_err(|e| e.to_string())?;

    let to_u = |v: i64| u64::try_from(v).unwrap_or(0);
    let baseline = (1..=n)
        .filter(|&w| counts[n + w] > 0)
        .map(|w| to_u(counts[w]))
        .collect();
    Ok(Sample {
        hour,
        current: to_u(counts[0]),
        baseline,
        weeks_requested: n,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const P: Params = Params {
        drop_percent: 50,
        rise_percent: 100,
        min_pageviews: 50,
    };

    fn sample(current: u64, baseline: &[u64]) -> Sample {
        Sample {
            hour: Utc
                .with_ymd_and_hms(2026, 10, 21, 5, 0, 0)
                .unwrap()
                .naive_utc(),
            current,
            baseline: baseline.to_vec(),
            weeks_requested: 4,
        }
    }

    fn verdict(out: &[(String, Observation)], id: &str) -> &'static str {
        match out.iter().find(|(i, _)| i == id).map(|(_, o)| o) {
            Some(Observation::Firing(_)) => "firing",
            Some(Observation::Clear) => "clear",
            None => "silent",
        }
    }

    #[test]
    fn target_hour_is_the_last_whole_hour_past_the_grace() {
        let t = |h, m| Utc.with_ymd_and_hms(2026, 10, 21, h, m, 0).unwrap();
        let grace = Duration::minutes(2);
        let hour = |h| t(h, 0).naive_utc();
        assert_eq!(target_hour(t(6, 2), grace), hour(5));
        assert_eq!(
            target_hour(t(6, 1), grace),
            hour(4),
            "still inside the grace"
        );
        assert_eq!(target_hour(t(6, 59), grace), hour(5));
        assert_eq!(target_hour(t(0, 5), grace), hour(23) - Duration::days(1));
    }

    #[test]
    fn a_drop_at_the_threshold_fires_and_just_under_does_not() {
        // mean 100: 50 is exactly -50%.
        let out = judge(
            "s",
            &sample(50, &[100, 100, 100, 100]),
            &P,
            Firing::default(),
        );
        assert_eq!(verdict(&out, &drop_id("s")), "firing");
        let out = judge(
            "s",
            &sample(51, &[100, 100, 100, 100]),
            &P,
            Firing::default(),
        );
        assert_eq!(verdict(&out, &drop_id("s")), "silent");
    }

    #[test]
    fn the_volume_floor_keeps_quiet_sites_silent() {
        // 3 -> 1 is -67% but nobody should be paged for it.
        let out = judge("s", &sample(1, &[3, 3, 3, 3]), &P, Firing::default());
        assert_eq!(verdict(&out, &drop_id("s")), "clear");
        assert_eq!(verdict(&out, &rise_id("s")), "clear");
        // A mean of exactly the floor is judged.
        let out = judge("s", &sample(10, &[50, 50, 50, 50]), &P, Firing::default());
        assert_eq!(verdict(&out, &drop_id("s")), "firing");
        let out = judge("s", &sample(10, &[49, 49, 49, 49]), &P, Firing::default());
        assert_eq!(verdict(&out, &drop_id("s")), "clear");
    }

    #[test]
    fn a_rise_needs_its_own_threshold() {
        let out = judge(
            "s",
            &sample(200, &[100, 100, 100, 100]),
            &P,
            Firing::default(),
        );
        assert_eq!(verdict(&out, &rise_id("s")), "firing");
        let out = judge(
            "s",
            &sample(199, &[100, 100, 100, 100]),
            &P,
            Firing::default(),
        );
        assert_eq!(verdict(&out, &rise_id("s")), "silent");
        let off = Params {
            rise_percent: 0,
            drop_percent: 0,
            ..P
        };
        assert!(judge("s", &sample(1000, &[100, 100]), &off, Firing::default()).is_empty());
    }

    #[test]
    fn hysteresis_holds_between_eighty_percent_and_the_threshold() {
        let firing = Firing {
            drop: true,
            rise: false,
        };
        // -45% is under the 50% threshold but above 80% of it (40%): hold.
        let out = judge("s", &sample(55, &[100, 100, 100, 100]), &P, firing);
        assert_eq!(verdict(&out, &drop_id("s")), "silent");
        // -39% is clear of it.
        let out = judge("s", &sample(61, &[100, 100, 100, 100]), &P, firing);
        assert_eq!(verdict(&out, &drop_id("s")), "clear");
    }

    #[test]
    fn too_little_history_decides_nothing() {
        let out = judge(
            "s",
            &sample(0, &[1000]),
            &P,
            Firing {
                drop: true,
                rise: true,
            },
        );
        assert!(out.is_empty());
        let out = judge("s", &sample(0, &[]), &P, Firing::default());
        assert!(out.is_empty());
    }

    #[test]
    fn details_are_machine_readable() {
        let out = judge(
            "s",
            &sample(20, &[100, 100, 100, 100]),
            &P,
            Firing::default(),
        );
        let Observation::Firing(alert) = &out[0].1 else {
            panic!("expected a drop")
        };
        assert_eq!(alert.id, "traffic_drop:s");
        assert_eq!(alert.details["pageviews"], 20);
        assert_eq!(alert.details["baseline_mean"], 100.0);
        assert_eq!(alert.details["change_percent"], -80.0);
        assert_eq!(alert.details["hour_start"], "2026-10-21T05:00:00Z");
    }
}
