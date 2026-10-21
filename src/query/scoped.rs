//! Reading only the cold-tier files a request can touch (F-06).
//!
//! `events_all` re-expands a glob over every site and every day on each query,
//! and `union_by_name` opens every footer to build its schema. At 100 sites x 90
//! days that is 9,000 files per request however narrow the filter; a
//! site-scoped file list costs about 3 ms instead of about 0.6 s (see the
//! `query_scale` benchmark). [`ColdReads`] builds that list.
//!
//! # Why the list must be made under the tier lock
//!
//! A compaction moves its source files to `NNNN.parquet.del` and renames the
//! merged file in, all under the exclusive [`TierLock`](crate::storage::TierLock).
//! `events_all` is immune to that by construction: it re-expands the glob at
//! bind time, inside the query, which holds the shared side. A file list made
//! *before* the shared lock is taken is not: a compaction can run between the
//! listing and the open, and the query then fails with "No such file" for a
//! source that was merged a moment ago, or, if the merged file is the one
//! missed, silently omits a day's rows.
//!
//! So a [`ColdReads`] only exists inside the closure `run_query_with` calls
//! while it holds the shared lock: it borrows the storage for that closure's
//! lifetime and has no constructor a handler could use earlier. Holding the
//! lock for the whole closure then keeps the files it listed in place until the
//! statement finishes. `tests::a_list_made_before_the_lock_breaks_under_compaction`
//! pins why that ordering matters, and
//! `tests::compaction_cannot_swap_files_out_from_under_a_scoped_query` that it
//! holds.

use super::QueryScope;
use crate::storage::parquet::ParquetStorage;
use crate::storage::schema::scoped_events_sql;
use chrono::NaiveDate;

/// Builds site-and-date-scoped event sources. See the module documentation for
/// when one may exist.
#[derive(Clone, Copy)]
pub struct ColdReads<'a> {
    storage: &'a ParquetStorage,
}

impl<'a> ColdReads<'a> {
    /// Wrap `storage`. Callers outside this crate's query runner and tests
    /// should not need this: the runner hands one to the closure it runs under
    /// the tier lock.
    pub const fn new(storage: &'a ParquetStorage) -> Self {
        Self { storage }
    }

    /// A relation holding the whole hot table plus the site's Parquet files for
    /// the inclusive day range, ready to put after `FROM`.
    pub fn source(&self, site_id: &str, from: NaiveDate, to: NaiveDate) -> String {
        scoped_events_sql(&self.storage.files_for_range(site_id, from, to))
    }

    /// Like [`ColdReads::source`], over every site that has a partition on disk.
    ///
    /// For the site listing, which counts recent events per site. Costs one
    /// directory listing per site and day.
    pub fn all_sites_source(&self, from: NaiveDate, to: NaiveDate) -> String {
        let files: Vec<_> = self
            .storage
            .known_site_ids()
            .iter()
            .flat_map(|site| self.storage.files_for_range(site, from, to))
            .collect();
        scoped_events_sql(&files)
    }

