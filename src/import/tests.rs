use super::*;
use std::io::Cursor;
use std::path::Path;

const NOW: i64 = 1_800_000_000;

/// 2026-03-14 12:00:00 UTC in microseconds.
const T0: i64 = 1_773_489_600_000_000;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn events_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("events")
    }

    /// Open the database as a fresh process would, and prepare it.
    fn open(&self) -> (Connection, ParquetStorage) {
        std::fs::create_dir_all(self.events_dir()).unwrap();
        let conn = Connection::open(self.dir.path().join("mallard.duckdb")).unwrap();
        let storage = ParquetStorage::new(&self.events_dir(), 0);
        prepare(&conn, &storage).unwrap();
        (conn, storage)
    }
}

fn options() -> Options {
    Options {
        format: Format::Ga4BigQuery,
        site_id: "example.com".to_string(),
        key: b"test-key-0123456789".to_vec(),
        policy: Policy::default(),
        include_all_events: false,
        skip_invalid: false,
        now_secs: NOW,
    }
}

/// One GA4 export row: a page view of `path` by `user`'s session `session`, at
/// `T0 + offset_secs`.
fn page_view(user: &str, session: i64, offset_secs: i64, path: &str) -> String {
    format!(
        r#"{{"event_date":"20260314","event_timestamp":"{ts}","event_name":"page_view","event_params":[{{"key":"page_location","value":{{"string_value":"https://example.com{path}?utm_source=news"}}}},{{"key":"ga_session_id","value":{{"int_value":"{session}"}}}}],"user_pseudo_id":"{user}","platform":"WEB","device":{{"category":"desktop","operating_system":"Windows","web_info":{{"browser":"Firefox","browser_version":"123"}}}},"geo":{{"region":"Bavaria","city":"Munich"}}}}"#,
        ts = T0 + offset_secs * 1_000_000
    )
}

fn file(lines: &[String]) -> Cursor<Vec<u8>> {
    Cursor::new(lines.join("\n").into_bytes())
}

/// Two GA4 sessions (alice: 3 pages, bob: 1 page).
fn sample() -> Vec<String> {
    vec![
        page_view("alice.1", 100, 0, "/"),
        page_view("alice.1", 100, 60, "/pricing"),
        page_view("alice.1", 100, 120, "/docs"),
        page_view("bob.2", 200, 30, "/"),
    ]
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn parquet_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "parquet") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn leftovers(dir: &Path, suffix: &str) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.to_string_lossy().ends_with(suffix) {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn an_import_lands_in_partitioned_parquet_and_is_readable() {
    let env = Env::new();
    let (conn, storage) = env.open();
    let report = import(&conn, &storage, file(&sample()), &options()).unwrap();

    assert_eq!(
        (report.valid, report.inserted, report.already_present),
        (4, 4, 0)
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM events"),
        0,
        "hot table drained"
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM events_all"), 4);
    let files = parquet_files(&env.events_dir());
    assert_eq!(files.len(), 1);
    assert!(
        files[0]
            .to_string_lossy()
            .contains("site_id=example.com/date=2026-03-14/"),
        "{files:?}"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(DISTINCT visitor_id) FROM events_all"),
        2,
        "one pseudonymous visitor per GA4 session"
    );
}

#[test]
fn running_the_same_file_again_adds_nothing() {
    let env = Env::new();
    let (conn, storage) = env.open();
    import(&conn, &storage, file(&sample()), &options()).unwrap();
    let files_before = parquet_files(&env.events_dir());

    let again = import(&conn, &storage, file(&sample()), &options()).unwrap();
    assert_eq!(
        (again.valid, again.inserted, again.already_present),
        (4, 0, 4)
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM events_all"), 4);
    assert_eq!(
        parquet_files(&env.events_dir()),
        files_before,
        "no new file"
    );
}

#[test]
fn an_overlapping_export_adds_only_its_new_rows() {
    let env = Env::new();
    let (conn, storage) = env.open();
    let first = sample()[..3].to_vec();
    import(&conn, &storage, file(&first), &options()).unwrap();

    let report = import(&conn, &storage, file(&sample()), &options()).unwrap();
    assert_eq!((report.inserted, report.already_present), (1, 3));
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM events_all"), 4);
}

#[test]
fn identical_events_are_kept_once_per_occurrence_and_stay_idempotent() {
    // Under round_timestamps two reloads in one hour have the same key. The
    // multiset rule keeps both the first time and neither the second.
    let env = Env::new();
    let (conn, storage) = env.open();
    let mut opts = options();
    opts.policy.round_timestamps = true;
    let lines = vec![
        page_view("alice.1", 100, 0, "/"),
        page_view("alice.1", 100, 90, "/"),
        page_view("alice.1", 100, 400, "/"),
    ];
    let first = import(&conn, &storage, file(&lines), &opts).unwrap();
    assert_eq!(first.inserted, 3);
    let second = import(&conn, &storage, file(&lines), &opts).unwrap();
    assert_eq!(second.inserted, 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM events_all"), 3);
}

#[test]
fn the_same_events_for_another_site_are_not_duplicates() {
    let env = Env::new();
    let (conn, storage) = env.open();
    import(&conn, &storage, file(&sample()), &options()).unwrap();
    let mut other = options();
    other.site_id = "other.org".to_string();
    let report = import(&conn, &storage, file(&sample()), &other).unwrap();
    assert_eq!(report.inserted, 4);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM events_all"), 8);
}

#[test]
fn invalid_lines_abort_with_line_numbers_and_write_nothing() {
    let env = Env::new();
    let (conn, storage) = env.open();
    let mut lines = sample();
    lines.insert(1, "this is not json".to_string());
    lines.insert(
        3,
        r#"{"event_timestamp":"12","event_name":"page_view"}"#.to_string(),
    );

    let Err(ImportError::Invalid(report)) = import(&conn, &storage, file(&lines), &options())
    else {
        panic!("expected the import to be refused")
    };
    let listed: Vec<(u64, &str)> = report
        .errors
        .iter()
        .map(|e| (e.line, e.message.as_str()))
        .collect();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].0, 2);
    assert!(listed[0].1.contains("not valid JSON"));
    assert_eq!(listed[1].0, 4);
    assert!(listed[1].1.contains("before 2010"));
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM events_all"), 0);
    assert!(parquet_files(&env.events_dir()).is_empty());
    // The staging table is gone too.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM duckdb_tables() WHERE table_name = 'import_stage'"
        ),
        0
    );
}

