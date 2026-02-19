use duckdb::Connection;
use std::fs;
use std::path::{Path, PathBuf};

/// Manages Parquet file storage with date-partitioned layout.
///
/// Storage layout:
/// ```text
/// data/events/site_id=example.com/date=2024-01-15/0001.parquet
/// ```
pub struct ParquetStorage {
    base_dir: PathBuf,
}

/// Validate that a site_id is safe for use in filesystem paths.
///
/// Rejects path traversal sequences (`..`, `/`, `\`) and control characters
/// that could be used to escape the partition directory.
fn is_safe_path_component(s: &str) -> bool {
    !s.is_empty()
        && !s.contains("..")
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains('\0')
        && s.len() <= 256
}

impl ParquetStorage {
    pub fn new(base_dir: &Path) -> Self {
        Self {
            base_dir: base_dir.to_path_buf(),
        }
    }

    /// Returns the partition directory for a given site and date.
    pub fn partition_dir(&self, site_id: &str, date: &str) -> PathBuf {
        self.base_dir
            .join(format!("site_id={site_id}"))
            .join(format!("date={date}"))
    }

    /// Generates the next available Parquet file path in the partition.
    fn next_file_path(&self, site_id: &str, date: &str) -> PathBuf {
        let dir = self.partition_dir(site_id, date);
        fs::create_dir_all(&dir).ok();

        let mut num = 1u32;
        loop {
            let path = dir.join(format!("{num:04}.parquet"));
            if !path.exists() {
                return path;
            }
            num += 1;
        }
    }

    /// Flush events from the in-memory DuckDB table to partitioned Parquet files.
    ///
    /// Groups events by (site_id, date) and writes each partition to its own file.
    pub fn flush_events(&self, conn: &Connection) -> Result<usize, FlushError> {
        // Get distinct partitions with counts
        let mut stmt = conn
            .prepare(
                "SELECT site_id, STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d') AS d, COUNT(*) AS cnt FROM events GROUP BY site_id, d ORDER BY site_id, d",
            )
            .map_err(FlushError::Query)?;

        let partitions: Vec<(String, String, usize)> = stmt
            .query_map([], |row| {
                let site_id: String = row.get(0)?;
                let date: String = row.get(1)?;
                let count: usize = row.get(2)?;
                Ok((site_id, date, count))
            })
            .map_err(FlushError::Query)?
            .filter_map(Result::ok)
            .collect();

        if partitions.is_empty() {
            return Ok(0);
        }

        let mut total_flushed = 0usize;

        for (site_id, date, count) in &partitions {
            // Validate site_id to prevent path traversal in filesystem operations
            if !is_safe_path_component(site_id) {
                tracing::warn!(site_id, "Skipping flush for invalid site_id");
                continue;
            }
            let file_path = self.next_file_path(site_id, date);
            let file_path_str = file_path.to_string_lossy();

            // Note: COPY TO does not support parameterized queries in DuckDB.
            // site_id and date are internal values from the events table, not user input.
            let escaped_site = site_id.replace('\'', "''");

            let copy_sql = format!(
                "COPY (SELECT * FROM events WHERE site_id = '{escaped_site}' AND STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d') = '{date}') TO '{file_path_str}' (FORMAT PARQUET, COMPRESSION ZSTD)"
            );

            conn.execute_batch(&copy_sql).map_err(FlushError::Write)?;

            total_flushed += count;

            // Delete flushed events from the in-memory events table.
            // The events_all view unions this table with the Parquet files,
            // so deleted events remain visible to queries via the cold tier.
            conn.execute_batch(&format!(
                "DELETE FROM events WHERE site_id = '{escaped_site}' AND STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d') = '{date}'"
            ))
            .map_err(FlushError::Delete)?;
        }

        // Refresh the query view so newly written Parquet files are included.
        // Non-fatal: hot events remain visible through the events table.
        let _ = crate::storage::schema::setup_query_view(conn, &self.base_dir);

        Ok(total_flushed)
    }

    /// Delete Parquet partition directories older than the given number of days.
    ///
    /// Returns the number of partition directories removed.
    pub fn cleanup_old_partitions(&self, retention_days: u32) -> std::io::Result<usize> {
        if retention_days == 0 {
            return Ok(0); // Unlimited retention
        }

        let cutoff =
            chrono::Utc::now().date_naive() - chrono::Duration::days(i64::from(retention_days));
        let cutoff_str = cutoff.format("%Y-%m-%d").to_string();
        let mut removed = 0usize;

        // Iterate site_id=* directories
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
            // Iterate date=* directories inside each site
            for date_entry in fs::read_dir(&site_path)?.flatten() {
                let date_path = date_entry.path();
                if !date_path.is_dir() {
                    continue;
                }
                let dir_name = date_entry.file_name();
                let dir_name = dir_name.to_string_lossy();
                if let Some(date_str) = dir_name.strip_prefix("date=") {
                    if date_str < cutoff_str.as_str() {
                        fs::remove_dir_all(&date_path)?;
                        removed += 1;
                    }
                }
            }
        }

        Ok(removed)
    }
}

#[derive(Debug)]
pub enum FlushError {
    Query(duckdb::Error),
    Write(duckdb::Error),
    Delete(duckdb::Error),
}

impl std::fmt::Display for FlushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Query(e) => write!(f, "Query error: {e}"),
            Self::Write(e) => write!(f, "Write error: {e}"),
            Self::Delete(e) => write!(f, "Delete error: {e}"),
        }
    }
}

