use duckdb::Connection;
use std::fs;
use std::path::{Path, PathBuf};

/// Manages Parquet file storage with a date-partitioned layout.
///
/// Storage layout:
/// ```text
/// data/events/site_id=example.com/date=2024-01-15/0001.parquet
/// ```
#[derive(Clone)]
pub struct ParquetStorage {
    base_dir: PathBuf,
    /// Compact a partition once it holds at least this many files. 0 = never.
    compact_after_files: usize,
}

/// Validate that a component is safe to use as a filesystem path segment.
///
/// Rejects path traversal sequences, separators, control characters, and names
/// that are special on any supported platform.
fn is_safe_path_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s != "."
        && s != ".."
        && !s.contains("..")
        && !s.contains('/')
        && !s.contains('\\')
        && !s.starts_with('.')
        && !s.chars().any(char::is_control)
}

/// Escape a string for embedding in a single-quoted DuckDB SQL literal.
fn sql_quote(value: &str) -> String {
    value.replace('\'', "''")
}

impl ParquetStorage {
    pub fn new(base_dir: &Path, compact_after_files: usize) -> Self {
        Self {
            base_dir: base_dir.to_path_buf(),
            compact_after_files,
        }
    }

    /// The base events directory.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// The partition directory for a given site and date.
    pub fn partition_dir(&self, site_id: &str, date: &str) -> PathBuf {
        self.base_dir
            .join(format!("site_id={site_id}"))
            .join(format!("date={date}"))
    }

