use super::QueryScope;
use super::timeseries::{Granularity, TimeBucket};
use duckdb::Connection;
use serde::{Deserialize, Serialize};
use std::fmt::Write;

/// What an export request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    /// One row per day: visitors, pageviews, and that day's own top page/source.
    Daily,
    /// One row per stored event.
    Raw,
}

impl ExportKind {
    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "daily" => Some(Self::Daily),
            "raw" => Some(Self::Raw),
            _ => None,
        }
    }
}

/// Output format for an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
}

impl ExportFormat {
    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "csv" => Some(Self::Csv),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    pub const fn content_type(self) -> &'static str {
        match self {
            Self::Csv => "text/csv; charset=utf-8",
            Self::Json => "application/json",
        }
    }

    pub const fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
        }
    }
}

/// One row of a daily export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyRow {
    pub date: String,
    pub visitors: u64,
    pub pageviews: u64,
    /// The most-viewed page *on that day*.
    pub top_page: String,
    /// The most common source *on that day*.
    pub top_source: String,
}

/// Maximum rows a raw export will return.
///
/// A raw export streams whole events, so it needs a hard ceiling regardless of
/// the date-range cap.
pub const MAX_RAW_ROWS: usize = 1_000_000;

/// Build a daily export.
///
/// Each row carries the top page and source *for that day*. The previous export
/// computed a single top page and a single top source for the entire period and
/// then repeated those two strings on every row, which made the columns look
/// like per-day data while carrying none.
///
/// # Errors
///
/// Returns an error if a query fails.
pub fn query_daily_export(
    conn: &Connection,
    scope: &QueryScope,
) -> Result<Vec<DailyRow>, duckdb::Error> {
    let buckets = super::timeseries::query_timeseries(conn, scope, Granularity::Day)?;
    let top_pages = query_daily_top(conn, scope, "pathname")?;
    let top_sources = query_daily_top(conn, scope, "referrer_source")?;

    Ok(buckets
        .into_iter()
        .map(
            |TimeBucket {
                 date,
                 visitors,
                 pageviews,
             }| {
                let top_page = lookup(&top_pages, &date).unwrap_or_else(|| "(none)".to_string());
                let top_source =
                    lookup(&top_sources, &date).unwrap_or_else(|| "(direct)".to_string());
                DailyRow {
                    date,
                    visitors,
                    pageviews,
                    top_page,
                    top_source,
                }
            },
        )
        .collect())
}

fn lookup(pairs: &[(String, String)], date: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(d, _)| d == date)
        .map(|(_, value)| value.clone())
}

