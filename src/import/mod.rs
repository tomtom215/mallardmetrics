//! Offline importers for event-level exports of other analytics products.
//!
//! `mallard-metrics import <format> --site-id <id> --file <path>` reads a file,
//! validates every line, and writes the events through the same hot-table and
//! Parquet flush path the server uses, so crash recovery is the existing
//! recovery. See `docs/src/importing-data.md` for the operator's view; this
//! comment is the design record.
//!
//! # Why a CLI and not an upload endpoint
//!
//! DuckDB holds an exclusive lock on the database file while the server runs,
//! so an importer is either a process that runs with the server **stopped**
//! (this), or a request handler inside the running server. The endpoint would
//! have to accept an unbounded upload under the 30 s request timeout and 1 MiB
//! ingest body limit, hold the writer connection and the exclusive tier lock
//! for the length of a multi-million-row flush (stalling every dashboard query
//! and every ingest), and add an admin-scope write surface that can create
//! partitions for any site. The CLI needs none of that, has no timeout, can be
//! run again after a failure, and never competes with live traffic. The price
//! is downtime for the length of the import (measured in the docs), which is
//! acceptable for a once-per-migration operation.
//!
//! # Storage model
//!
//! 1. Recovery as at server start: leftover temp files, interrupted flushes.
//! 2. Rows are parsed and appended to a DuckDB `TEMP` staging table, so the
//!    process holds one line and one append chunk; the staged rows spill to disk
//!    under DuckDB's `memory_limit`.
//! 3. One `INSERT ... SELECT` moves the staged rows that are not already stored
//!    into the hot `events` table. It is a single statement, so it either
//!    happens or it does not.
//! 4. [`ParquetStorage::flush_events`] moves the hot rows to Parquet: stage
//!    `.pending`, delete hot rows, publish. A crash at any point leaves either
//!    rows in the hot table (durable, visible) or a staged file that startup
//!    recovery publishes. Re-running the same import is then a no-op.
//!
//! # Idempotency key
//!
//! An imported event is identified by `(site_id, visitor_id, timestamp,
//! event_name, pathname)`, compared as a multiset: if the store already holds
//! `k` rows with a key and the file holds `n`, `max(0, n - k)` are inserted.
//! The visitor id is a keyed hash of the source's own identifiers, so the same
//! source event always maps to the same key, and a second export that overlaps
//! the first adds only its new rows. Multiset rather than set so that two
//! genuinely identical events (a reload within the same second under
//! `round_timestamps`) are both kept the first time and neither again.
//!
//! # How imported rows behave in reports
//!
//! See `docs/src/importing-data.md#how-imported-rows-behave`; each statement
//! there is pinned by a test in this module.

pub mod cli;
pub mod ga4;

use crate::ingest::buffer::Event;
use crate::storage::parquet::{FlushError, ParquetStorage};
use chrono::{NaiveDate, NaiveDateTime};
use duckdb::Connection;
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;

/// Prefix of every visitor id written by an importer. Anything stored with it
/// came from an import; the live ingest path writes 64 hex characters.
pub const VISITOR_NAMESPACE: &str = "imp:";

/// Longest line read. A GA4 row with a long `items` array is a few KiB; a
/// megabyte-long line is a corrupt file, and bounding it bounds memory.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Errors listed individually; the rest are counted.
pub const MAX_LISTED_ERRORS: usize = 20;

/// Rows appended to the staging table before DuckDB is told to flush them.
const APPEND_CHUNK: usize = 50_000;

/// Name of the staging table in DuckDB's `temp` catalog.
const STAGE: &str = "import_stage";

/// Source formats with a verified, documented export schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// GA4 BigQuery export as newline-delimited JSON. See [`ga4`].
    Ga4BigQuery,
}

impl Format {
    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "ga4-bigquery" => Some(Self::Ga4BigQuery),
            _ => None,
        }
    }

    pub const SLUGS: &'static [&'static str] = &["ga4-bigquery"];
}

/// The privacy settings of the instance receiving the import, applied to
/// imported rows exactly as the ingest path applies them to live ones.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Policy {
    pub round_timestamps: bool,
    pub strip_referrer_query: bool,
    pub suppress_visitor_id: bool,
    pub suppress_browser_version: bool,
    pub suppress_os_version: bool,
    pub suppress_screen_size: bool,
    /// `city` | `region` | `country` | `none`.
    pub geoip_precision: String,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            round_timestamps: false,
            strip_referrer_query: false,
            suppress_visitor_id: false,
            suppress_browser_version: false,
            suppress_os_version: false,
            suppress_screen_size: false,
            geoip_precision: "city".to_string(),
        }
    }
}

