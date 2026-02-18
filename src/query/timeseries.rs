use duckdb::Connection;

/// A single time bucket with visitor and pageview counts.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TimeBucket {
    pub date: String,
    pub visitors: u64,
    pub pageviews: u64,
}

/// Time granularity for bucketing.
#[derive(Debug, Clone, Copy)]
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
}

/// Query time-series data for a site within a date range.
pub fn query_timeseries(
    conn: &Connection,
    site_id: &str,
    start_date: &str,
    end_date: &str,
    granularity: Granularity,
) -> Result<Vec<TimeBucket>, duckdb::Error> {
    let trunc = granularity.trunc_unit();
    let fmt = granularity.format_str();

    let sql = format!(
        "SELECT strftime(DATE_TRUNC('{trunc}', timestamp), '{fmt}') AS bucket,
                COUNT(DISTINCT visitor_id) AS visitors,
                COUNT(*) FILTER (WHERE event_name = 'pageview') AS pageviews
         FROM events
         WHERE site_id = ? AND timestamp >= CAST(? AS TIMESTAMP) AND timestamp < CAST(? AS TIMESTAMP)
         GROUP BY bucket
         ORDER BY bucket"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(duckdb::params![site_id, start_date, end_date], |row| {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        conn
    }

    fn insert_pageview(conn: &Connection, timestamp: &str) {
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
             VALUES ('test.com', 'v1', CAST(? AS TIMESTAMP), 'pageview', '/')",
            duckdb::params![timestamp],
        )
        .unwrap();
    }

    #[test]
    fn test_daily_timeseries() {
        let conn = setup_test_db();
        insert_pageview(&conn, "2024-01-15 10:00:00");
        insert_pageview(&conn, "2024-01-15 14:00:00");
        insert_pageview(&conn, "2024-01-16 10:00:00");

        let buckets = query_timeseries(
            &conn,
            "test.com",
            "2024-01-01",
            "2024-02-01",
            Granularity::Day,
        )
        .unwrap();

        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].date, "2024-01-15");
        assert_eq!(buckets[0].pageviews, 2);
        assert_eq!(buckets[1].date, "2024-01-16");
        assert_eq!(buckets[1].pageviews, 1);
    }

    #[test]
    fn test_hourly_timeseries() {
        let conn = setup_test_db();
        insert_pageview(&conn, "2024-01-15 10:00:00");
        insert_pageview(&conn, "2024-01-15 10:30:00");
        insert_pageview(&conn, "2024-01-15 14:00:00");

        let buckets = query_timeseries(
            &conn,
            "test.com",
            "2024-01-01",
            "2024-02-01",
            Granularity::Hour,
        )
        .unwrap();

        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].date, "2024-01-15 10:00");
        assert_eq!(buckets[0].pageviews, 2);
        assert_eq!(buckets[1].date, "2024-01-15 14:00");
    }

    #[test]
    fn test_empty_timeseries() {
        let conn = setup_test_db();
        let buckets = query_timeseries(
            &conn,
            "test.com",
            "2024-01-01",
            "2024-02-01",
            Granularity::Day,
        )
        .unwrap();

        assert!(buckets.is_empty());
    }
}
