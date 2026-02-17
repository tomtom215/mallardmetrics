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

            // Delete flushed events from the in-memory table
            conn.execute_batch(&format!(
                "DELETE FROM events WHERE site_id = '{escaped_site}' AND STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d') = '{date}'"
            ))
            .map_err(FlushError::Delete)?;
        }

        Ok(total_flushed)
    }

    /// Load events from Parquet files back into DuckDB for querying.
    #[allow(dead_code)]
    pub fn load_events(
        &self,
        conn: &Connection,
        site_id: &str,
        start_date: &str,
        end_date: &str,
    ) -> Result<(), FlushError> {
        let site_dir = self.base_dir.join(format!("site_id={site_id}"));
        if !site_dir.exists() {
            return Ok(());
        }

        // Use DuckDB's glob to read all matching Parquet files
        let pattern = format!("{}/date=*/**.parquet", site_dir.to_string_lossy());

        // Create a view from parquet files, filtering by date range
        let sql = format!(
            "INSERT INTO events SELECT * FROM read_parquet('{pattern}') WHERE CAST(timestamp AS DATE) >= CAST('{start_date}' AS DATE) AND CAST(timestamp AS DATE) < CAST('{end_date}' AS DATE)"
        );

        conn.execute_batch(&sql).map_err(FlushError::Query)?;
        Ok(())
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
}
