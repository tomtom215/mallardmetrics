//! One site and date range as a single Parquet file, for a data warehouse.
//!
//! The file is written by DuckDB's `COPY ... TO` to a path the caller chose
//! (always under `<data_dir>/exports`), because DuckDB cannot write Parquet to a
//! socket and Parquet's footer comes last, so a streaming writer would need the
//! whole file in memory anyway. The handler then streams the finished file and
//! deletes it. See `docs/src/exporting-parquet.md` for the measurements behind
//! that choice.
//!
//! # Consistency
//!
//! The statement runs inside the caller's `run_scoped` closure, so it holds the
//! shared tier lock for exactly as long as `COPY` runs: no flush, compaction or
//! erasure can change the file list under it, and the hot table is read in the
//! statement's own snapshot. The lock is *not* held while the client downloads,
//! which is shorter than the raw export holds it.
//!
//! # Row order
//!
//! Unspecified. The files are read oldest day first, so the output is in
//! practice roughly chronological, but it is not sorted: `ORDER BY timestamp`
//! made DuckDB sort the whole result, which raised the peak memory of a
//! 976,696-row export from 178 MB to 800 MB (process high-water mark, debug
//! build, 3 runs each) for the same time. A warehouse sorts on load anyway.
//!
//! # `visitor_id`
//!
//! Excluded, as in the raw export ([`super::export::RAW_COLUMNS`]): it is a
//! pseudonymous per-visitor identifier, and a file of per-event pseudonyms in a
//! warehouse is the artefact this project exists not to produce. The one thing
//! derived from it is `is_imported`, which says whether a row came from an
//! importer and identifies nobody.

use super::QueryScope;
use super::export::MAX_RAW_ROWS;
use duckdb::Connection;
use std::path::Path;

/// Most rows one export file holds. A range with more is refused, not cut: a
/// warehouse load that is silently missing its tail is worse than an error.
pub const MAX_ROWS: usize = MAX_RAW_ROWS;

/// MIME type registered for Parquet files.
pub const CONTENT_TYPE: &str = "application/vnd.apache.parquet";

/// One column of the exported file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    pub name: &'static str,
    /// The DuckDB / Parquet logical type.
    pub sql_type: &'static str,
    pub nullable: bool,
}

const fn col(name: &'static str, sql_type: &'static str, nullable: bool) -> Column {
    Column {
        name,
        sql_type,
        nullable,
    }
}

/// The file's schema, in column order. `docs/src/exporting-parquet.md` repeats
/// it; a test keeps the two, and the file DuckDB actually writes, in step.
pub const SCHEMA: &[Column] = &[
    col("site_id", "VARCHAR", false),
    col("timestamp", "TIMESTAMP", false),
    col("event_name", "VARCHAR", false),
    col("pathname", "VARCHAR", false),
    col("hostname", "VARCHAR", true),
    col("referrer", "VARCHAR", true),
    col("referrer_source", "VARCHAR", true),
    col("utm_source", "VARCHAR", true),
    col("utm_medium", "VARCHAR", true),
    col("utm_campaign", "VARCHAR", true),
    col("utm_content", "VARCHAR", true),
    col("utm_term", "VARCHAR", true),
    col("browser", "VARCHAR", true),
    col("browser_version", "VARCHAR", true),
    col("os", "VARCHAR", true),
    col("os_version", "VARCHAR", true),
    col("device_type", "VARCHAR", true),
    col("screen_size", "VARCHAR", true),
    col("country_code", "VARCHAR", true),
    col("region", "VARCHAR", true),
    col("city", "VARCHAR", true),
    col("props", "VARCHAR", true),
    col("revenue_amount", "DECIMAL(12,2)", true),
    col("revenue_currency", "VARCHAR", true),
    col("is_imported", "BOOLEAN", false),
];

/// What a write produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Written {
        rows: usize,
    },
    /// More than the cap matched; nothing is left on disk.
    TooManyRows,
}

/// True for a string that can sit inside a SQL string literal unescaped.
fn is_plain(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | ' '))
}

fn quote(text: &str) -> String {
    text.replace('\'', "''")
}

