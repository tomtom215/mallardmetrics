use super::Filter;
use chrono::{NaiveDateTime, Utc};
use duckdb::Connection;
use serde::{Deserialize, Serialize};

/// A snapshot of activity in the last few minutes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealtimeSnapshot {
    /// Distinct visitors seen inside the window.
    pub current_visitors: u64,
    /// Pageviews inside the window.
    pub pageviews: u64,
    /// Length of the window, in minutes.
    pub window_minutes: u32,
    /// Most-viewed pages inside the window.
    pub top_pages: Vec<RealtimeEntry>,
    /// Most common acquisition sources inside the window.
    pub top_sources: Vec<RealtimeEntry>,
    /// Per-minute pageview counts, oldest first, one entry per minute.
    pub per_minute: Vec<u64>,
}

/// A ranked entry in a realtime list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealtimeEntry {
    pub value: String,
    pub visitors: u64,
}

/// How many rows each realtime list holds.
const TOP_N: usize = 10;

/// The site, window and segment every realtime sub-query shares.
///
/// Deliberately not a [`QueryScope`](super::QueryScope): that type's upper
/// bound is exclusive, which is right for a report over whole days but wrong
/// here. "Right now" ends at this instant, and an event stamped exactly `until`
/// belongs inside the window — the per-minute series generates a bucket for
/// that minute, so excluding the event would leave the series disagreeing with
/// the totals beside it. The segment predicates are borrowed from `QueryScope`
/// unchanged, so a filter means the same thing on every endpoint.
struct RealtimeScope {
    site_id: String,
    since: NaiveDateTime,
    until: NaiveDateTime,
    filters: super::QueryScope,
}

impl RealtimeScope {
    fn new(site_id: &str, since: NaiveDateTime, until: NaiveDateTime, filters: &[Filter]) -> Self {
        Self {
            site_id: site_id.to_string(),
            since,
            until,
            // Only the filter half of this scope is ever read; the dates and
            // session window on it are unused.
            filters: super::QueryScope::new(site_id, "", "", "30 minutes")
                .with_filters(filters.to_vec()),
        }
    }

    /// `site_id = ? AND timestamp BETWEEN ? AND ?`, plus the segment.
    fn where_clause(&self) -> String {
        format!(
            "site_id = ? AND timestamp >= ? AND timestamp <= ?{}",
            self.filters.filter_clause()
        )
    }

    /// Bound values in the order [`Self::where_clause`] spells them.
    fn params(&self) -> Vec<String> {
        let mut out = vec![
            self.site_id.clone(),
            self.since.to_string(),
            self.until.to_string(),
        ];
        out.extend(self.filters.filter_params().map(str::to_string));
        out
    }
}

/// Query current activity for a site.
///
/// "Right now" is the last `window_minutes`, ending at the current UTC instant.
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn query_realtime(
    conn: &Connection,
    site_id: &str,
    window_minutes: u32,
    filters: &[Filter],
) -> Result<RealtimeSnapshot, duckdb::Error> {
    query_realtime_at(
        conn,
        site_id,
        window_minutes,
        filters,
        Utc::now().naive_utc(),
    )
}