#[test]
fn skip_invalid_imports_the_rest() {
    let env = Env::new();
    let (conn, storage) = env.open();
    let mut lines = sample();
    lines.push("{broken".to_string());
    lines.insert(2, String::new());
    let mut opts = options();
    opts.skip_invalid = true;
    let report = import(&conn, &storage, file(&lines), &opts).unwrap();
    assert_eq!(
        (
            report.valid,
            report.invalid,
            report.blank_lines,
            report.inserted
        ),
        (4, 1, 1, 4)
    );
    assert_eq!(report.errors[0].line, 6);
}

#[test]
fn an_oversized_line_is_one_error_and_does_not_derail_the_next() {
    let big = format!(r#"{{"x":"{}"}}"#, "a".repeat(MAX_LINE_BYTES + 10));
    let input = format!("{big}\n{}\n", sample()[0]);
    let report = dry_run(Cursor::new(input.into_bytes()), &options()).unwrap();
    assert_eq!(report.invalid, 1);
    assert_eq!(report.errors[0].line, 1);
    assert!(report.errors[0].message.contains("longer than"));
    assert_eq!(report.valid, 1);
}

#[test]
fn invalid_utf8_and_crlf_and_bom_are_handled() {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(sample()[0].as_bytes());
    bytes.extend_from_slice(b"\r\n");
    bytes.extend_from_slice(&[0xFF, 0xFE, b'\n']);
    bytes.extend_from_slice(sample()[1].as_bytes());
    let report = dry_run(Cursor::new(bytes), &options()).unwrap();
    assert_eq!(report.valid, 2);
    assert_eq!(report.invalid, 1);
    assert_eq!(report.errors[0].line, 2);
    assert!(report.errors[0].message.contains("UTF-8"));
}

#[test]
fn a_dry_run_counts_and_touches_nothing() {
    let report = dry_run(file(&sample()), &options()).unwrap();
    assert!(report.dry_run);
    assert_eq!((report.valid, report.invalid, report.inserted), (4, 0, 0));
    assert_eq!(report.days.len(), 1);
    assert_eq!(
        report.first_timestamp.unwrap().to_string(),
        "2026-03-14 12:00:00"
    );
    assert_eq!(
        report.last_timestamp.unwrap().to_string(),
        "2026-03-14 12:02:00"
    );
    let text = report.render();
    assert!(
        text.contains("mode: dry-run") && text.contains("valid_events: 4"),
        "{text}"
    );
}

#[test]
fn skipped_rows_are_counted_by_reason() {
    let mut lines = sample();
    lines.push(sample()[0].replace("page_view", "session_start"));
    lines.push(sample()[0].replace("\"WEB\"", "\"ANDROID\""));
    let report = dry_run(file(&lines), &options()).unwrap();
    assert_eq!(report.valid, 4);
    assert_eq!(report.skipped.values().sum::<u64>(), 2);
}

#[test]
fn midnight_utc_boundaries_use_utc_not_the_property_time_zone() {
    // 2026-03-14 23:59:59.999999 UTC is event_date 20260315 in UTC+2; the
    // partition follows the UTC timestamp, which is what every query uses.
    let env = Env::new();
    let (conn, storage) = env.open();
    let late = page_view("a", 1, 12 * 3600 - 1, "/").replace("20260314", "20260315");
    let early = page_view("b", 2, 12 * 3600, "/");
    import(&conn, &storage, file(&[late, early]), &options()).unwrap();
    let days: Vec<String> = parquet_files(&env.events_dir())
        .iter()
        .map(|p| {
            p.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(days, ["date=2026-03-14", "date=2026-03-15"]);
}

// ── Crash points ─────────────────────────────────────────────────────────

/// Reopen as a new process would (the old connection is dropped first, which
/// releases DuckDB's file lock).
fn restart(env: &Env, conn: Connection) -> (Connection, ParquetStorage) {
    drop(conn);
    env.open()
}

fn assert_exactly_the_sample(conn: &Connection, env: &Env) {
    assert_eq!(count(conn, "SELECT COUNT(*) FROM events_all"), 4);
    assert_eq!(count(conn, "SELECT COUNT(*) FROM events"), 0);
    assert_eq!(leftovers(&env.events_dir(), ".pending"), 0);
    assert_eq!(parquet_files(&env.events_dir()).len(), 1);
}

#[test]
fn crash_after_the_insert_before_any_flush() {
    let env = Env::new();
    let (conn, _storage) = env.open();
    let report = stage_and_merge(&conn, file(&sample()), &options()).unwrap();
    assert_eq!(report.inserted, 4);
    // The process dies here: rows are committed in the hot table only. They
    // are durable and already visible to reports.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM events_all"), 4);

    let (conn, storage) = restart(&env, conn);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM events_all"),
        4,
        "survived the restart"
    );
    let again = import(&conn, &storage, file(&sample()), &options()).unwrap();
    assert_eq!((again.inserted, again.already_present), (0, 4));
    assert_exactly_the_sample(&conn, &env);
}

#[test]
fn crash_after_the_staged_file_is_written() {
    let env = Env::new();
    let (conn, storage) = env.open();
    storage.inject_flush_crash(Some(false));
    let Err(ImportError::Flush(_)) = import(&conn, &storage, file(&sample()), &options()) else {
        panic!("the injected crash should surface as a flush error")
    };
    assert_eq!(leftovers(&env.events_dir(), ".pending"), 1);

    // Startup recovery sees the staged rows are all still hot: a duplicate.
    let (conn, storage) = restart(&env, conn);
    assert_eq!(leftovers(&env.events_dir(), ".pending"), 0);
    let again = import(&conn, &storage, file(&sample()), &options()).unwrap();
    assert_eq!(again.inserted, 0);
    assert_exactly_the_sample(&conn, &env);
}

#[test]
fn crash_after_the_hot_rows_are_deleted() {
    let env = Env::new();
    let (conn, storage) = env.open();
    storage.inject_flush_crash(Some(true));
    let Err(ImportError::Flush(_)) = import(&conn, &storage, file(&sample()), &options()) else {
        panic!("the injected crash should surface as a flush error")
    };
    // The only copy of the rows is the staged file.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM events"), 0);
    assert_eq!(leftovers(&env.events_dir(), ".pending"), 1);

    let (conn, storage) = restart(&env, conn);
    // Recovery published it, so the rows are back without re-running anything.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM events_all"), 4);
    let again = import(&conn, &storage, file(&sample()), &options()).unwrap();
    assert_eq!((again.inserted, again.already_present), (0, 4));
    assert_exactly_the_sample(&conn, &env);
}

#[test]
fn a_failed_scan_leaves_the_events_table_untouched() {
    struct Failing(u8);
    impl std::io::Read for Failing {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                return Err(std::io::Error::other("disk went away"));
            }
            self.0 -= 1;
            let line = format!("{}\n", sample()[0]);
            buf[..line.len()].copy_from_slice(line.as_bytes());
            Ok(line.len())
        }
    }
    let env = Env::new();
    let (conn, storage) = env.open();
    let reader = std::io::BufReader::with_capacity(1 << 16, Failing(1));
    let Err(ImportError::Io(_)) = import(&conn, &storage, reader, &options()) else {
        panic!("expected an I/O error")
    };
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM events_all"), 0);
}