impl std::error::Error for FlushError {}

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
             VALUES (?, ?, ?, 'pageview', ?)",
            duckdb::params![site_id, "visitor1", timestamp, pathname],
        )
        .unwrap();
    }

    #[test]
    fn test_partition_dir() {
        let storage = ParquetStorage::new(Path::new("/data/events"));
        let dir = storage.partition_dir("example.com", "2024-01-15");
        assert_eq!(
            dir,
            PathBuf::from("/data/events/site_id=example.com/date=2024-01-15")
        );
    }

    #[test]
    fn test_flush_empty_table() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path());

        let count = storage.flush_events(&conn).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_flush_and_verify() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path());

        insert_test_event(&conn, "example.com", "2024-01-15 10:00:00", "/");
        insert_test_event(&conn, "example.com", "2024-01-15 11:00:00", "/about");

        let count = storage.flush_events(&conn).unwrap();
        assert_eq!(count, 2);

        // Verify events removed from in-memory table
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM events").unwrap();
        let remaining: i64 = stmt.query_row([], |row| row.get(0)).unwrap();
        assert_eq!(remaining, 0);

        // Verify Parquet file exists
        let parquet_dir = storage.partition_dir("example.com", "2024-01-15");
        assert!(parquet_dir.join("0001.parquet").exists());
    }

    #[test]
    fn test_flush_multiple_sites() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path());

        insert_test_event(&conn, "site-a.com", "2024-01-15 10:00:00", "/");
        insert_test_event(&conn, "site-b.com", "2024-01-15 10:00:00", "/");

        let count = storage.flush_events(&conn).unwrap();
        assert_eq!(count, 2);

        assert!(storage
            .partition_dir("site-a.com", "2024-01-15")
            .join("0001.parquet")
            .exists());
        assert!(storage
            .partition_dir("site-b.com", "2024-01-15")
            .join("0001.parquet")
            .exists());
    }

    #[test]
    fn test_flush_multiple_dates() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path());

        insert_test_event(&conn, "example.com", "2024-01-15 10:00:00", "/");
        insert_test_event(&conn, "example.com", "2024-01-16 10:00:00", "/");

        let count = storage.flush_events(&conn).unwrap();
        assert_eq!(count, 2);

        assert!(storage
            .partition_dir("example.com", "2024-01-15")
            .join("0001.parquet")
            .exists());
        assert!(storage
            .partition_dir("example.com", "2024-01-16")
            .join("0001.parquet")
            .exists());
    }

    #[test]
    fn test_incremental_file_numbering() {
        let conn = setup_test_db();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path());

        // First flush
        insert_test_event(&conn, "example.com", "2024-01-15 10:00:00", "/");
        storage.flush_events(&conn).unwrap();

        // Second flush to same partition
        insert_test_event(&conn, "example.com", "2024-01-15 11:00:00", "/about");
        storage.flush_events(&conn).unwrap();

        let partition = storage.partition_dir("example.com", "2024-01-15");
        assert!(partition.join("0001.parquet").exists());
        assert!(partition.join("0002.parquet").exists());
    }

    #[test]
    fn test_cleanup_zero_retention_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path());
        let removed = storage.cleanup_old_partitions(0).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_cleanup_nonexistent_dir() {
        let storage = ParquetStorage::new(Path::new("/nonexistent/path/events"));
        let removed = storage.cleanup_old_partitions(30).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_cleanup_removes_old_partitions() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path());

        // Create old partition (far in the past)
        let old_dir = storage.partition_dir("example.com", "2020-01-01");
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("0001.parquet"), b"fake").unwrap();

        // Create recent partition (today)
        let today = chrono::Utc::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        let new_dir = storage.partition_dir("example.com", &today);
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(new_dir.join("0001.parquet"), b"fake").unwrap();

        let removed = storage.cleanup_old_partitions(30).unwrap();
        assert_eq!(removed, 1);

        // Old partition should be gone
        assert!(!old_dir.exists());
        // Recent partition should remain
        assert!(new_dir.exists());
    }

    #[test]
    fn test_cleanup_across_multiple_sites() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path());

        // Create old partitions for two sites
        let old_a = storage.partition_dir("site-a.com", "2020-06-15");
        let old_b = storage.partition_dir("site-b.com", "2020-03-10");
        fs::create_dir_all(&old_a).unwrap();
        fs::create_dir_all(&old_b).unwrap();
        fs::write(old_a.join("0001.parquet"), b"fake").unwrap();
        fs::write(old_b.join("0001.parquet"), b"fake").unwrap();

        let removed = storage.cleanup_old_partitions(30).unwrap();
        assert_eq!(removed, 2);
        assert!(!old_a.exists());
        assert!(!old_b.exists());
    }

    #[test]
    fn test_is_safe_path_component_valid() {
        assert!(is_safe_path_component("example.com"));
        assert!(is_safe_path_component("my-site.org"));
        assert!(is_safe_path_component("site123"));
    }

    #[test]
    fn test_is_safe_path_component_rejects_traversal() {
        assert!(!is_safe_path_component("../../../etc"));
        assert!(!is_safe_path_component("site/../secret"));
        assert!(!is_safe_path_component("site/../../passwd"));
    }

    #[test]
    fn test_is_safe_path_component_rejects_slashes() {
        assert!(!is_safe_path_component("site/subdir"));
        assert!(!is_safe_path_component("site\\subdir"));
    }

    #[test]
    fn test_is_safe_path_component_rejects_empty() {
        assert!(!is_safe_path_component(""));
    }

    #[test]
    fn test_is_safe_path_component_rejects_null() {
        assert!(!is_safe_path_component("site\0id"));
    }
}