impl Policy {
    /// The policy of a loaded configuration (`gdpr_mode` already applied).
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            round_timestamps: config.round_timestamps,
            strip_referrer_query: config.strip_referrer_query,
            suppress_visitor_id: config.suppress_visitor_id,
            suppress_browser_version: config.suppress_browser_version,
            suppress_os_version: config.suppress_os_version,
            suppress_screen_size: config.suppress_screen_size,
            geoip_precision: config.geoip_precision.clone(),
        }
    }
}

/// What parsing one line produced.
#[derive(Debug)]
pub enum Parsed {
    Event(Box<Event>),
    /// A valid row that is deliberately not imported, and why.
    Skip(String),
}

/// Everything an import needs besides the data.
#[derive(Debug, Clone)]
pub struct Options {
    pub format: Format,
    pub site_id: String,
    /// HMAC key for the pseudonymous visitor ids.
    pub key: Vec<u8>,
    pub policy: Policy,
    /// Keep the source's bookkeeping events too.
    pub include_all_events: bool,
    /// Import the valid rows even when some rows are invalid.
    pub skip_invalid: bool,
    /// "Now" in Unix seconds, for the future-timestamp check.
    pub now_secs: i64,
}

/// One rejected line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineError {
    /// 1-based.
    pub line: u64,
    pub message: String,
}

/// What an import did, or in a dry run would do.
#[derive(Debug, Default, Clone)]
pub struct Report {
    pub dry_run: bool,
    pub lines_read: u64,
    pub blank_lines: u64,
    /// Rows that became events.
    pub valid: u64,
    /// Rows rejected as malformed.
    pub invalid: u64,
    /// Valid rows deliberately left out, by reason.
    pub skipped: BTreeMap<String, u64>,
    /// The first [`MAX_LISTED_ERRORS`] rejected lines.
    pub errors: Vec<LineError>,
    pub first_timestamp: Option<NaiveDateTime>,
    pub last_timestamp: Option<NaiveDateTime>,
    pub days: BTreeSet<NaiveDate>,
    /// Valid rows the store already held. Known only after a real run.
    pub already_present: u64,
    /// Rows written. Known only after a real run.
    pub inserted: u64,
}

impl Report {
    fn note(&mut self, event: &Event) {
        self.valid += 1;
        let ts = event.timestamp;
        if self.first_timestamp.is_none_or(|first| ts < first) {
            self.first_timestamp = Some(ts);
        }
        if self.last_timestamp.is_none_or(|last| ts > last) {
            self.last_timestamp = Some(ts);
        }
        self.days.insert(ts.date());
    }

    fn reject(&mut self, line: u64, message: String) {
        self.invalid += 1;
        if self.errors.len() < MAX_LISTED_ERRORS {
            self.errors.push(LineError { line, message });
        }
    }

    /// Plain `key: value` lines, stable enough for a script to read.
    pub fn render(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "mode: {}",
            if self.dry_run { "dry-run" } else { "import" }
        );
        let _ = writeln!(out, "lines_read: {}", self.lines_read);
        let _ = writeln!(out, "blank_lines: {}", self.blank_lines);
        let _ = writeln!(out, "valid_events: {}", self.valid);
        let _ = writeln!(out, "invalid_lines: {}", self.invalid);
        let skipped: u64 = self.skipped.values().sum();
        let _ = writeln!(out, "skipped: {skipped}");
        for (reason, count) in &self.skipped {
            let _ = writeln!(out, "  skipped {count}: {reason}");
        }
        if let (Some(first), Some(last)) = (self.first_timestamp, self.last_timestamp) {
            let _ = writeln!(out, "first_event_utc: {first}");
            let _ = writeln!(out, "last_event_utc: {last}");
            let _ = writeln!(out, "days_covered: {}", self.days.len());
        }
        if self.dry_run {
            let _ = writeln!(
                out,
                "already_present: unknown (a dry run does not open the database)"
            );
            let _ = writeln!(out, "inserted: 0 (dry run, nothing written)");
        } else {
            let _ = writeln!(out, "already_present: {}", self.already_present);
            let _ = writeln!(out, "inserted: {}", self.inserted);
        }
        out
    }
}