/// [`query_realtime`] with the end of the window supplied explicitly.
///
/// The window end is a bound parameter rather than SQL's `NOW()` for two
/// reasons. `NOW()` returns `TIMESTAMP WITH TIME ZONE`, and subtracting an
/// `INTERVAL` from that type is implemented by DuckDB's ICU extension — which
/// this build does not load, so every realtime query failed at bind time.
/// Casting it back would not fix the second problem: the cast uses the session
/// time zone, which follows the *host's* locale, while ingestion timestamps are
/// naive UTC. On any server not set to UTC the window would silently slide by
/// the UTC offset.
///
/// Computing the instant once in Rust also keeps the four sub-queries
/// consistent with each other; separate `NOW()` calls could straddle a minute
/// boundary and report a per-minute series that disagreed with the totals.
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn query_realtime_at(
    conn: &Connection,
    site_id: &str,
    window_minutes: u32,
    filters: &[Filter],
    now: NaiveDateTime,
) -> Result<RealtimeSnapshot, duckdb::Error> {
    let window = window_minutes.max(1);
    let since = now - chrono::Duration::minutes(i64::from(window));

    // The realtime window is its own range, so it does not reuse a request's
    // `QueryScope` dates — but it does reuse the segment, so that clicking a
    // breakdown row narrows "right now" the same way it narrows every other
    // panel. Without this the endpoint quietly ignored `filters` while the API
    // documentation promised every stats endpoint honoured them.
    let scope = RealtimeScope::new(site_id, since, now, filters);

    let sql = format!(
        "SELECT COUNT(DISTINCT visitor_id),
                COUNT(*) FILTER (WHERE event_name = 'pageview')
         FROM events_all
         WHERE {}",
        scope.where_clause()
    );
    let mut stmt = conn.prepare(&sql)?;
    let (current_visitors, pageviews): (u64, u64) = stmt
        .query_row(duckdb::params_from_iter(scope.params()), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    drop(stmt);

    let top_pages = query_top(conn, &scope, "pathname")?;
    let top_sources = query_top(conn, &scope, "referrer_source")?;
    let per_minute = query_per_minute(conn, &scope, since, now)?;

    Ok(RealtimeSnapshot {
        current_visitors,
        pageviews,
        window_minutes: window,
        top_pages,
        top_sources,
        per_minute,
    })
}

/// Top values of one column inside the realtime window.
///
/// `column` is a fixed identifier chosen by the caller, never request input.
fn query_top(
    conn: &Connection,
    scope: &RealtimeScope,
    column: &str,
) -> Result<Vec<RealtimeEntry>, duckdb::Error> {
    let where_clause = scope.where_clause();
    let sql = format!(
        "SELECT COALESCE({column}, '(direct)') AS value, COUNT(DISTINCT visitor_id) AS visitors
         FROM events_all
         WHERE {where_clause}
         GROUP BY value
         ORDER BY visitors DESC, value
         LIMIT {TOP_N}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(duckdb::params_from_iter(scope.params()), |row| {
            Ok(RealtimeEntry {
                value: row.get(0)?,
                visitors: row.get(1)?,
            })
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

/// Per-minute pageview counts across the window, gap-filled and oldest first.
fn query_per_minute(
    conn: &Connection,
    scope: &RealtimeScope,
    since: NaiveDateTime,
    until: NaiveDateTime,
) -> Result<Vec<u64>, duckdb::Error> {
    let where_clause = scope.where_clause();
    let sql = format!(
        "WITH spine AS (
             SELECT UNNEST(generate_series(
                 DATE_TRUNC('minute', CAST(? AS TIMESTAMP)),
                 DATE_TRUNC('minute', CAST(? AS TIMESTAMP)),
                 INTERVAL '1 minute'
             )) AS bucket
         ),
         agg AS (
             SELECT DATE_TRUNC('minute', timestamp) AS bucket,
                    COUNT(*) FILTER (WHERE event_name = 'pageview') AS pageviews
             FROM events_all
             WHERE {where_clause}
             GROUP BY 1
         )
         SELECT COALESCE(agg.pageviews, 0)
         FROM spine LEFT JOIN agg ON agg.bucket = spine.bucket
         ORDER BY spine.bucket"
    );
    let mut stmt = conn.prepare(&sql)?;
    // The spine's two bounds bind first, then the scope's own parameters.
    let mut bound: Vec<String> = vec![since.to_string(), until.to_string()];
    bound.extend(scope.params());
    let rows = stmt
        .query_map(duckdb::params_from_iter(bound), |row| row.get::<_, u64>(0))?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_support::TestDb;

    /// A fixed "now" so the window boundaries are exact.
    ///
    /// The tests used to insert `NOW() - INTERVAL 'n minutes'` and query against
    /// the wall clock, which made an event inserted at `:59.9` land in a
    /// different minute bucket from the spine built a millisecond later.
    fn now() -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 3, 14)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap()
    }

    /// Insert an event `minutes_ago` before [`now`].
    fn insert_relative(db: &TestDb, visitor: &str, minutes_ago: i64, pathname: &str) {
        let ts = now() - chrono::Duration::minutes(minutes_ago);
        db.conn
            .execute(
                "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
                 VALUES ('test.com', ?, ?, 'pageview', ?)",
                duckdb::params![visitor, ts, pathname],
            )
            .unwrap();
    }

    fn snapshot(db: &TestDb, window: u32) -> RealtimeSnapshot {
        query_realtime_at(&db.conn, "test.com", window, &[], now()).unwrap()
    }

    #[test]
    fn test_realtime_counts_recent_visitors() {
        let db = TestDb::new();
        insert_relative(&db, "v1", 1, "/");
        insert_relative(&db, "v2", 2, "/pricing");
        insert_relative(&db, "v1", 3, "/about");

        let snap = snapshot(&db, 5);
        assert_eq!(snap.current_visitors, 2);
        assert_eq!(snap.pageviews, 3);
        assert_eq!(snap.window_minutes, 5);
    }

    #[test]
    fn test_realtime_excludes_events_outside_the_window() {
        let db = TestDb::new();
        insert_relative(&db, "recent", 1, "/");
        insert_relative(&db, "old", 60, "/");

        assert_eq!(snapshot(&db, 5).current_visitors, 1);
    }

    #[test]
    fn test_realtime_window_boundary_is_inclusive_at_both_ends() {
        let db = TestDb::new();
        insert_relative(&db, "exactly_at_the_edge", 5, "/");
        insert_relative(&db, "just_outside", 6, "/");
        insert_relative(&db, "right_now", 0, "/");

        assert_eq!(snapshot(&db, 5).current_visitors, 2);
    }

    #[test]
    fn test_realtime_ignores_events_in_the_future() {
        // A client with a skewed clock cannot inflate "right now" — the window
        // is bounded above as well as below.
        let db = TestDb::new();
        insert_relative(&db, "v1", 1, "/");
        insert_relative(&db, "time_traveller", -120, "/");

        assert_eq!(snapshot(&db, 5).current_visitors, 1);
    }

    #[test]
    fn test_realtime_is_empty_without_traffic() {
        let db = TestDb::new();
        let snap = snapshot(&db, 5);
        assert_eq!(snap.current_visitors, 0);
        assert_eq!(snap.pageviews, 0);
        assert!(snap.top_pages.is_empty());
    }

    #[test]
    fn test_realtime_top_pages_are_ranked() {
        let db = TestDb::new();
        insert_relative(&db, "v1", 1, "/popular");
        insert_relative(&db, "v2", 1, "/popular");
        insert_relative(&db, "v3", 1, "/quiet");

        let snap = snapshot(&db, 5);
        assert_eq!(snap.top_pages[0].value, "/popular");
        assert_eq!(snap.top_pages[0].visitors, 2);
        assert_eq!(snap.top_pages[1].value, "/quiet");
    }

    #[test]
    fn test_realtime_sources_default_to_direct() {
        let db = TestDb::new();
        insert_relative(&db, "v1", 1, "/");
        assert_eq!(snapshot(&db, 5).top_sources[0].value, "(direct)");
    }

    #[test]
    fn test_per_minute_series_covers_the_whole_window() {
        let db = TestDb::new();
        insert_relative(&db, "v1", 0, "/");
        let snap = snapshot(&db, 5);
        // generate_series is inclusive at both ends, so a 5-minute window has 6
        // minute boundaries.
        assert_eq!(snap.per_minute.len(), 6);
        assert_eq!(snap.per_minute.iter().sum::<u64>(), 1);
    }

    #[test]
    fn test_per_minute_series_places_events_in_the_right_bucket() {
        let db = TestDb::new();
        insert_relative(&db, "v1", 4, "/");
        insert_relative(&db, "v2", 4, "/");
        insert_relative(&db, "v3", 0, "/");

        // Oldest first: minute -5, -4, -3, -2, -1, 0.
        assert_eq!(snapshot(&db, 5).per_minute, vec![0, 2, 0, 0, 0, 1]);
    }

    #[test]
    fn test_per_minute_totals_agree_with_the_headline_pageviews() {
        let db = TestDb::new();
        for minutes_ago in 0..5 {
            insert_relative(&db, "v1", minutes_ago, "/");
        }
        let snap = snapshot(&db, 5);
        assert_eq!(snap.per_minute.iter().sum::<u64>(), snap.pageviews);
    }

    #[test]
    fn test_zero_window_is_clamped() {
        let db = TestDb::new();
        assert_eq!(snapshot(&db, 0).window_minutes, 1);
    }

    #[test]
    fn test_realtime_is_scoped_to_the_site() {
        let db = TestDb::new();
        insert_relative(&db, "v1", 1, "/");
        db.conn
            .execute(
                "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
                 VALUES ('other.com', 'v9', ?, 'pageview', '/')",
                duckdb::params![now()],
            )
            .unwrap();
        assert_eq!(snapshot(&db, 5).current_visitors, 1);
    }

    #[test]
    fn test_realtime_uses_the_wall_clock_by_default() {
        // `query_realtime` must reach the same rows as `query_realtime_at` with
        // the current instant, or the public entry point is untested.
        let db = TestDb::new();
        let ts = Utc::now().naive_utc() - chrono::Duration::seconds(30);
        db.conn
            .execute(
                "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
                 VALUES ('test.com', 'v1', ?, 'pageview', '/')",
                duckdb::params![ts],
            )
            .unwrap();
        let snap = query_realtime(&db.conn, "test.com", 5, &[]).unwrap();
        assert_eq!(snap.current_visitors, 1);
        assert_eq!(snap.pageviews, 1);
    }
}
