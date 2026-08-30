//! Shared fixtures for query-layer tests.
//!
//! Behavioral-extension tests are the reason this exists. The previous suite
//! wrote them as `if let Ok(x) = query(...) { assert!(...) }`, which passes
//! whether or not the extension loads — so a regression in the funnel,
//! retention or session SQL could not fail the build. Here, a test declares its
//! dependency with [`TestDb::require_behavioral`], which skips locally when the
//! extension is unavailable (no network in a dev sandbox) but *fails* when
//! `MALLARD_REQUIRE_BEHAVIORAL=1` is set, as CI does.

use super::QueryScope;
use duckdb::Connection;

/// An in-memory database with the schema, the `events_all` view, and — where
/// available — the `behavioral` extension loaded.
pub struct TestDb {
    pub conn: Connection,
    /// Whether the behavioral extension loaded successfully.
    pub behavioral: bool,
    /// Kept alive so the temp directory outlives the view definition.
    _dir: tempfile::TempDir,
}

impl TestDb {
    pub fn new() -> Self {
        let conn = Connection::open_in_memory().expect("open in-memory DuckDB");
        crate::storage::schema::init_schema(&conn).expect("init schema");
        let dir = tempfile::tempdir().expect("tempdir");
        crate::storage::schema::setup_query_view(&conn, dir.path()).expect("query view");
        let behavioral = crate::storage::schema::load_behavioral_extension(&conn).is_ok();
        Self {
            conn,
            behavioral,
            _dir: dir,
        }
    }

    /// Declare that this test needs the behavioral extension.
    ///
    /// Returns `true` when the test should proceed. When the extension is
    /// missing it returns `false` (test skipped) unless
    /// `MALLARD_REQUIRE_BEHAVIORAL=1`, in which case it panics so CI fails.
    pub fn require_behavioral(&self, what: &str) -> bool {
        if self.behavioral {
            return true;
        }
        assert!(
            std::env::var("MALLARD_REQUIRE_BEHAVIORAL").as_deref() != Ok("1"),
            "the behavioral extension is required for {what} but could not be loaded, \
             and MALLARD_REQUIRE_BEHAVIORAL=1 is set"
        );
        eprintln!("skipping {what}: behavioral extension unavailable");
        false
    }

    /// Insert an event with an explicit name.
    pub fn insert_event(&self, visitor: &str, timestamp: &str, name: &str, pathname: &str) {
        self.conn
            .execute(
                "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
                 VALUES ('test.com', ?, CAST(? AS TIMESTAMP), ?, ?)",
                duckdb::params![visitor, timestamp, name, pathname],
            )
            .expect("insert event");
    }

    /// Insert a revenue-bearing event.
    pub fn insert_revenue(
        &self,
        visitor: &str,
        timestamp: &str,
        name: &str,
        amount: f64,
        currency: &str,
    ) {
        self.conn
            .execute(
                "INSERT INTO events
                 (site_id, visitor_id, timestamp, event_name, pathname, revenue_amount, revenue_currency)
                 VALUES ('test.com', ?, CAST(? AS TIMESTAMP), ?, '/', ?, ?)",
                duckdb::params![visitor, timestamp, name, amount, currency],
            )
            .expect("insert revenue event");
    }

    /// Insert an event carrying custom properties.
    pub fn insert_with_props(&self, visitor: &str, timestamp: &str, name: &str, props: &str) {
        self.conn
            .execute(
                "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname, props)
                 VALUES ('test.com', ?, CAST(? AS TIMESTAMP), ?, '/', ?)",
                duckdb::params![visitor, timestamp, name, props],
            )
            .expect("insert event with props");
    }

    /// Insert an event with dimension columns populated.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_dimensional(
        &self,
        visitor: &str,
        timestamp: &str,
        pathname: &str,
        browser: Option<&str>,
        country: Option<&str>,
        utm_source: Option<&str>,
        city: Option<&str>,
    ) {
        self.conn
            .execute(
                "INSERT INTO events
                 (site_id, visitor_id, timestamp, event_name, pathname, browser, country_code, utm_source, city)
                 VALUES ('test.com', ?, CAST(? AS TIMESTAMP), 'pageview', ?, ?, ?, ?, ?)",
                duckdb::params![visitor, timestamp, pathname, browser, country, utm_source, city],
            )
            .expect("insert dimensional event");
    }
}

impl Default for TestDb {
    fn default() -> Self {
        Self::new()
    }
}

/// Insert a pageview for `test.com`.
pub fn insert_pageview(conn: &Connection, visitor: &str, timestamp: &str, pathname: &str) {
    conn.execute(
        "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
         VALUES ('test.com', ?, CAST(? AS TIMESTAMP), 'pageview', ?)",
        duckdb::params![visitor, timestamp, pathname],
    )
    .expect("insert pageview");
}

/// A scope over `test.com` with the default 30-minute session window.
pub fn scope(start: &str, end: &str) -> QueryScope {
    QueryScope::new("test.com", start, end, "30 minutes")
}