    /// Highest existing file number in a partition, or 0 when empty.
    ///
    /// A single `read_dir` replaces the O(n) `exists()` probing the previous
    /// implementation used.
    fn max_file_number(dir: &Path) -> u32 {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()?
                    .strip_suffix(".parquet")?
                    .parse::<u32>()
                    .ok()
            })
            .max()
            .unwrap_or(0)
    }

    /// List the numbered Parquet files in a partition, sorted ascending.
    fn partition_files(dir: &Path) -> Vec<(u32, PathBuf)> {
        let mut files: Vec<(u32, PathBuf)> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let num = name
                    .to_str()?
                    .strip_suffix(".parquet")?
                    .parse::<u32>()
                    .ok()?;
                Some((num, entry.path()))
            })
            .collect();
        files.sort_unstable_by_key(|(num, _)| *num);
        files
    }

    /// The next available Parquet file path in a partition.
    fn next_file_path(&self, site_id: &str, date: &str) -> std::io::Result<PathBuf> {
        let dir = self.partition_dir(site_id, date);
        fs::create_dir_all(&dir)?;
        Ok(dir.join(format!("{:04}.parquet", Self::max_file_number(&dir) + 1)))
    }

    /// Write a query's result set to `final_path` atomically.
    ///
    /// DuckDB's `COPY TO` streams directly into the destination, so a crash or a
    /// full disk mid-write leaves a truncated file behind. Because `events_all`
    /// globs every `*.parquet` in the tree, one truncated file makes *every*
    /// subsequent query fail — permanently, until an operator finds and deletes
    /// it. Writing to a temporary name and renaming into place makes the file
    /// appear only once it is complete; `rename` within a directory is atomic on
    /// every platform this runs on.
    fn copy_to_parquet_atomically(
        conn: &Connection,
        select_sql: &str,
        final_path: &Path,
    ) -> Result<(), FlushError> {
        // The temporary name deliberately does not end in `.parquet`, so a
        // leftover from a crashed process is invisible to the read glob.
        let tmp_path = final_path.with_extension("parquet.tmp");
        let tmp_str = sql_quote(&tmp_path.to_string_lossy());

        let copy_sql =
            format!("COPY ({select_sql}) TO '{tmp_str}' (FORMAT PARQUET, COMPRESSION ZSTD)");

        if let Err(e) = conn.execute_batch(&copy_sql) {
            let _ = fs::remove_file(&tmp_path);
            return Err(FlushError::Write(e));
        }

        fs::rename(&tmp_path, final_path).map_err(|e| {
            let _ = fs::remove_file(&tmp_path);
            FlushError::Rename(e)
        })
    }

    /// Remove `*.parquet.tmp` files left behind by an interrupted write.
    ///
    /// Called at startup. These files are invisible to queries, but they waste
    /// disk indefinitely if nothing cleans them up.
    pub fn cleanup_temp_files(&self) -> std::io::Result<usize> {
        let mut removed = 0usize;
        let Ok(sites) = fs::read_dir(&self.base_dir) else {
            return Ok(0);
        };
        for site in sites.flatten() {
            if !site.path().is_dir() {
                continue;
            }
            let Ok(dates) = fs::read_dir(site.path()) else {
                continue;
            };
            for date in dates.flatten() {
                if !date.path().is_dir() {
                    continue;
                }
                let Ok(files) = fs::read_dir(date.path()) else {
                    continue;
                };
                for file in files.flatten() {
                    if file.file_name().to_string_lossy().ends_with(".parquet.tmp")
                        && fs::remove_file(file.path()).is_ok()
                    {
                        removed += 1;
                    }
                }
            }
        }
        Ok(removed)
    }

    /// Flush events from the DuckDB `events` table to partitioned Parquet files.
    ///
    /// Groups events by (site_id, date) and writes each partition to its own file.
    ///
    /// # Errors
    ///
    /// Returns an error if the partition query, the Parquet write, the rename, or
    /// the follow-up delete fails.
    pub fn flush_events(&self, conn: &Connection) -> Result<usize, FlushError> {
        let mut stmt = conn
            .prepare(
                "SELECT site_id, STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d') AS d, COUNT(*) AS cnt \
                 FROM events GROUP BY site_id, d ORDER BY site_id, d",
            )
            .map_err(FlushError::Query)?;

        let partitions: Vec<(String, String, usize)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(FlushError::Query)?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);

        if partitions.is_empty() {
            return Ok(0);
        }

        let mut total_flushed = 0usize;
        let mut touched: Vec<(String, String)> = Vec::new();

        for (site_id, date, count) in &partitions {
            if !is_safe_path_component(site_id) {
                tracing::warn!(site_id, "Skipping flush for unsafe site_id");
                continue;
            }
            // `date` is produced by STRFTIME so it cannot contain a quote, but it
            // is escaped anyway rather than relying on that invariant holding.
            let escaped_site = sql_quote(site_id);
            let escaped_date = sql_quote(date);
            let predicate = format!(
                "site_id = '{escaped_site}' \
                 AND STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d') = '{escaped_date}'"
            );

            let file_path = self
                .next_file_path(site_id, date)
                .map_err(FlushError::Rename)?;

            Self::copy_to_parquet_atomically(
                conn,
                &format!("SELECT * FROM events WHERE {predicate}"),
                &file_path,
            )?;

            total_flushed += count;
            touched.push((site_id.clone(), date.clone()));

            // Only now that the Parquet file is durably in place do we drop the
            // rows from the hot table.
            conn.execute_batch(&format!("DELETE FROM events WHERE {predicate}"))
                .map_err(FlushError::Delete)?;
        }

        if total_flushed > 0 {
            for (site_id, date) in &touched {
                if let Err(e) = self.compact_partition(conn, site_id, date) {
                    // Compaction is an optimisation; a failure must not lose data
                    // or fail the flush that already succeeded.
                    tracing::warn!(site_id, date, error = %e, "Parquet compaction failed");
                }
            }
            // Recreate the union view so the newly written files are visible.
            let _ = crate::storage::schema::setup_query_view(conn, &self.base_dir);
        }

        Ok(total_flushed)
    }

    /// Merge the small Parquet files in a partition into a single file.
    ///
    /// A 60-second flush interval writes ~1440 files per site per day. DuckDB
    /// opens and reads the footer of every matching file on every query, so
    /// without compaction scan cost grows linearly with uptime.
    ///
    /// The merged file is written under a temporary name and renamed into place
    /// before the sources are deleted, so an interruption at any point leaves
    /// either the old files or the new one — never neither.
    ///
    /// Returns the number of source files merged (0 if compaction was not due).
    ///
    /// # Errors
    ///
    /// Returns an error if reading, writing, or renaming the Parquet files fails.
    pub fn compact_partition(
        &self,
        conn: &Connection,
        site_id: &str,
        date: &str,
    ) -> Result<usize, FlushError> {
        if self.compact_after_files == 0 || !is_safe_path_component(site_id) {
            return Ok(0);
        }
        let dir = self.partition_dir(site_id, date);
        let files = Self::partition_files(&dir);
        if files.len() < self.compact_after_files {
            return Ok(0);
        }

        let glob = sql_quote(&dir.join("*.parquet").to_string_lossy());
        // Write the merged output above the existing numbering so it sorts last
        // and cannot collide with a concurrent flush's next_file_path().
        let merged_number = Self::max_file_number(&dir) + 1;
        let merged_path = dir.join(format!("{merged_number:04}.parquet"));

        Self::copy_to_parquet_atomically(
            conn,
            &format!(
                "SELECT * FROM read_parquet('{glob}', union_by_name=true, hive_partitioning=false)"
            ),
            &merged_path,
        )?;

        // The merged file now contains everything the sources held, so removing
        // them cannot lose data. A failure here leaves duplicates, so bail out
        // by deleting the merged file instead.
        for (_, path) in &files {
            if let Err(e) = fs::remove_file(path) {
                tracing::error!(
                    path = %path.display(),
                    error = %e,
                    "Could not remove a compacted source file; discarding the merged \
                     file to avoid double-counting"
                );
                let _ = fs::remove_file(&merged_path);
                return Err(FlushError::Rename(e));
            }
        }

        tracing::info!(
            site_id,
            date,
            merged = files.len(),
            "Compacted Parquet partition"
        );
        Ok(files.len())
    }

    /// Delete Parquet partition directories older than `retention_days`.
    ///
    /// Returns the number of partition directories removed.
    ///
    /// # Errors
    ///
    /// Returns an error if a directory cannot be read or removed.
    pub fn cleanup_old_partitions(&self, retention_days: u32) -> std::io::Result<usize> {
        if retention_days == 0 {
            return Ok(0);
        }

        let cutoff =
            chrono::Utc::now().date_naive() - chrono::Duration::days(i64::from(retention_days));
        let cutoff_str = cutoff.format("%Y-%m-%d").to_string();
        let mut removed = 0usize;

        let entries = match fs::read_dir(&self.base_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };

        for site_entry in entries.flatten() {
            let site_path = site_entry.path();
            if !site_path.is_dir() {
                continue;
            }
            for date_entry in fs::read_dir(&site_path)?.flatten() {
                let date_path = date_entry.path();
                if !date_path.is_dir() {
                    continue;
                }
                let dir_name = date_entry.file_name();
                let dir_name = dir_name.to_string_lossy();
                if let Some(date_str) = dir_name.strip_prefix("date=") {
                    // ISO-8601 dates compare correctly as strings.
                    if date_str < cutoff_str.as_str() {
                        fs::remove_dir_all(&date_path)?;
                        removed += 1;
                    }
                }
            }
        }

        Ok(removed)
    }

    /// Delete the on-disk partitions for a site across an inclusive date range.
    ///
    /// Returns the number of partition directories removed.
    ///
    /// # Errors
    ///
    /// Returns an error if `site_id` is not a safe path component.
    pub fn erase_partitions(
        &self,
        site_id: &str,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
    ) -> std::io::Result<u64> {
        if !is_safe_path_component(site_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unsafe site_id",
            ));
        }
        let mut removed = 0u64;
        let mut current = start;
        while current <= end {
            let date_str = current.format("%Y-%m-%d").to_string();
            let dir = self.partition_dir(site_id, &date_str);
            if dir.exists() {
                match fs::remove_dir_all(&dir) {
                    Ok(()) => {
                        removed += 1;
                        tracing::info!(site_id, date = %date_str, "Removed Parquet partition");
                    }
                    Err(e) => tracing::warn!(
                        site_id,
                        date = %date_str,
                        error = %e,
                        "Failed to remove Parquet partition"
                    ),
                }
            }
            let Some(next) = current.succ_opt() else {
                break;
            };
            current = next;
        }
        Ok(removed)
    }

    /// Site IDs that have at least one on-disk partition.
    pub fn known_site_ids(&self) -> Vec<String> {
        let mut sites: Vec<String> = fs::read_dir(&self.base_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_prefix("site_id="))
                    .map(str::to_string)
            })
            .collect();
        sites.sort_unstable();
        sites.dedup();
        sites
    }
}