/// Why an import stopped.
#[derive(Debug)]
pub enum ImportError {
    /// The input could not be read.
    Io(std::io::Error),
    /// Some lines were invalid and `skip_invalid` was off. Nothing was written.
    Invalid(Box<Report>),
    Db(duckdb::Error),
    /// The flush to Parquet failed. The rows are safe in the hot table or a
    /// staged file; running the import again finishes the job.
    Flush(FlushError),
    Other(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "could not read the input: {e}"),
            Self::Invalid(r) => write!(
                f,
                "{} invalid line(s); nothing was written (fix them, or pass --skip-invalid)",
                r.invalid
            ),
            Self::Db(e) => write!(f, "database error: {e}"),
            Self::Flush(e) => write!(
                f,
                "writing Parquet failed: {e}. The imported rows are not lost; run the same \
                 import again to finish it"
            ),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<duckdb::Error> for ImportError {
    fn from(e: duckdb::Error) -> Self {
        Self::Db(e)
    }
}

/// One line read within the size bound.
enum Read {
    Line(Vec<u8>),
    TooLong,
}

/// Read up to the next newline, never holding more than [`MAX_LINE_BYTES`].
fn read_bounded_line<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Read>> {
    let mut buf = Vec::new();
    let n = std::io::Read::take(reader.by_ref(), MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        return Ok(Some(Read::Line(buf)));
    }
    if buf.len() <= MAX_LINE_BYTES {
        return Ok(Some(Read::Line(buf))); // last line, no newline
    }
    // Discard the rest of the oversized line without keeping it.
    loop {
        let (used, done) = {
            let available = reader.fill_buf()?;
            available
                .iter()
                .position(|&b| b == b'\n')
                .map_or((available.len(), available.is_empty()), |p| (p + 1, true))
        };
        reader.consume(used);
        if done {
            break;
        }
    }
    Ok(Some(Read::TooLong))
}

/// Read, validate and hand each event to `sink`.
fn scan<R: BufRead>(
    mut reader: R,
    opts: &Options,
    mut sink: impl FnMut(&Event) -> Result<(), duckdb::Error>,
    report: &mut Report,
) -> Result<(), ImportError> {
    let mut number = 0u64;
    while let Some(read) = read_bounded_line(&mut reader).map_err(ImportError::Io)? {
        number += 1;
        report.lines_read = number;
        let bytes = match read {
            Read::Line(bytes) => bytes,
            Read::TooLong => {
                report.reject(
                    number,
                    format!("line is longer than {MAX_LINE_BYTES} bytes"),
                );
                continue;
            }
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            report.reject(number, "line is not valid UTF-8".to_string());
            continue;
        };
        // A byte-order mark on the first line is a text-editor artefact.
        let text = text.trim_start_matches('\u{feff}').trim();
        if text.is_empty() {
            report.blank_lines += 1;
            continue;
        }
        let parsed = match opts.format {
            Format::Ga4BigQuery => ga4::parse_line(
                text,
                &opts.site_id,
                &opts.policy,
                &opts.key,
                opts.include_all_events,
                opts.now_secs,
            ),
        };
        match parsed {
            Err(message) => report.reject(number, message),
            Ok(Parsed::Skip(reason)) => *report.skipped.entry(reason).or_default() += 1,
            Ok(Parsed::Event(event)) => {
                report.note(&event);
                sink(&event)?;
            }
        }
    }
    Ok(())
}

/// Validate a file and report what an import would do. Touches no database.
///
/// # Errors
///
/// Only [`ImportError::Io`]: invalid lines are in the report.
pub fn dry_run<R: BufRead>(reader: R, opts: &Options) -> Result<Report, ImportError> {
    let mut report = Report {
        dry_run: true,
        ..Report::default()
    };
    scan(reader, opts, |_| Ok(()), &mut report)?;
    Ok(report)
}

/// Bring a freshly opened database to the state the server would have before
/// it serves a request: migrated, interrupted writes recovered, view built.
///
/// # Errors
///
/// Returns a message when the database cannot be made ready; importing into it
/// would be unsafe.
pub fn prepare(conn: &Connection, storage: &ParquetStorage) -> Result<(), ImportError> {
    crate::storage::migrations::run_migrations(conn)?;
    storage
        .cleanup_temp_files()
        .map_err(|e| ImportError::Other(format!("could not clean up temporary files: {e}")))?;
    storage.recover_pending_flushes(conn)?;
    crate::storage::schema::setup_query_view(conn, storage.base_dir())?;
    Ok(())
}

fn columns() -> String {
    crate::storage::schema::EVENT_COLUMNS.join(", ")
}

/// Validate a file, stage it, and insert the rows that are not already stored
/// into the hot table. Stops before the Parquet flush; see [`import`].
///
/// # Errors
///
/// [`ImportError::Invalid`] when lines are invalid and `skip_invalid` is off
/// (nothing has been written to `events` by then), or a database error.
pub fn stage_and_merge<R: BufRead>(
    conn: &Connection,
    reader: R,
    opts: &Options,
) -> Result<Report, ImportError> {
    let cols = columns();
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS temp.{STAGE}; \
         CREATE TEMP TABLE {STAGE} AS SELECT {cols} FROM events WHERE false"
    ))?;

    let mut report = Report::default();
    let scanned = {
        let mut appender = conn.appender_to_catalog_and_db(STAGE, "temp", "main")?;
        let mut pending = 0usize;
        let result = scan(
            reader,
            opts,
            |e| {
                appender.append_row(duckdb::params![
                    e.site_id,
                    e.visitor_id,
                    e.timestamp,
                    e.event_name,
                    e.pathname,
                    e.hostname,
                    e.referrer,
                    e.referrer_source,
                    e.utm_source,
                    e.utm_medium,
                    e.utm_campaign,
                    e.utm_content,
                    e.utm_term,
                    e.browser,
                    e.browser_version,
                    e.os,
                    e.os_version,
                    e.device_type,
                    e.screen_size,
                    e.country_code,
                    e.region,
                    e.city,
                    e.props,
                    e.revenue_amount,
                    e.revenue_currency,
                ])?;
                pending += 1;
                if pending >= APPEND_CHUNK {
                    pending = 0;
                    appender.flush()?;
                }
                Ok(())
            },
            &mut report,
        );
        match result {
            Ok(()) => appender.flush().map_err(ImportError::from),
            Err(e) => Err(e),
        }
    };
    if let Err(e) = scanned {
        let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS temp.{STAGE}"));
        return Err(e);
    }

    if report.invalid > 0 && !opts.skip_invalid {
        let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS temp.{STAGE}"));
        return Err(ImportError::Invalid(Box::new(report)));
    }

    if report.valid > 0 {
        let (low, high) = (
            report.first_timestamp.unwrap_or_default(),
            report.last_timestamp.unwrap_or_default(),
        );
        let prefix = format!("{}%", opts_prefix(opts.format));
        let r = |c: &str| format!("r.{c}");
        let projection = crate::storage::schema::EVENT_COLUMNS
            .iter()
            .map(|c| r(c))
            .collect::<Vec<_>>()
            .join(", ");
        let inserted = conn.execute(
            &format!(
                "INSERT INTO events ({cols}) \
                 WITH ranked AS (\
                    SELECT {cols}, ROW_NUMBER() OVER (\
                        PARTITION BY site_id, visitor_id, timestamp, event_name, pathname) AS rn \
                    FROM temp.{STAGE}), \
                 existing AS (\
                    SELECT site_id, visitor_id, timestamp, event_name, pathname, COUNT(*) AS n \
                    FROM events_all \
                    WHERE site_id = ? AND visitor_id LIKE ? AND timestamp >= ? AND timestamp <= ? \
                    GROUP BY ALL) \
                 SELECT {projection} FROM ranked r LEFT JOIN existing e \
                   ON r.site_id = e.site_id AND r.visitor_id = e.visitor_id \
                  AND r.timestamp = e.timestamp AND r.event_name = e.event_name \
                  AND r.pathname = e.pathname \
                 WHERE r.rn > COALESCE(e.n, 0)"
            ),
            duckdb::params![opts.site_id, prefix, low, high],
        )?;
        report.inserted = inserted as u64;
        report.already_present = report.valid.saturating_sub(report.inserted);
    }
    conn.execute_batch(&format!("DROP TABLE IF EXISTS temp.{STAGE}"))?;
    Ok(report)
}