/// The leading value of one column for each day in range.
///
/// `column` is a fixed identifier chosen by the caller, never request input.
fn query_daily_top(
    conn: &Connection,
    scope: &QueryScope,
    column: &str,
) -> Result<Vec<(String, String)>, duckdb::Error> {
    let sql = format!(
        "WITH per_day AS (
             SELECT STRFTIME(DATE_TRUNC('day', timestamp), '%Y-%m-%d') AS day,
                    COALESCE({column}, '(unknown)') AS value,
                    COUNT(DISTINCT visitor_id) AS visitors
             FROM events_all
             WHERE {where_clause}
             GROUP BY day, value
         ),
         ranked AS (
             SELECT day, value,
                    ROW_NUMBER() OVER (PARTITION BY day ORDER BY visitors DESC, value) AS rank
             FROM per_day
         )
         SELECT day, value FROM ranked WHERE rank = 1 ORDER BY day",
        where_clause = scope.where_clause()
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(duckdb::params_from_iter(scope.params()), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

/// Columns emitted by a raw export.
///
/// `visitor_id` is deliberately excluded. It is a pseudonymous identifier, and
/// a CSV of per-event pseudonyms leaving the server is exactly the artefact
/// this project exists to avoid producing.
pub const RAW_COLUMNS: &[&str] = &[
    "timestamp",
    "event_name",
    "pathname",
    "hostname",
    "referrer",
    "referrer_source",
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_content",
    "utm_term",
    "browser",
    "browser_version",
    "os",
    "os_version",
    "device_type",
    "screen_size",
    "country_code",
    "region",
    "city",
    "props",
    "revenue_amount",
    "revenue_currency",
];

/// Rows buffered per chunk by [`stream_raw_export`] callers that have no
/// better number. 10k rows of 23 short strings is a few MB.
pub const EXPORT_CHUNK_ROWS: usize = 10_000;

/// The raw-export statement. `limit` is interpolated: it is a `usize` already
/// capped by MAX_RAW_ROWS, so it cannot carry SQL, and binding it would place
/// an integer parameter after a variable number of filter values.
fn raw_export_sql(scope: &QueryScope, limit: usize) -> String {
    let limit = limit.min(MAX_RAW_ROWS);
    // Every column is cast to VARCHAR so one row-mapping path handles them all.
    let projection = RAW_COLUMNS
        .iter()
        .map(|c| format!("CAST({c} AS VARCHAR)"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT {projection} FROM events_all WHERE {} ORDER BY timestamp LIMIT {limit}",
        scope.where_clause()
    )
}

fn read_raw_row(row: &duckdb::Row<'_>) -> Result<Vec<Option<String>>, duckdb::Error> {
    (0..RAW_COLUMNS.len())
        .map(|i| row.get::<_, Option<String>>(i))
        .collect()
}

/// Export raw events as rows of nullable strings.
///
/// This materialises every row (and `to_csv`/`raw_to_json` then build a second
/// full copy), so memory is a multiple of the result size. Prefer
/// [`stream_raw_export`] for anything near [`MAX_RAW_ROWS`].
///
/// # Errors
///
/// Returns an error if the query fails or a row cannot be read. A row that
/// cannot be read used to be dropped silently, which produced an export that
/// looked complete and was not.
pub fn query_raw_export(
    conn: &Connection,
    scope: &QueryScope,
    limit: usize,
) -> Result<Vec<Vec<Option<String>>>, duckdb::Error> {
    let mut stmt = conn.prepare(&raw_export_sql(scope, limit))?;
    stmt.query_map(duckdb::params_from_iter(scope.params()), |row| {
        read_raw_row(row)
    })?
    .collect()
}

/// Export raw events in chunks of at most `chunk_rows`, handing each to `sink`
/// instead of collecting them (S-12).
///
/// Peak memory is one chunk plus whatever the sink retains, rather than the
/// whole result several times over. `sink` returns `false` to stop early, for
/// example when the HTTP client has gone away. Returns the number of rows
/// delivered.
///
/// The statement stays open across chunks, so the caller must keep whatever
/// protects the read (the tier read lock, and the connection) for the whole
/// call. A handler streaming to a socket would run this on a blocking thread
/// and push each chunk into a bounded channel.
///
/// # Errors
///
/// Returns an error if the query fails or a row cannot be read. Chunks already
/// delivered cannot be recalled, so a streaming consumer must treat an error as
/// a truncated body.
pub fn stream_raw_export<F>(
    conn: &Connection,
    scope: &QueryScope,
    limit: usize,
    chunk_rows: usize,
    mut sink: F,
) -> Result<usize, duckdb::Error>
where
    F: FnMut(&[Vec<Option<String>>]) -> bool,
{
    let chunk_rows = chunk_rows.max(1);
    let mut stmt = conn.prepare(&raw_export_sql(scope, limit))?;
    let mut rows = stmt.query(duckdb::params_from_iter(scope.params()))?;

    let mut delivered = 0usize;
    let mut chunk: Vec<Vec<Option<String>>> = Vec::with_capacity(chunk_rows.min(EXPORT_CHUNK_ROWS));
    while let Some(row) = rows.next()? {
        chunk.push(read_raw_row(row)?);
        if chunk.len() >= chunk_rows {
            delivered += chunk.len();
            let keep_going = sink(&chunk);
            chunk.clear();
            if !keep_going {
                return Ok(delivered);
            }
        }
    }
    if !chunk.is_empty() {
        delivered += chunk.len();
        sink(&chunk);
    }
    Ok(delivered)
}

/// Escape a CSV field.
///
/// Always quotes, doubles internal quotes, and neutralises the leading
/// characters that spreadsheets interpret as formulas.
pub fn escape_csv_field(field: &str) -> String {
    let escaped = field.replace('"', "\"\"");
    if escaped.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("\"'{escaped}\"")
    } else {
        format!("\"{escaped}\"")
    }
}

/// The CSV header line, with its newline.
pub fn csv_header_line(header: &[&str]) -> String {
    let mut out = header
        .iter()
        .map(|h| escape_csv_field(h))
        .collect::<Vec<_>>()
        .join(",");
    out.push('\n');
    out
}

/// Render data rows as CSV lines, without a header. Chunks concatenate.
pub fn csv_rows(rows: &[Vec<Option<String>>]) -> String {
    let mut out = String::with_capacity(rows.len() * 128);
    for row in rows {
        let line = row
            .iter()
            .map(|v| escape_csv_field(v.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join(",");
        let _ = writeln!(out, "{line}");
    }
    out
}

/// Render a header row and data rows as CSV.
pub fn to_csv(header: &[&str], rows: &[Vec<Option<String>>]) -> String {
    let mut out = csv_header_line(header);
    out.push_str(&csv_rows(rows));
    out
}

/// Render daily rows as CSV.
pub fn daily_to_csv(rows: &[DailyRow]) -> String {
    let mut out = String::from("date,visitors,pageviews,top_page,top_source\n");
    for row in rows {
        let _ = writeln!(
            out,
            "{},{},{},{},{}",
            row.date,
            row.visitors,
            row.pageviews,
            escape_csv_field(&row.top_page),
            escape_csv_field(&row.top_source)
        );
    }
    out
}

fn raw_row_object(row: &[Option<String>]) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> = RAW_COLUMNS
        .iter()
        .zip(row.iter())
        .map(|(name, value)| {
            let json = value.as_deref().map_or(serde_json::Value::Null, |v| {
                serde_json::Value::String(v.to_string())
            });
            ((*name).to_string(), json)
        })
        .collect();
    serde_json::Value::Object(map)
}

/// Render raw rows as a JSON array of objects.
pub fn raw_to_json(rows: &[Vec<Option<String>>]) -> String {
    let objects: Vec<serde_json::Value> = rows.iter().map(|r| raw_row_object(r)).collect();
    serde_json::to_string(&objects).unwrap_or_else(|_| "[]".to_string())
}

/// Opening bracket of a streamed raw JSON export.
pub const JSON_STREAM_OPEN: &str = "[";
/// Closing bracket of a streamed raw JSON export.
pub const JSON_STREAM_CLOSE: &str = "]";

/// One chunk of a streamed raw JSON export.
///
/// The row objects are comma separated, with a leading comma unless this is
/// the first chunk. Emit
/// [`JSON_STREAM_OPEN`], every chunk in order, then [`JSON_STREAM_CLOSE`], and
/// the concatenation equals [`raw_to_json`] of all the rows.
pub fn raw_json_chunk(rows: &[Vec<Option<String>>], first_chunk: bool) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let body = rows
        .iter()
        // `Value`'s Display is infallible, unlike `to_string` on arbitrary types.
        .map(|r| raw_row_object(r).to_string())
        .collect::<Vec<_>>()
        .join(",");
    if first_chunk {
        body
    } else {
        format!(",{body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_support::{TestDb, insert_pageview, scope};

    #[test]
    fn test_daily_export_reports_each_day_top_page() {
        // Regression: the export computed one top page for the whole period and
        // repeated it on every row, so a column labelled per-day carried none.
        let db = TestDb::new();
        // Day 1: /alpha leads.
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/alpha");
        insert_pageview(&db.conn, "v2", "2024-01-15 11:00:00", "/alpha");
        insert_pageview(&db.conn, "v3", "2024-01-15 12:00:00", "/beta");
        // Day 2: /beta leads.
        insert_pageview(&db.conn, "v4", "2024-01-16 10:00:00", "/beta");
        insert_pageview(&db.conn, "v5", "2024-01-16 11:00:00", "/beta");
        insert_pageview(&db.conn, "v6", "2024-01-16 12:00:00", "/alpha");

        let rows = query_daily_export(&db.conn, &scope("2024-01-15", "2024-01-17")).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].date, "2024-01-15");
        assert_eq!(rows[0].top_page, "/alpha");
        assert_eq!(rows[1].date, "2024-01-16");
        assert_eq!(rows[1].top_page, "/beta", "each day gets its own leader");
    }

    #[test]
    fn test_daily_export_includes_empty_days() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        let rows = query_daily_export(&db.conn, &scope("2024-01-14", "2024-01-17")).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].visitors, 0);
        assert_eq!(rows[0].top_page, "(none)");
        assert_eq!(rows[1].visitors, 1);
    }

    #[test]
    fn test_daily_export_defaults_source_to_direct() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        let rows = query_daily_export(&db.conn, &scope("2024-01-15", "2024-01-16")).unwrap();
        assert_eq!(rows[0].top_source, "(unknown)");
    }

    #[test]
    fn test_raw_export_returns_one_row_per_event() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&db.conn, "v1", "2024-01-15 10:05:00", "/about");

        let rows = query_raw_export(&db.conn, &scope("2024-01-01", "2024-02-01"), 1000).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), RAW_COLUMNS.len());
    }

    #[test]
    fn test_raw_export_never_contains_the_visitor_id() {
        // A CSV of per-event pseudonyms is exactly what this project exists to
        // avoid producing.
        assert!(!RAW_COLUMNS.contains(&"visitor_id"));
        let db = TestDb::new();
        insert_pageview(&db.conn, "secret-visitor-hash", "2024-01-15 10:00:00", "/");
        let rows = query_raw_export(&db.conn, &scope("2024-01-01", "2024-02-01"), 10).unwrap();
        let csv = to_csv(RAW_COLUMNS, &rows);
        assert!(!csv.contains("secret-visitor-hash"));
    }

    #[test]
    fn test_raw_export_respects_the_limit() {
        let db = TestDb::new();
        for i in 0..10 {
            insert_pageview(&db.conn, "v1", &format!("2024-01-15 10:0{i}:00"), "/");
        }
        let rows = query_raw_export(&db.conn, &scope("2024-01-01", "2024-02-01"), 3).unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn test_raw_export_is_ordered_by_time() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 12:00:00", "/late");
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/early");
        let rows = query_raw_export(&db.conn, &scope("2024-01-01", "2024-02-01"), 10).unwrap();
        assert_eq!(rows[0][2].as_deref(), Some("/early"));
        assert_eq!(rows[1][2].as_deref(), Some("/late"));
    }

    #[test]
    fn test_csv_escaping() {
        assert_eq!(escape_csv_field("/about"), "\"/about\"");
        assert_eq!(escape_csv_field("it's \"great\""), "\"it's \"\"great\"\"\"");
    }

    #[test]
    fn test_csv_formula_injection_is_neutralised() {
        assert_eq!(escape_csv_field("=CMD|'/c calc'"), "\"'=CMD|'/c calc'\"");
        assert_eq!(escape_csv_field("+1+2"), "\"'+1+2\"");
        assert_eq!(escape_csv_field("-1-2"), "\"'-1-2\"");
        assert_eq!(escape_csv_field("@SUM(A1)"), "\"'@SUM(A1)\"");
        // Leading tab and CR are also treated as formula leaders by Excel.
        assert_eq!(escape_csv_field("\t=1+1"), "\"'\t=1+1\"");
    }

    #[test]
    fn test_to_csv_shape() {
        let rows = vec![
            vec![Some("a".to_string()), None],
            vec![Some("b".to_string()), Some("c".to_string())],
        ];
        let csv = to_csv(&["one", "two"], &rows);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "\"one\",\"two\"");
        assert_eq!(lines[1], "\"a\",\"\"");
        assert_eq!(lines[2], "\"b\",\"c\"");
    }

    #[test]
    fn test_raw_to_json_uses_column_names() {
        let rows = vec![vec![Some("x".to_string()); RAW_COLUMNS.len()]];
        let json: serde_json::Value = serde_json::from_str(&raw_to_json(&rows)).unwrap();
        assert_eq!(json[0]["pathname"], "x");
        assert!(json[0].get("visitor_id").is_none());
    }

    #[test]
    fn test_raw_to_json_renders_nulls() {
        let rows = vec![vec![None; RAW_COLUMNS.len()]];
        let json: serde_json::Value = serde_json::from_str(&raw_to_json(&rows)).unwrap();
        assert!(json[0]["referrer"].is_null());
    }

    #[test]
    fn test_daily_csv_header_and_rows() {
        let rows = vec![DailyRow {
            date: "2024-01-15".to_string(),
            visitors: 2,
            pageviews: 3,
            top_page: "/".to_string(),
            top_source: "Google".to_string(),
        }];
        let csv = daily_to_csv(&rows);
        assert!(csv.starts_with("date,visitors,pageviews,top_page,top_source\n"));
        assert!(csv.contains("2024-01-15,2,3,\"/\",\"Google\""));
    }

    #[test]
    fn test_slug_parsing() {
        assert_eq!(ExportKind::from_slug("daily"), Some(ExportKind::Daily));
        assert_eq!(ExportKind::from_slug("raw"), Some(ExportKind::Raw));
        assert_eq!(ExportKind::from_slug("xml"), None);
        assert_eq!(ExportFormat::from_slug("csv"), Some(ExportFormat::Csv));
        assert_eq!(ExportFormat::from_slug("json"), Some(ExportFormat::Json));
        assert_eq!(ExportFormat::from_slug("xml"), None);
    }

    #[test]
    fn test_format_metadata() {
        assert!(ExportFormat::Csv.content_type().contains("text/csv"));
        assert_eq!(ExportFormat::Json.extension(), "json");
    }

    fn seeded(n: usize) -> TestDb {
        let db = TestDb::new();
        for i in 0..n {
            insert_pageview(
                &db.conn,
                &format!("v{i}"),
                &format!("2024-01-15 10:{:02}:00", i % 60),
                &format!("/p{i}"),
            );
        }
        db
    }

    #[test]
    fn test_streamed_export_equals_the_materialised_one() {
        // S-12: the chunked path must produce exactly the bytes the
        // collect-everything path does, for CSV and JSON, whatever the chunking.
        let db = seeded(7);
        let sc = scope("2024-01-15", "2024-01-16");
        let all = query_raw_export(&db.conn, &sc, 100).unwrap();
        assert_eq!(all.len(), 7);

        for chunk_rows in [1, 2, 3, 7, 100] {
            let mut csv = csv_header_line(RAW_COLUMNS);
            let mut json = String::from(JSON_STREAM_OPEN);
            let mut first = true;
            let mut chunks = 0;
            let n = stream_raw_export(&db.conn, &sc, 100, chunk_rows, |rows| {
                assert!(rows.len() <= chunk_rows, "chunk exceeded its bound");
                csv.push_str(&csv_rows(rows));
                json.push_str(&raw_json_chunk(rows, first));
                first = false;
                chunks += 1;
                true
            })
            .unwrap();
            json.push_str(JSON_STREAM_CLOSE);
            assert_eq!(n, 7);
            assert_eq!(chunks, 7usize.div_ceil(chunk_rows));
            assert_eq!(csv, to_csv(RAW_COLUMNS, &all), "chunk_rows={chunk_rows}");
            assert_eq!(json, raw_to_json(&all), "chunk_rows={chunk_rows}");
        }
    }

    #[test]
    fn test_streamed_export_stops_when_the_sink_says_so() {
        let db = seeded(10);
        let sc = scope("2024-01-15", "2024-01-16");
        let mut seen = 0;
        let n = stream_raw_export(&db.conn, &sc, 100, 3, |rows| {
            seen += rows.len();
            false
        })
        .unwrap();
        assert_eq!((n, seen), (3, 3), "no chunk may be produced after a stop");
    }

    #[test]
    fn test_streamed_export_of_nothing_calls_the_sink_never() {
        let db = TestDb::new();
        let sc = scope("2024-01-15", "2024-01-16");
        let n = stream_raw_export(&db.conn, &sc, 100, 5, |_| panic!("no rows, no chunks")).unwrap();
        assert_eq!(n, 0);
        assert_eq!(raw_json_chunk(&[], true), "");
    }

    #[test]
    fn test_streamed_export_honours_the_limit() {
        let db = seeded(10);
        let sc = scope("2024-01-15", "2024-01-16");
        let n = stream_raw_export(&db.conn, &sc, 4, 3, |_| true).unwrap();
        assert_eq!(n, 4);
    }
}