#[derive(Debug)]
pub enum FlushError {
    Query(duckdb::Error),
    Write(duckdb::Error),
    Delete(duckdb::Error),
    Rename(std::io::Error),
}

impl std::fmt::Display for FlushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Query(e) => write!(f, "Query error: {e}"),
            Self::Write(e) => write!(f, "Write error: {e}"),
            Self::Delete(e) => write!(f, "Delete error: {e}"),
            Self::Rename(e) => write!(f, "Rename error: {e}"),
        }
    }
}

impl std::error::Error for FlushError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Query(e) | Self::Write(e) | Self::Delete(e) => Some(e),
            Self::Rename(e) => Some(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        conn
    }

    fn insert_test_event(conn: &Connection, site_id: &str, timestamp: &str, pathname: &str) {
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
             VALUES (?, ?, CAST(? AS TIMESTAMP), 'pageview', ?)",
            duckdb::params![site_id, "visitor1", timestamp, pathname],
        )
        .unwrap();
    }

    #[test]
    fn test_partition_dir() {
        let storage = ParquetStorage::new(Path::new("/data/events"), 0);
        assert_eq!(
            storage.partition_dir("example.com", "2024-01-15"),
            PathBuf::from("/data/events/site_id=example.com/date=2024-01-15")
        );
    }

    #[test]
    fn test_flush_empty_table() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);
        assert_eq!(storage.flush_events(&conn).unwrap(), 0);
    }

    #[test]
    fn test_flush_and_verify() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);

        insert_test_event(&conn, "example.com", "2024-01-15 10:00:00", "/");
        insert_test_event(&conn, "example.com", "2024-01-15 11:00:00", "/about");

        assert_eq!(storage.flush_events(&conn).unwrap(), 2);

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
        assert!(
            storage
                .partition_dir("example.com", "2024-01-15")
                .join("0001.parquet")
                .exists()
        );
    }

    #[test]
    fn test_flush_multiple_sites() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);

        insert_test_event(&conn, "site-a.com", "2024-01-15 10:00:00", "/");
        insert_test_event(&conn, "site-b.com", "2024-01-15 10:00:00", "/");

        assert_eq!(storage.flush_events(&conn).unwrap(), 2);
        assert!(
            storage
                .partition_dir("site-a.com", "2024-01-15")
                .join("0001.parquet")
                .exists()
        );
        assert!(
            storage
                .partition_dir("site-b.com", "2024-01-15")
                .join("0001.parquet")
                .exists()
        );
    }

    #[test]
    fn test_flush_multiple_dates() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);

        insert_test_event(&conn, "example.com", "2024-01-15 10:00:00", "/");
        insert_test_event(&conn, "example.com", "2024-01-16 10:00:00", "/");

        assert_eq!(storage.flush_events(&conn).unwrap(), 2);
        assert!(
            storage
                .partition_dir("example.com", "2024-01-15")
                .join("0001.parquet")
                .exists()
        );
        assert!(
            storage
                .partition_dir("example.com", "2024-01-16")
                .join("0001.parquet")
                .exists()
        );
    }

    #[test]
    fn test_incremental_file_numbering() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);

        insert_test_event(&conn, "example.com", "2024-01-15 10:00:00", "/");
        storage.flush_events(&conn).unwrap();
        insert_test_event(&conn, "example.com", "2024-01-15 11:00:00", "/about");
        storage.flush_events(&conn).unwrap();

        let partition = storage.partition_dir("example.com", "2024-01-15");
        assert!(partition.join("0001.parquet").exists());
        assert!(partition.join("0002.parquet").exists());
    }

    #[test]
    fn test_flush_leaves_no_temp_files() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);
        insert_test_event(&conn, "example.com", "2024-01-15 10:00:00", "/");
        storage.flush_events(&conn).unwrap();

        let partition = storage.partition_dir("example.com", "2024-01-15");
        let temps: Vec<_> = fs::read_dir(&partition)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(
            temps.is_empty(),
            "temp files must be renamed away: {temps:?}"
        );
    }

    #[test]
    fn test_cleanup_temp_files_removes_interrupted_writes() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);
        let partition = storage.partition_dir("example.com", "2024-01-15");
        fs::create_dir_all(&partition).unwrap();
        fs::write(partition.join("0001.parquet"), b"real").unwrap();
        fs::write(partition.join("0002.parquet.tmp"), b"interrupted").unwrap();

        assert_eq!(storage.cleanup_temp_files().unwrap(), 1);
        assert!(partition.join("0001.parquet").exists());
        assert!(!partition.join("0002.parquet.tmp").exists());
    }

    #[test]
    fn test_temp_file_is_invisible_to_the_read_glob() {
        // The interrupted-write file must not end in `.parquet`, or a crash
        // would leave a truncated file that breaks every subsequent query.
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);
        let partition = storage.partition_dir("s.com", "2024-01-15");
        fs::create_dir_all(&partition).unwrap();
        let tmp = partition.join("0001.parquet").with_extension("parquet.tmp");
        assert!(!tmp.to_string_lossy().ends_with(".parquet"));
        assert_eq!(tmp.file_name().unwrap(), "0001.parquet.tmp");
    }

    #[test]
    fn test_compaction_merges_small_files() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 3);

        // Three flushes -> three files -> compaction triggers on the third.
        for hour in 10..13 {
            insert_test_event(
                &conn,
                "example.com",
                &format!("2024-01-15 {hour}:00:00"),
                "/",
            );
            storage.flush_events(&conn).unwrap();
        }

        let partition = storage.partition_dir("example.com", "2024-01-15");
        let files = ParquetStorage::partition_files(&partition);
        assert_eq!(files.len(), 1, "expected one merged file, got {files:?}");

        // All three rows must survive the merge.
        crate::storage::schema::setup_query_view(&conn, dir.path()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events_all", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3, "compaction must not lose rows");
    }

    #[test]
    fn test_compaction_is_disabled_at_zero() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);

        for hour in 10..14 {
            insert_test_event(
                &conn,
                "example.com",
                &format!("2024-01-15 {hour}:00:00"),
                "/",
            );
            storage.flush_events(&conn).unwrap();
        }
        let partition = storage.partition_dir("example.com", "2024-01-15");
        assert_eq!(ParquetStorage::partition_files(&partition).len(), 4);
    }

    #[test]
    fn test_compaction_below_threshold_is_a_noop() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 10);
        insert_test_event(&conn, "example.com", "2024-01-15 10:00:00", "/");
        storage.flush_events(&conn).unwrap();
        assert_eq!(
            storage
                .compact_partition(&conn, "example.com", "2024-01-15")
                .unwrap(),
            0
        );
    }

    #[test]
    fn test_cleanup_zero_retention_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);
        assert_eq!(storage.cleanup_old_partitions(0).unwrap(), 0);
    }

    #[test]
    fn test_cleanup_nonexistent_dir() {
        let storage = ParquetStorage::new(Path::new("/nonexistent/path/events"), 0);
        assert_eq!(storage.cleanup_old_partitions(30).unwrap(), 0);
    }

    #[test]
    fn test_cleanup_removes_old_partitions() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);

        let old_dir = storage.partition_dir("example.com", "2020-01-01");
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("0001.parquet"), b"fake").unwrap();

        let today = chrono::Utc::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        let new_dir = storage.partition_dir("example.com", &today);
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(new_dir.join("0001.parquet"), b"fake").unwrap();

        assert_eq!(storage.cleanup_old_partitions(30).unwrap(), 1);
        assert!(!old_dir.exists());
        assert!(new_dir.exists());
    }

    #[test]
    fn test_cleanup_across_multiple_sites() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);

        let old_a = storage.partition_dir("site-a.com", "2020-06-15");
        let old_b = storage.partition_dir("site-b.com", "2020-03-10");
        fs::create_dir_all(&old_a).unwrap();
        fs::create_dir_all(&old_b).unwrap();

        assert_eq!(storage.cleanup_old_partitions(30).unwrap(), 2);
        assert!(!old_a.exists());
        assert!(!old_b.exists());
    }

    #[test]
    fn test_erase_partitions_range() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);
        for day in 14..=17 {
            let p = storage.partition_dir("example.com", &format!("2024-01-{day}"));
            fs::create_dir_all(&p).unwrap();
        }
        let removed = storage
            .erase_partitions(
                "example.com",
                chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2024, 1, 16).unwrap(),
            )
            .unwrap();
        assert_eq!(removed, 2);
        assert!(storage.partition_dir("example.com", "2024-01-14").exists());
        assert!(!storage.partition_dir("example.com", "2024-01-15").exists());
        assert!(!storage.partition_dir("example.com", "2024-01-16").exists());
        assert!(storage.partition_dir("example.com", "2024-01-17").exists());
    }

    #[test]
    fn test_erase_partitions_rejects_unsafe_site_id() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);
        let today = chrono::Utc::now().date_naive();
        assert!(storage.erase_partitions("../etc", today, today).is_err());
    }

    #[test]
    fn test_known_site_ids() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);
        fs::create_dir_all(storage.partition_dir("b.com", "2024-01-15")).unwrap();
        fs::create_dir_all(storage.partition_dir("a.com", "2024-01-15")).unwrap();
        assert_eq!(storage.known_site_ids(), vec!["a.com", "b.com"]);
    }

    #[test]
    fn test_known_site_ids_empty_dir() {
        let storage = ParquetStorage::new(Path::new("/nonexistent/events"), 0);
        assert!(storage.known_site_ids().is_empty());
    }

    #[test]
    fn test_max_file_number_with_many_files() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);
        let partition = storage.partition_dir("example.com", "2024-01-15");
        fs::create_dir_all(&partition).unwrap();
        for n in 1u32..=100 {
            fs::write(partition.join(format!("{n:04}.parquet")), b"fake").unwrap();
        }
        assert_eq!(
            storage
                .next_file_path("example.com", "2024-01-15")
                .unwrap()
                .file_name()
                .unwrap(),
            "0101.parquet"
        );
    }

    #[test]
    fn test_next_file_path_ignores_non_numeric_files() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);
        let partition = storage.partition_dir("example.com", "2024-01-15");
        fs::create_dir_all(&partition).unwrap();
        fs::write(partition.join("0001.parquet"), b"fake").unwrap();
        fs::write(partition.join("0002.parquet"), b"fake").unwrap();
        fs::write(partition.join("temp.tmp"), b"fake").unwrap();
        fs::write(partition.join("README.txt"), b"fake").unwrap();
        fs::write(partition.join("0003.parquet.tmp"), b"fake").unwrap();

        assert_eq!(
            storage
                .next_file_path("example.com", "2024-01-15")
                .unwrap()
                .file_name()
                .unwrap(),
            "0003.parquet"
        );
    }

    #[test]
    fn test_is_safe_path_component_valid() {
        assert!(is_safe_path_component("example.com"));
        assert!(is_safe_path_component("my-site.org"));
        assert!(is_safe_path_component("site123"));
        assert!(is_safe_path_component("localhost:8080"));
    }

    #[test]
    fn test_is_safe_path_component_rejects_traversal() {
        assert!(!is_safe_path_component("../../../etc"));
        assert!(!is_safe_path_component("site/../secret"));
        assert!(!is_safe_path_component(".."));
        assert!(!is_safe_path_component("."));
    }

    #[test]
    fn test_is_safe_path_component_rejects_separators_and_control_chars() {
        assert!(!is_safe_path_component("site/subdir"));
        assert!(!is_safe_path_component("site\\subdir"));
        assert!(!is_safe_path_component("site\0id"));
        assert!(!is_safe_path_component("site\nid"));
    }

    #[test]
    fn test_is_safe_path_component_rejects_hidden_and_empty() {
        assert!(!is_safe_path_component(""));
        assert!(!is_safe_path_component(".hidden"));
        assert!(!is_safe_path_component(&"a".repeat(257)));
    }

    #[test]
    fn test_sql_quote_escapes_single_quotes() {
        assert_eq!(sql_quote("it's"), "it''s");
        assert_eq!(sql_quote("plain"), "plain");
    }

    #[test]
    fn test_flush_works_when_base_dir_contains_a_quote() {
        // The base directory comes from operator config and was previously
        // interpolated into COPY TO without escaping.
        let parent = tempfile::tempdir().unwrap();
        let odd = parent.path().join("data's");
        fs::create_dir_all(&odd).unwrap();

        let conn = setup_test_db();
        let storage = ParquetStorage::new(&odd, 0);
        insert_test_event(&conn, "example.com", "2024-01-15 10:00:00", "/");
        assert_eq!(storage.flush_events(&conn).unwrap(), 1);
        assert!(
            storage
                .partition_dir("example.com", "2024-01-15")
                .join("0001.parquet")
                .exists()
        );
    }

    #[test]
    fn test_flush_error_exposes_source() {
        let err = FlushError::Rename(std::io::Error::other("boom"));
        assert!(std::error::Error::source(&err).is_some());
        assert!(err.to_string().contains("Rename error"));
    }
}
