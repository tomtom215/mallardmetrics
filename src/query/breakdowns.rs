use duckdb::Connection;

/// A breakdown row: dimension value + count.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BreakdownRow {
    pub value: String,
    pub visitors: u64,
    pub pageviews: u64,
}

/// Available breakdown dimensions.
#[derive(Debug, Clone, Copy)]
pub enum Dimension {
    Page,
    ReferrerSource,
    CountryCode,
    Browser,
    Os,
    DeviceType,
}

impl Dimension {
    const fn column_name(self) -> &'static str {
        match self {
            Self::Page => "pathname",
            Self::ReferrerSource => "referrer_source",
            Self::CountryCode => "country_code",
            Self::Browser => "browser",
            Self::Os => "os",
            Self::DeviceType => "device_type",
        }
    }
}

/// Query a breakdown of events by a given dimension.
pub fn query_breakdown(
    conn: &Connection,
    site_id: &str,
    start_date: &str,
    end_date: &str,
    dimension: Dimension,
    limit: usize,
) -> Result<Vec<BreakdownRow>, duckdb::Error> {
    let col = dimension.column_name();

    // Using format! for column name is safe here since it comes from a fixed enum
    let sql = format!(
        "SELECT COALESCE({col}, '(unknown)') AS dim_value,
                COUNT(DISTINCT visitor_id) AS visitors,
                COUNT(*) FILTER (WHERE event_name = 'pageview') AS pageviews
         FROM events_all
         WHERE site_id = ? AND timestamp >= CAST(? AS TIMESTAMP) AND timestamp < CAST(? AS TIMESTAMP)
         GROUP BY dim_value
         ORDER BY visitors DESC
         LIMIT ?"
    );

    let mut stmt = conn.prepare(&sql)?;
    let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = stmt
        .query_map(
            duckdb::params![site_id, start_date, end_date, limit_i64],
            |row| {
                Ok(BreakdownRow {
                    value: row.get(0)?,
                    visitors: row.get(1)?,
                    pageviews: row.get(2)?,
                })
            },
        )?
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
        let dir = tempfile::tempdir().unwrap();
        crate::storage::schema::setup_query_view(&conn, dir.path()).unwrap();
        drop(dir); // view was already created; TempDir no longer needed
        conn
    }

    fn insert_event(conn: &Connection, visitor_id: &str, pathname: &str, browser: Option<&str>) {
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname, browser)
             VALUES ('test.com', ?, '2024-01-15 10:00:00', 'pageview', ?, ?)",
            duckdb::params![visitor_id, pathname, browser],
        )
        .unwrap();
    }

    #[test]
    fn test_breakdown_by_page() {
        let conn = setup_test_db();
        insert_event(&conn, "v1", "/", None);
        insert_event(&conn, "v1", "/about", None);
        insert_event(&conn, "v2", "/", None);

        let rows = query_breakdown(
            &conn,
            "test.com",
            "2024-01-01",
            "2024-02-01",
            Dimension::Page,
            10,
        )
        .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].value, "/");
        assert_eq!(rows[0].visitors, 2);
        assert_eq!(rows[0].pageviews, 2);
    }

    #[test]
    fn test_breakdown_by_browser() {
        let conn = setup_test_db();
        insert_event(&conn, "v1", "/", Some("Chrome"));
        insert_event(&conn, "v2", "/", Some("Firefox"));
        insert_event(&conn, "v3", "/", Some("Chrome"));

        let rows = query_breakdown(
            &conn,
            "test.com",
            "2024-01-01",
            "2024-02-01",
            Dimension::Browser,
            10,
        )
        .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].value, "Chrome");
        assert_eq!(rows[0].visitors, 2);
    }

    #[test]
    fn test_breakdown_limit() {
        let conn = setup_test_db();
        insert_event(&conn, "v1", "/a", None);
        insert_event(&conn, "v2", "/b", None);
        insert_event(&conn, "v3", "/c", None);

        let rows = query_breakdown(
            &conn,
            "test.com",
            "2024-01-01",
            "2024-02-01",
            Dimension::Page,
            2,
        )
        .unwrap();

        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_breakdown_empty() {
        let conn = setup_test_db();
        let rows = query_breakdown(
            &conn,
            "test.com",
            "2024-01-01",
            "2024-02-01",
            Dimension::Page,
            10,
        )
        .unwrap();

        assert!(rows.is_empty());
    }

    #[test]
    fn test_breakdown_null_values() {
        let conn = setup_test_db();
        insert_event(&conn, "v1", "/", None); // browser is NULL

        let rows = query_breakdown(
            &conn,
            "test.com",
            "2024-01-01",
            "2024-02-01",
            Dimension::Browser,
            10,
        )
        .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, "(unknown)");
    }
}