    /// `scope` narrowed to the files its own time range can touch, widened by
    /// `lookback_days` for a query that deliberately reads before its start.
    ///
    /// A scope whose bounds do not parse is returned unchanged and keeps
    /// reading `events_all`: slower, never wrong.
    pub fn scope(&self, scope: QueryScope, lookback_days: u32) -> QueryScope {
        match scope.scan_days(lookback_days) {
            Some((from, to)) => {
                let source = self.source(&scope.site_id, from, to);
                scope.with_source(source)
            }
            None => scope,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{Filter, breakdowns, events, flow, funnel, metrics, retention};
    use crate::query::{revenue, sequences, timeseries};
    use crate::storage::TierLock;
    use duckdb::Connection;
    use parking_lot::Mutex;
    use serde::Serialize;
    use std::path::Path;
    use std::sync::Arc;

    /// A file-backed database with a real reader pool, a real Parquet tree and
    /// the `events_all` view, like production (L28: the in-memory fixtures
    /// cannot see tier problems).
    struct Fixture {
        _dir: tempfile::TempDir,
        events_dir: std::path::PathBuf,
        writer: Arc<Mutex<Connection>>,
        storage: ParquetStorage,
        tier: TierLock,
        behavioral: bool,
    }

    impl Fixture {
        fn new(compact_after: usize) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let conn = Connection::open(dir.path().join("t.duckdb")).expect("open");
            crate::storage::schema::init_schema(&conn).expect("schema");
            let events_dir = dir.path().join("events");
            std::fs::create_dir_all(&events_dir).expect("events dir");
            crate::storage::schema::setup_query_view(&conn, &events_dir).expect("view");
            let behavioral = crate::storage::schema::load_behavioral_extension(&conn).is_ok();
            Self {
                storage: ParquetStorage::new(&events_dir, compact_after),
                events_dir,
                writer: Arc::new(Mutex::new(conn)),
                tier: TierLock::new(),
                behavioral,
                _dir: dir,
            }
        }

        fn insert(&self, site: &str, visitor: &str, ts: &str, name: &str, path: &str) {
            self.writer
                .lock()
                .execute(
                    "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname, \
                     browser, country_code, revenue_amount, revenue_currency, props) \
                     VALUES (?, ?, CAST(? AS TIMESTAMP), ?, ?, ?, ?, ?, ?, ?)",
                    duckdb::params![
                        site,
                        visitor,
                        ts,
                        name,
                        path,
                        if visitor.len() % 2 == 0 {
                            "Chrome"
                        } else {
                            "Firefox"
                        },
                        if visitor.len() % 3 == 0 { "US" } else { "DE" },
                        if name == "purchase" { Some(9.5_f64) } else { None },
                        if name == "purchase" { Some("USD") } else { None },
                        if name == "purchase" {
                            Some(r#"{"plan":"pro","tier":{"n":1}}"#)
                        } else {
                            None
                        },
                    ],
                )
                .expect("insert");
        }

        /// Move everything in the hot table to Parquet.
        fn flush(&self) {
            let _tier = self.tier.write();
            // Deferred: no automatic compaction, so a test decides when files merge.
            self.storage
                .flush_events_deferred(&self.writer.lock())
                .expect("flush");
        }

        fn scoped(&self, scope: QueryScope, lookback: u32) -> QueryScope {
            ColdReads::new(&self.storage).scope(scope, lookback)
        }
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("date")
    }

    /// Several days, two sites, sessions that cross midnight, a visitor whose
    /// first visit predates the report window, and a hot-tier tail.
    fn populate(fx: &Fixture) {
        for (site, offset) in [("a.com", 0), ("b.com", 1)] {
            for d in 10..=16 {
                for v in 0..6 {
                    let visitor = format!("{site}-v{v}");
                    let hour = 8 + v + offset;
                    fx.insert(
                        site,
                        &visitor,
                        &format!("2024-01-{d:02} {hour:02}:00:00"),
                        "pageview",
                        "/",
                    );
                    fx.insert(
                        site,
                        &visitor,
                        &format!("2024-01-{d:02} {hour:02}:10:00"),
                        "pageview",
                        "/pricing",
                    );
                    if v % 2 == 0 {
                        fx.insert(
                            site,
                            &visitor,
                            &format!("2024-01-{d:02} {hour:02}:20:00"),
                            "purchase",
                            "/checkout",
                        );
                    }
                }
                // A session that starts at 23:50 and carries on after midnight.
                let next = d + 1;
                let night = format!("{site}-night{d}");
                fx.insert(site, &night, &format!("2024-01-{d:02} 23:50:00"), "pageview", "/");
                fx.insert(
                    site,
                    &night,
                    &format!("2024-01-{next:02} 00:05:00"),
                    "pageview",
                    "/pricing",
                );
                // Exactly on the day boundaries.
                fx.insert(
                    site,
                    &format!("{site}-edge{d}"),
                    &format!("2024-01-{d:02} 00:00:00"),
                    "pageview",
                    "/edge",
                );
            }
        }
        // Everything so far goes to Parquet; the rows inserted after this stay
        // in the hot table, so reads span both tiers.
        fx.flush();
        for site in ["a.com", "b.com"] {
            fx.insert(site, &format!("{site}-hot"), "2024-01-17 09:00:00", "pageview", "/");
            fx.insert(site, &format!("{site}-hot"), "2024-01-17 09:05:00", "pageview", "/pricing");
            fx.insert(site, &format!("{site}-v0"), "2024-01-17 09:06:00", "purchase", "/checkout");
        }
    }

    /// Every report, serialised, so two runs compare byte for byte.
    fn all_reports(
        conn: &Connection,
        scope: &QueryScope,
        lookback: u32,
        behavioral: bool,
    ) -> Vec<(String, String)> {
        fn ser<T: Serialize>(name: &str, r: Result<T, duckdb::Error>) -> (String, String) {
            (
                name.to_string(),
                serde_json::to_string(&r.unwrap_or_else(|e| panic!("{name}: {e}"))).expect("json"),
            )
        }
        let mut out = vec![
            ser("event_metrics", metrics::query_event_metrics(conn, scope)),
            ser(
                "timeseries_day",
                timeseries::query_timeseries(conn, scope, timeseries::Granularity::Day),
            ),
            ser("goals", events::query_goals(conn, scope)),
            ser("property_keys", events::query_property_keys(conn, scope)),
            ser(
                "property_values",
                events::query_property_values(conn, scope, "plan", Some("purchase")),
            ),
            ser("revenue", revenue::query_revenue(conn, scope)),
            ser(
                "export_daily",
                crate::query::export::query_daily_export(conn, scope),
            ),
            ser(
                "export_raw",
                crate::query::export::query_raw_export(conn, scope, 1000),
            ),
        ];
        for slug in breakdowns::Dimension::SLUGS {
            let dim = breakdowns::Dimension::from_slug(slug).expect("slug");
            if dim.requires_behavioral() && !behavioral {
                continue;
            }
            out.push(ser(
                slug,
                breakdowns::query_breakdown(conn, scope, dim, 50),
            ));
        }
        if behavioral {
            let steps = ["pathname = '/'", "pathname = '/pricing'", "event_name = 'purchase'"];
            out.push(ser("core_metrics", metrics::query_core_metrics(conn, scope)));
            out.push(ser(
                "session_metrics",
                metrics::query_session_metrics(conn, scope),
            ));
            out.push(ser(
                "funnel",
                funnel::query_funnel(conn, scope, "1 day", "", &steps),
            ));
            out.push(ser(
                "sequence",
                sequences::execute_sequence_match(conn, scope, &steps),
            ));
            for dir in [flow::Direction::Forward, flow::Direction::Backward] {
                out.push(ser(
                    "flow",
                    flow::query_flow(conn, scope, "/pricing", dir, 20),
                ));
            }
            out.push(ser(
                "retention",
                retention::query_retention_at(conn, scope, 3, lookback, far_future()),
            ));
        }
        out
    }

    fn far_future() -> chrono::NaiveDateTime {
        day("2030-01-01").and_hms_opt(0, 0, 0).expect("time")
    }

    /// Run every report through `events_all` and through the scoped source and
    /// demand identical output. `lookback` is retention's.
    fn assert_same(fx: &Fixture, scope: &QueryScope, lookback: u32, label: &str) {
        let _tier = fx.tier.read();
        let guard = fx.writer.lock();
        let baseline = all_reports(&guard, scope, lookback, fx.behavioral);
        let narrowed = fx.scoped(scope.clone(), retention::scan_lookback_days(lookback));
        assert!(
            narrowed.events() != crate::query::EVENTS_ALL,
            "{label}: the scope was not narrowed, so this compares a path with itself"
        );
        let scoped = all_reports(&guard, &narrowed, lookback, fx.behavioral);
        assert_eq!(baseline.len(), scoped.len());
        for ((name, want), (_, got)) in baseline.iter().zip(&scoped) {
            assert_eq!(want, got, "{label}: {name} differs between events_all and the scoped read");
        }
    }

    #[test]
    fn scoped_reads_equal_events_all_for_every_report() {
        // The scoped read is a performance change and must not be an answer
        // change. Ranges are chosen to hit the edges: a whole middle span, one
        // day, a span starting and ending mid-day (sessions crossing midnight
        // at both ends), a span that reaches into the hot tier, one that is
        // entirely hot, one for a site with other sites' data beside it, and
        // an empty one.
        let fx = Fixture::new(0);
        populate(&fx);
        if !fx.behavioral {
            assert!(
                std::env::var("MALLARD_REQUIRE_BEHAVIORAL").as_deref() != Ok("1"),
                "MALLARD_REQUIRE_BEHAVIORAL=1 but the extension did not load"
            );
            eprintln!("behavioral extension unavailable: comparing the non-behavioral reports only");
        }
        let cases: &[(&str, &str, &str, &str)] = &[
            ("a.com", "2024-01-11", "2024-01-15", "middle span"),
            ("a.com", "2024-01-12", "2024-01-13", "one day"),
            ("b.com", "2024-01-12 23:55:00", "2024-01-14 00:03:00", "mid-day bounds across midnight"),
            ("a.com", "2024-01-16", "2024-01-19", "reaches the hot tier"),
            ("b.com", "2024-01-17", "2024-01-18", "hot tier only"),
            ("a.com", "2024-01-01", "2024-02-01", "wider than the data"),
            ("a.com", "2023-06-01", "2023-06-08", "empty range"),
            ("nobody.com", "2024-01-10", "2024-01-18", "unknown site"),
        ];
        for (site, start, end, label) in cases {
            for lookback in [1, 30] {
                let scope = QueryScope::new(*site, *start, *end, "30 minutes");
                assert_same(&fx, &scope, lookback, label);
            }
        }
    }

    #[test]
    fn scoped_reads_equal_events_all_under_filters_and_after_compaction() {
        let fx = Fixture::new(2);
        populate(&fx);
        // Several small files per day, then compact each, so the scoped list
        // sees merged (`.m1`) files as well as plain ones.
        for round in 0..3 {
            for d in 10..=12 {
                fx.insert(
                    "a.com",
                    &format!("extra-{round}-{d}"),
                    // Off the fixture's other timestamps: raw export orders by
                    // timestamp alone, so a tie would be ordered arbitrarily.
                    &format!("2024-01-{d:02} 12:{:02}:30", 30 + round),
                    "pageview",
                    "/extra",
                );
            }
            fx.flush();
        }
        for d in 10..=12 {
            let date = format!("2024-01-{d:02}");
            let n = fx
                .storage
                .compact_partition(&fx.writer.lock(), &fx.tier, "a.com", &date)
                .expect("compact");
            assert!(n > 0, "{date}: nothing was compacted, so the test is not mixing levels");
        }
        let filters = vec![
            Filter {
                dimension: breakdowns::Dimension::from_slug("browsers").expect("slug"),
                negated: false,
                value: "Chrome".to_string(),
            },
            Filter {
                dimension: breakdowns::Dimension::from_slug("countries").expect("slug"),
                negated: true,
                value: "US".to_string(),
            },
        ];
        let scope = QueryScope::new("a.com", "2024-01-10", "2024-01-14", "30 minutes")
            .with_filters(filters);
        assert_same(&fx, &scope, 7, "filtered after compaction");
    }

    #[test]
    fn retention_needs_the_lookback_widening() {
        // A visitor first seen before the report starts is not a new visitor,
        // and only the files before `start` can say so. A scope without the
        // widening would count them as new: this is the one place where the
        // read set is deliberately wider than the report's range.
        let fx = Fixture::new(0);
        if !fx.behavioral {
            return;
        }
        fx.insert("a.com", "old", "2024-01-08 10:00:00", "pageview", "/");
        fx.flush();
        fx.insert("a.com", "old", "2024-01-20 10:00:00", "pageview", "/");
        fx.insert("a.com", "new", "2024-01-20 11:00:00", "pageview", "/");
        fx.flush();
        let scope = QueryScope::new("a.com", "2024-01-15", "2024-01-25", "30 minutes");
        let run = |scope: &QueryScope| {
            let guard = fx.writer.lock();
            retention::query_retention_at(&guard, scope, 2, 14, far_future()).expect("retention")
        };
        let _tier = fx.tier.read();
        let want = run(&scope);
        let widened = run(&fx.scoped(scope.clone(), retention::scan_lookback_days(14)));
        let unwidened = run(&fx.scoped(scope, 0));
        assert_eq!(
            serde_json::to_string(&want).expect("json"),
            serde_json::to_string(&widened).expect("json")
        );
        assert_ne!(
            serde_json::to_string(&want).expect("json"),
            serde_json::to_string(&unwidened).expect("json"),
            "without the lookback the old visitor is miscounted as new"
        );
    }

    #[test]
    fn scan_days_follow_the_scope_and_refuse_garbage() {
        let scope = QueryScope::new("a.com", "2024-01-10", "2024-01-14", "30 minutes");
        assert_eq!(scope.scan_days(0), Some((day("2024-01-10"), day("2024-01-14"))));
        assert_eq!(scope.scan_days(30), Some((day("2023-12-11"), day("2024-01-14"))));
        let timed = QueryScope::new("a.com", "2024-01-10 23:55:00", "2024-01-11 00:03:00", "1 hour");
        assert_eq!(timed.scan_days(0), Some((day("2024-01-10"), day("2024-01-11"))));
        let bad = QueryScope::new("a.com", "yesterday", "2024-01-14", "30 minutes");
        assert_eq!(bad.scan_days(0), None);
        // A scope that cannot be narrowed keeps reading the view.
        let storage = ParquetStorage::new(Path::new("/nonexistent"), 0);
        assert_eq!(
            ColdReads::new(&storage).scope(bad, 0).events(),
            crate::query::EVENTS_ALL
        );
    }

    #[test]
    fn a_list_made_before_the_lock_breaks_under_compaction() {
        // The hazard the module documentation describes, reproduced: list the
        // files, let a compaction swap them, then open the stale list.
        let fx = Fixture::new(2);
        for i in 0..2 {
            fx.insert("a.com", &format!("v{i}"), "2024-01-10 10:00:00", "pageview", "/");
            fx.flush();
        }
        let stale = ColdReads::new(&fx.storage).source("a.com", day("2024-01-10"), day("2024-01-10"));
        let merged = fx
            .storage
            .compact_partition(&fx.writer.lock(), &fx.tier, "a.com", "2024-01-10")
            .expect("compact");
        assert_eq!(merged, 2);
        let result = fx
            .writer
            .lock()
            .query_row(&format!("SELECT COUNT(*) FROM {stale}"), [], |r| r.get::<_, i64>(0));
        assert!(
            result.is_err(),
            "a pre-lock file list should reference files the compaction moved away"
        );
        // Listed again, under the lock, it is right.
        let _tier = fx.tier.read();
        let fresh = ColdReads::new(&fx.storage).source("a.com", day("2024-01-10"), day("2024-01-10"));
        let n: i64 = fx
            .writer
            .lock()
            .query_row(&format!("SELECT COUNT(*) FROM {fresh}"), [], |r| r.get(0))
            .expect("fresh list");
        assert_eq!(n, 2);
    }

    #[test]
    fn compaction_cannot_swap_files_out_from_under_a_scoped_query() {
        // A scoped query holds the shared tier lock from listing to the end of
        // its statement, so a compaction that wants to swap waits for it and
        // the query sees one consistent file set. Hammered from two threads:
        // every count must be the full, constant total.
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        let fx = Arc::new(Fixture::new(2));
        let per_flush = 5;
        for round in 0..4 {
            for v in 0..per_flush {
                fx.insert(
                    "a.com",
                    &format!("v{round}-{v}"),
                    "2024-01-10 10:00:00",
                    "pageview",
                    "/",
                );
            }
            fx.flush();
        }
        let total = i64::from(4 * per_flush);
        let done = Arc::new(AtomicBool::new(false));
        let compactions = Arc::new(AtomicU32::new(0));

        // A separate reader connection, as production has (L28).
        let readers = crate::storage::ReaderPool::new(&fx.writer, 2);
        assert_eq!(readers.len(), 2);

        let compactor = {
            let (fx, done, compactions) = (Arc::clone(&fx), Arc::clone(&done), Arc::clone(&compactions));
            std::thread::spawn(move || {
                for round in 0..12 {
                    // New small files keep the planner finding work to merge.
                    fx.insert(
                        "a.com",
                        &format!("c{round}"),
                        "2024-01-10 11:00:00",
                        "pageview",
                        "/c",
                    );
                    fx.flush();
                    let moved = fx
                        .storage
                        .compact_partition(&fx.writer.lock(), &fx.tier, "a.com", "2024-01-10")
                        .expect("compact");
                    if moved > 0 {
                        compactions.fetch_add(1, Ordering::SeqCst);
                    }
                }
                done.store(true, Ordering::SeqCst);
            })
        };

        let mut queries = 0u32;
        while !done.load(Ordering::SeqCst) {
            let _tier = fx.tier.read();
            let source =
                ColdReads::new(&fx.storage).source("a.com", day("2024-01-10"), day("2024-01-10"));
            let conn = readers.acquire();
            let n: i64 = conn
                .lock()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {source} WHERE site_id = 'a.com'"),
                    [],
                    |r| r.get(0),
                )
                .expect("a scoped query must never lose a file to a compaction");
            // Rows only grow (the compactor adds one per round), never shrink
            // and never double.
            assert!(n >= total, "rows went missing: {n} < {total}");
            assert!(n <= total + 12, "rows were double counted: {n}");
            queries += 1;
            drop(_tier);
            std::thread::yield_now();
        }
        compactor.join().expect("compactor");
        assert!(queries > 0);
        assert!(compactions.load(Ordering::SeqCst) > 0, "no compaction ran");
        let _ = &fx.events_dir;
    }
}