const fn opts_prefix(format: Format) -> &'static str {
    match format {
        Format::Ga4BigQuery => ga4::VISITOR_PREFIX,
    }
}

/// Move every hot row to Parquet, with the server's own flush.
///
/// # Errors
///
/// [`ImportError::Flush`] when a write fails; nothing is lost, and the next
/// run (of this or the server) completes it.
pub fn flush_all(conn: &Connection, storage: &ParquetStorage) -> Result<(), ImportError> {
    // Each call writes at most the per-flush partition cap; loop until a call
    // finds nothing left. The bound only stops a flush that makes no progress.
    for _ in 0..100_000 {
        if storage.flush_events(conn).map_err(ImportError::Flush)? == 0 {
            break;
        }
    }
    let left: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE visitor_id LIKE 'imp:%'",
        [],
        |row| row.get(0),
    )?;
    if left > 0 {
        return Err(ImportError::Other(format!(
            "{left} imported row(s) are still in the hot table after flushing; they are \
             durable and visible to reports, and the server flushes them on its next flush"
        )));
    }
    Ok(())
}

/// The whole import: [`stage_and_merge`], then [`flush_all`].
///
/// The database must already be [`prepare`]d.
///
/// # Errors
///
/// As for the two steps.
pub fn import<R: BufRead>(
    conn: &Connection,
    storage: &ParquetStorage,
    reader: R,
    opts: &Options,
) -> Result<Report, ImportError> {
    let report = stage_and_merge(conn, reader, opts)?;
    flush_all(conn, storage)?;
    Ok(report)
}

#[cfg(test)]
mod tests;