// ── What imported rows are ───────────────────────────────────────────────

#[test]
fn imported_rows_are_namespaced_and_carry_no_identifiers() {
    let env = Env::new();
    let (conn, storage) = env.open();
    import(&conn, &storage, file(&sample()), &options()).unwrap();
    let ids: Vec<String> = conn
        .prepare("SELECT DISTINCT visitor_id FROM events_all")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ids.len(), 2);
    for id in &ids {
        assert!(id.starts_with(VISITOR_NAMESPACE), "{id}");
        // A live id is 64 hex characters, so the two spaces cannot meet.
        assert_ne!(id.len(), 64);
        assert!(!id.contains("alice") && !id.contains("bob"));
    }
    // Nothing in the stored rows mentions the source's identifiers or queries.
    let dump: String = conn
        .query_row(
            "SELECT string_agg(CAST(t AS VARCHAR), ' ') FROM events_all t",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for needle in ["alice.1", "bob.2", "utm_source=news&", "?"] {
        assert!(!dump.contains(needle), "{needle} in {dump}");
    }
}

#[test]
fn the_same_user_in_two_sessions_is_two_unlinked_visitors() {
    let env = Env::new();
    let (conn, storage) = env.open();
    let lines = vec![
        page_view("alice.1", 100, 0, "/"),
        page_view("alice.1", 101, 86_400, "/"),
    ];
    import(&conn, &storage, file(&lines), &options()).unwrap();
    assert_eq!(
        count(&conn, "SELECT COUNT(DISTINCT visitor_id) FROM events_all"),
        2,
        "no cross-session linkage, hence no retention cohorts from imported rows"
    );
}