/// The `COPY` statement. `COPY` cannot take bound parameters (LESSONS L1), so
/// the three scope values are interpolated; they are refused unless they are
/// plain (the site id is validated at the API boundary and the bounds are
/// formatted dates), and quoted regardless.
fn copy_sql(scope: &QueryScope, dest: &Path, max_rows: usize) -> Result<String, duckdb::Error> {
    for value in [&scope.site_id, &scope.start, &scope.end] {
        if !is_plain(value) {
            return Err(duckdb::Error::InvalidParameterName(format!(
                "refusing to export with an unexpected scope value {value:?}"
            )));
        }
    }
    let projection = SCHEMA
        .iter()
        .map(|c| match c.name {
            "is_imported" => "COALESCE(visitor_id LIKE 'imp:%', false) AS is_imported".to_string(),
            name => format!("CAST({name} AS {}) AS {name}", c.sql_type),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let limit = max_rows.min(MAX_ROWS) + 1;
    Ok(format!(
        "COPY (SELECT {projection} FROM {events} \
         WHERE site_id = '{site}' \
           AND timestamp >= CAST('{start}' AS TIMESTAMP) \
           AND timestamp < CAST('{end}' AS TIMESTAMP) \
         LIMIT {limit}) \
         TO '{dest}' (FORMAT PARQUET, COMPRESSION ZSTD)",
        events = scope.events(),
        site = quote(&scope.site_id),
        start = quote(&scope.start),
        end = quote(&scope.end),
        dest = quote(&dest.to_string_lossy()),
    ))
}

/// Write the scope's events to `dest`.
///
/// The caller must hold the shared tier lock for the call (it does, inside
/// `run_scoped`). On [`Outcome::TooManyRows`] or an error, `dest` is removed.
///
/// # Errors
///
/// Returns the database error, including an interrupt when the query deadline
/// passes.
pub fn write_parquet(
    conn: &Connection,
    scope: &QueryScope,
    dest: &Path,
    max_rows: usize,
) -> Result<Outcome, duckdb::Error> {
    let sql = copy_sql(scope, dest, max_rows)?;
    let _ = std::fs::remove_file(dest);
    match conn.execute(&sql, []) {
        Ok(rows) if rows > max_rows.min(MAX_ROWS) => {
            let _ = std::fs::remove_file(dest);
            Ok(Outcome::TooManyRows)
        }
        Ok(rows) => Ok(Outcome::Written { rows }),
        Err(e) => {
            let _ = std::fs::remove_file(dest);
            Err(e)
        }
    }
}

/// Name of the directory under the data directory that holds export files.
pub const EXPORTS_DIR: &str = "exports";

/// Longest an abandoned export file may stay before the next export sweeps it.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// Create `<data_dir>/exports` (owner-only), remove files a crashed export left
/// behind, and return a fresh file name inside it.
///
/// The name is a random UUID, never request input, so the path cannot leave
/// the directory. Only files this module names (`export-*.parquet.tmp`) are
/// ever swept.
///
/// # Errors
///
/// Returns the I/O error if the directory cannot be created.
pub fn new_export_path(data_dir: &Path) -> std::io::Result<std::path::PathBuf> {
    let dir = data_dir.join(EXPORTS_DIR);
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_AFTER);
        if name.starts_with("export-") && name.ends_with(".parquet.tmp") && stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(dir.join(format!(
        "export-{}.parquet.tmp",
        uuid::Uuid::new_v4().simple()
    )))
}

/// Data directories with an export in flight. One export per directory at a
/// time bounds the disk an export can use to one file.
static IN_FLIGHT: parking_lot::Mutex<Vec<std::path::PathBuf>> = parking_lot::Mutex::new(Vec::new());

/// The right to run the one export a data directory allows. Released on drop.
#[derive(Debug)]
pub struct ExportSlot(std::path::PathBuf);

impl ExportSlot {
    /// `None` when an export of `data_dir` is already running.
    pub fn try_claim(data_dir: &Path) -> Option<Self> {
        let mut active = IN_FLIGHT.lock();
        if active.iter().any(|d| d == data_dir) {
            return None;
        }
        active.push(data_dir.to_path_buf());
        Some(Self(data_dir.to_path_buf()))
    }
}

impl Drop for ExportSlot {
    fn drop(&mut self) {
        IN_FLIGHT.lock().retain(|d| d != &self.0);
    }
}

/// A file that is deleted when this is dropped, whichever way the request ends.
#[derive(Debug)]
pub struct TempFile(pub std::path::PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn_with_events() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        for (i, (site, ts, vid)) in [
            ("a.com", "2026-03-14 10:00:00.123456", "abc"),
            ("a.com", "2026-03-14 11:00:00", "imp:ga4:s:1"),
            ("a.com", "2026-03-15 00:00:00", "abc"),
            ("b.com", "2026-03-14 10:00:00", "abc"),
        ]
        .into_iter()
        .enumerate()
        {
            conn.execute(
                "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname, \
                 revenue_amount, revenue_currency, country_code) \
                 VALUES (?, ?, CAST(? AS TIMESTAMP), 'pageview', ?, ?, ?, 'DE')",
                duckdb::params![
                    site,
                    vid,
                    ts,
                    format!("/p{i}"),
                    if i == 0 { Some(19.99) } else { None },
                    if i == 0 { Some("EUR") } else { None },
                ],
            )
            .unwrap();
        }
        conn
    }

    fn scope() -> QueryScope {
        QueryScope::new("a.com", "2026-03-14", "2026-03-15", "30 minutes")
            .with_source(crate::storage::schema::scoped_events_sql(&[]))
    }

    #[test]
    fn writes_the_documented_schema_and_only_the_requested_rows() {
        let conn = conn_with_events();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.parquet");
        let outcome = write_parquet(&conn, &scope(), &dest, 100).unwrap();
        assert_eq!(outcome, Outcome::Written { rows: 2 });

        // The schema DuckDB wrote is the schema SCHEMA documents.
        let described: Vec<(String, String, String)> = conn
            .prepare(&format!(
                "SELECT column_name, column_type, \"null\" FROM (DESCRIBE SELECT * FROM read_parquet('{}'))",
                dest.display()
            ))
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let documented: Vec<&str> = SCHEMA.iter().map(|c| c.name).collect();
        let actual: Vec<&str> = described.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(actual, documented);
        for (column, (_, ty, _)) in SCHEMA.iter().zip(&described) {
            assert_eq!(
                ty.replace(' ', ""),
                column.sql_type.replace(' ', ""),
                "{}",
                column.name
            );
        }
        assert!(!actual.contains(&"visitor_id"));

        let imported: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM read_parquet('{}') WHERE is_imported",
                    dest.display()
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(imported, 1);
        let first: String = conn
            .query_row(
                &format!("SELECT CAST(timestamp AS VARCHAR) FROM read_parquet('{}') ORDER BY timestamp LIMIT 1", dest.display()),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(first, "2026-03-14 10:00:00.123456", "microseconds survive");
    }

    #[test]
    fn nullability_matches_the_table() {
        let conn = conn_with_events();
        let required: Vec<&str> = SCHEMA
            .iter()
            .filter(|c| !c.nullable)
            .map(|c| c.name)
            .collect();
        assert_eq!(
            required,
            [
                "site_id",
                "timestamp",
                "event_name",
                "pathname",
                "is_imported"
            ]
        );
        // The events table declares exactly those NOT NULL (visitor_id aside).
        let not_null: Vec<String> = conn
            .prepare(
                "SELECT column_name FROM duckdb_columns() WHERE table_name = 'events' \
                 AND NOT is_nullable AND column_name <> 'visitor_id' ORDER BY column_index",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(not_null, ["site_id", "timestamp", "event_name", "pathname"]);
    }

    #[test]
    fn export_paths_stay_inside_the_exports_directory_and_stale_files_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join(EXPORTS_DIR).join("export-old.parquet.tmp");
        let other = dir.path().join(EXPORTS_DIR).join("keep-me.txt");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"x").unwrap();
        std::fs::write(&other, b"x").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let path = new_export_path(dir.path()).unwrap();
        assert_eq!(path.parent().unwrap(), dir.path().join(EXPORTS_DIR));
        assert!(path.to_string_lossy().ends_with(".parquet.tmp"));
        assert_ne!(path, new_export_path(dir.path()).unwrap());
        assert!(!stale.exists(), "a stale export file is swept");
        assert!(other.exists(), "files it did not name are left alone");

        std::fs::write(&path, b"x").unwrap();
        drop(TempFile(path.clone()));
        assert!(!path.exists());
    }

    #[test]
    fn one_export_per_data_directory() {
        let a = std::path::Path::new("/data/a");
        let first = ExportSlot::try_claim(a).unwrap();
        assert!(ExportSlot::try_claim(a).is_none());
        assert!(ExportSlot::try_claim(std::path::Path::new("/data/b")).is_some());
        drop(first);
        assert!(ExportSlot::try_claim(a).is_some());
    }

    #[test]
    fn refuses_more_rows_than_the_cap_and_leaves_no_file() {
        let conn = conn_with_events();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.parquet");
        assert_eq!(
            write_parquet(&conn, &scope(), &dest, 1).unwrap(),
            Outcome::TooManyRows
        );
        assert!(!dest.exists());
    }

    #[test]
    fn an_empty_range_is_a_valid_file_with_the_schema() {
        let conn = conn_with_events();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.parquet");
        let empty = QueryScope::new("nobody.com", "2026-03-14", "2026-03-15", "30 minutes")
            .with_source(crate::storage::schema::scoped_events_sql(&[]));
        assert_eq!(
            write_parquet(&conn, &empty, &dest, 10).unwrap(),
            Outcome::Written { rows: 0 }
        );
        let n: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM read_parquet('{}')", dest.display()),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn refuses_a_scope_value_that_could_break_out_of_the_literal() {
        let conn = conn_with_events();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.parquet");
        let hostile = QueryScope::new("a.com' OR '1'='1", "2026-03-14", "2026-03-15", "30 minutes");
        assert!(write_parquet(&conn, &hostile, &dest, 10).is_err());
        assert!(!dest.exists());
    }
}
