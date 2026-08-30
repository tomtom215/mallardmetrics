use super::QueryScope;
use duckdb::Connection;
use serde::{Deserialize, Serialize};

/// A single time bucket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimeBucket {
    pub date: String,
    pub visitors: u64,
    pub pageviews: u64,
}

/// Bucket size for a time series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    Hour,
    Day,
}

impl Granularity {
    const fn trunc_unit(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
        }
    }

    const fn format_str(self) -> &'static str {
        match self {
            Self::Hour => "%Y-%m-%d %H:00",
            Self::Day => "%Y-%m-%d",
        }
    }

    const fn step_interval(self) -> &'static str {
        match self {
            Self::Hour => "1 hour",
            Self::Day => "1 day",
        }
    }

    /// Cap on generated buckets, so a wide range cannot produce a huge series.
    const fn max_buckets(self) -> usize {
        match self {
            // 90 days of hourly buckets; the API caps ranges well below this.
            Self::Hour => 2_200,
            // A little over a year of daily buckets.
            Self::Day => 400,
        }
    }

    /// Choose a granularity that keeps the series readable for a given span.
    pub const fn for_span_days(days: i64) -> Self {
        if days <= 2 { Self::Hour } else { Self::Day }
    }
}

/// Query a time series, emitting a row for every bucket in range.
///
/// Buckets with no events are returned with zero counts. Previously they were
/// simply absent, so a chart drawn from the result connected the days on either
/// side of an outage and showed traffic that never happened.
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn query_timeseries(
    conn: &Connection,
    scope: &QueryScope,
    granularity: Granularity,
) -> Result<Vec<TimeBucket>, duckdb::Error> {
    let sql = timeseries_sql(granularity, scope);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(duckdb::params_from_iter(timeseries_params(scope)), |row| {
            Ok(TimeBucket {
                date: row.get(0)?,
                visitors: row.get(1)?,
                pageviews: row.get(2)?,
            })
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

/// SQL for the gap-filled time series. Split out so it can be unit-tested.
fn timeseries_sql(granularity: Granularity, scope: &QueryScope) -> String {
    let filters = scope.filter_clause();
    let trunc = granularity.trunc_unit();
    let fmt = granularity.format_str();
    let step = granularity.step_interval();
    let max_buckets = granularity.max_buckets();

    // The spine is generated from the requested bounds so that empty buckets
    // still appear. `end` is exclusive, so one microsecond is subtracted before
    // generating the series; otherwise a range ending exactly on a boundary
    // would emit a trailing bucket that the aggregate can never populate.
    format!(
        "WITH bounds AS (
             SELECT CAST(? AS TIMESTAMP) AS lo, CAST(? AS TIMESTAMP) AS hi
         ),
         spine AS (
             SELECT UNNEST(generate_series(
                 DATE_TRUNC('{trunc}', (SELECT lo FROM bounds)),
                 (SELECT hi FROM bounds) - INTERVAL '1 microsecond',
                 INTERVAL '{step}'
             )) AS bucket_ts
             LIMIT {max_buckets}
         ),
         agg AS (
             SELECT DATE_TRUNC('{trunc}', timestamp) AS bucket_ts,
                    COUNT(DISTINCT visitor_id) AS visitors,
                    COUNT(*) FILTER (WHERE event_name = 'pageview') AS pageviews
             FROM events_all
             WHERE site_id = ?
               AND timestamp >= (SELECT lo FROM bounds)
               AND timestamp <  (SELECT hi FROM bounds){filters}
             GROUP BY 1
         )
         SELECT STRFTIME(spine.bucket_ts, '{fmt}'),
                COALESCE(agg.visitors, 0),
                COALESCE(agg.pageviews, 0)
         FROM spine LEFT JOIN agg ON agg.bucket_ts = spine.bucket_ts
         ORDER BY spine.bucket_ts"
    )
}

/// Bind order for [`timeseries_sql`]: start, end, site_id, then filter values.
///
/// The spine CTE needs the bounds before the aggregate needs the site, so this
/// query's parameter order differs from [`QueryScope::params`].
fn timeseries_params(scope: &QueryScope) -> Vec<&str> {
    let mut out = Vec::with_capacity(3 + scope.filters.len());
    out.push(scope.start.as_str());
    out.push(scope.end.as_str());
    out.push(scope.site_id.as_str());
    out.extend(scope.filter_params());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_support::{TestDb, insert_pageview, scope};

    fn run(conn: &Connection, scope: &QueryScope, granularity: Granularity) -> Vec<TimeBucket> {
        query_timeseries(conn, scope, granularity).unwrap()
    }

    #[test]
    fn test_daily_timeseries() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&db.conn, "v1", "2024-01-15 14:00:00", "/");
        insert_pageview(&db.conn, "v2", "2024-01-16 10:00:00", "/");

        let buckets = run(
            &db.conn,
            &scope("2024-01-15", "2024-01-17"),
            Granularity::Day,
        );
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].date, "2024-01-15");
        assert_eq!(buckets[0].pageviews, 2);
        assert_eq!(buckets[0].visitors, 1);
        assert_eq!(buckets[1].date, "2024-01-16");
        assert_eq!(buckets[1].pageviews, 1);
    }

    #[test]
    fn test_empty_days_are_filled_with_zeros() {
        // Regression: days with no traffic used to be omitted entirely, so a
        // chart drawn from the result connected the surrounding days and
        // implied traffic that never happened.
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");

        let buckets = run(
            &db.conn,
            &scope("2024-01-14", "2024-01-18"),
            Granularity::Day,
        );
        assert_eq!(
            buckets.iter().map(|b| b.date.as_str()).collect::<Vec<_>>(),
            vec!["2024-01-14", "2024-01-15", "2024-01-16", "2024-01-17"]
        );
        assert_eq!(buckets[0].pageviews, 0);
        assert_eq!(buckets[1].pageviews, 1);
        assert_eq!(buckets[2].pageviews, 0);
        assert_eq!(buckets[3].pageviews, 0);
    }

    #[test]
    fn test_hourly_timeseries_fills_gaps() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&db.conn, "v1", "2024-01-15 10:30:00", "/");
        insert_pageview(&db.conn, "v1", "2024-01-15 12:00:00", "/");

        let buckets = run(
            &db.conn,
            &scope("2024-01-15 10:00:00", "2024-01-15 13:00:00"),
            Granularity::Hour,
        );
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].date, "2024-01-15 10:00");
        assert_eq!(buckets[0].pageviews, 2);
        assert_eq!(buckets[1].date, "2024-01-15 11:00");
        assert_eq!(buckets[1].pageviews, 0, "the quiet hour must still appear");
        assert_eq!(buckets[2].pageviews, 1);
    }

    #[test]
    fn test_empty_range_still_returns_buckets() {
        let db = TestDb::new();
        let buckets = run(
            &db.conn,
            &scope("2024-01-15", "2024-01-18"),
            Granularity::Day,
        );
        assert_eq!(buckets.len(), 3);
        assert!(buckets.iter().all(|b| b.pageviews == 0 && b.visitors == 0));
    }

    #[test]
    fn test_exclusive_end_bound_emits_no_trailing_bucket() {
        let db = TestDb::new();
        let buckets = run(
            &db.conn,
            &scope("2024-01-15", "2024-01-16"),
            Granularity::Day,
        );
        assert_eq!(buckets.len(), 1, "one day, not two: end is exclusive");
        assert_eq!(buckets[0].date, "2024-01-15");
    }

    #[test]
    fn test_events_outside_the_range_are_excluded() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-14 23:59:59", "/");
        insert_pageview(&db.conn, "v2", "2024-01-16 00:00:00", "/");
        let buckets = run(
            &db.conn,
            &scope("2024-01-15", "2024-01-16"),
            Granularity::Day,
        );
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].pageviews, 0);
    }

    #[test]
    fn test_custom_events_do_not_count_as_pageviews() {
        let db = TestDb::new();
        db.insert_event("v1", "2024-01-15 10:00:00", "signup", "/");
        let buckets = run(
            &db.conn,
            &scope("2024-01-15", "2024-01-16"),
            Granularity::Day,
        );
        assert_eq!(buckets[0].pageviews, 0);
        assert_eq!(buckets[0].visitors, 1, "the visitor is still counted");
    }

    #[test]
    fn test_granularity_selection() {
        assert_eq!(Granularity::for_span_days(0), Granularity::Hour);
        assert_eq!(Granularity::for_span_days(1), Granularity::Hour);
        assert_eq!(Granularity::for_span_days(2), Granularity::Hour);
        assert_eq!(Granularity::for_span_days(7), Granularity::Day);
        assert_eq!(Granularity::for_span_days(365), Granularity::Day);
    }

    #[test]
    fn test_bucket_count_is_capped() {
        // A pathological range must not generate an unbounded spine.
        let db = TestDb::new();
        let wide = QueryScope::new("test.com", "1990-01-01", "2030-01-01", "30 minutes");
        let buckets = run(&db.conn, &wide, Granularity::Day);
        assert!(
            buckets.len() <= Granularity::Day.max_buckets(),
            "generated {} buckets",
            buckets.len()
        );
    }

    #[test]
    fn test_sql_uses_only_bound_parameters() {
        let sql = timeseries_sql(Granularity::Day, &scope("2024-01-01", "2024-02-01"));
        assert_eq!(sql.matches('?').count(), 3);
    }
}