// ── The documented-schema fixture ────────────────────────────────────────

const FIXTURE: &str = include_str!("../../tests/fixtures/ga4-bigquery-sample.ndjson");

#[test]
fn the_ga4_fixture_imports_as_documented() {
    let env = Env::new();
    let (conn, storage) = env.open();
    let report = import(&conn, &storage, Cursor::new(FIXTURE.as_bytes()), &options()).unwrap();

    assert_eq!(report.lines_read, 9);
    assert_eq!((report.valid, report.invalid, report.inserted), (6, 0, 6));
    let skipped: Vec<&str> = report.skipped.keys().map(String::as_str).collect();
    assert_eq!(skipped.len(), 3, "{skipped:?}");
    assert!(skipped.iter().any(|s| s.contains("session_start")));
    assert!(skipped.iter().any(|s| s.contains("user_engagement")));
    assert!(skipped.iter().any(|s| s.contains("ANDROID")));

    assert_eq!(
        count(&conn, "SELECT COUNT(DISTINCT visitor_id) FROM events_all"),
        3
    );
    let pageviews = count(
        &conn,
        "SELECT COUNT(*) FROM events_all WHERE event_name = 'pageview'",
    );
    assert_eq!(pageviews, 4);
    let (revenue, currency): (f64, String) = conn
        .query_row(
            "SELECT CAST(revenue_amount AS DOUBLE), revenue_currency FROM events_all WHERE event_name = 'purchase'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((revenue, currency.as_str()), (59.9, "EUR"));
    let (path, campaign, source): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT pathname, utm_campaign, referrer_source FROM events_all \
             WHERE event_name = 'pageview' AND utm_source = 'newsletter'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((path.as_str(), campaign.as_deref()), ("/", Some("spring")));
    assert_eq!(source.as_deref(), Some("mail.example.org"));
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM events_all WHERE pathname = '/blog/duck-care'"
        ),
        1,
        "trailing slash collapsed as at ingest"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM events_all WHERE os = 'macOS'"),
        1
    );

    // Identifiers and query strings from the source are nowhere in the store.
    let dump: String = conn
        .query_row(
            "SELECT string_agg(CAST(t AS VARCHAR), ' ') FROM events_all t",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for needle in [
        "crm-10442",
        "1840022001",
        "977310021",
        "T-90001",
        "color=yellow",
    ] {
        assert!(!dump.contains(needle), "{needle} was stored");
    }
}
