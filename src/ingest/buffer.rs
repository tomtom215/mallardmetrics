use crate::storage::parquet::ParquetStorage;
use chrono::NaiveDateTime;
use duckdb::Connection;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Represents a single analytics event ready for storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
pub struct Event {
    pub site_id: String,
    pub visitor_id: String,
    pub timestamp: NaiveDateTime,
    pub event_name: String,
    pub pathname: String,
    pub hostname: Option<String>,
    pub referrer: Option<String>,
    pub referrer_source: Option<String>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub utm_content: Option<String>,
    pub utm_term: Option<String>,
    pub browser: Option<String>,
    pub browser_version: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub device_type: Option<String>,
    pub screen_size: Option<String>,
    pub country_code: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub props: Option<String>,
    pub revenue_amount: Option<f64>,
    pub revenue_currency: Option<String>,
}

/// Thread-safe event buffer that accumulates events and flushes to Parquet
/// when the count threshold is reached.
pub struct EventBuffer {
    events: Mutex<Vec<Event>>,
    flush_threshold: usize,
    conn: Arc<Mutex<Connection>>,
    storage: ParquetStorage,
}

impl EventBuffer {
    pub fn new(
        flush_threshold: usize,
        conn: Arc<Mutex<Connection>>,
        storage: ParquetStorage,
    ) -> Self {
        Self {
            events: Mutex::new(Vec::with_capacity(flush_threshold)),
            flush_threshold,
            conn,
            storage,
        }
    }

    /// Returns a reference to the DuckDB connection for query access.
    pub const fn conn(&self) -> &Arc<Mutex<Connection>> {
        &self.conn
    }

    /// Add an event to the buffer. If the buffer reaches the threshold,
    /// automatically flushes to Parquet.
    pub fn push(&self, event: Event) -> Result<Option<usize>, BufferError> {
        let should_flush;
        {
            let mut events = self.events.lock();
            events.push(event);
            should_flush = events.len() >= self.flush_threshold;
        }

        if should_flush {
            let flushed = self.flush()?;
            Ok(Some(flushed))
        } else {
            Ok(None)
        }
    }

    /// Returns the current number of buffered events.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.events.lock().len()
    }

    /// Returns true if the buffer is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.events.lock().is_empty()
    }

    /// Flush all buffered events to Parquet via DuckDB.
    pub fn flush(&self) -> Result<usize, BufferError> {
        let events: Vec<Event> = {
            let mut buf = self.events.lock();
            std::mem::take(&mut *buf)
        };

        if events.is_empty() {
            return Ok(0);
        }

        let _count = events.len();
        let conn = self.conn.lock();

        // Insert events into DuckDB in-memory table
        for event in &events {
            conn.execute(
                "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname,
                 hostname, referrer, referrer_source, utm_source, utm_medium,
                 utm_campaign, utm_content, utm_term, browser, browser_version,
                 os, os_version, device_type, screen_size, country_code,
                 region, city, props, revenue_amount, revenue_currency)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                duckdb::params![
                    event.site_id,
                    event.visitor_id,
                    event.timestamp.format("%Y-%m-%d %H:%M:%S").to_string(),
                    event.event_name,
                    event.pathname,
                    event.hostname,
                    event.referrer,
                    event.referrer_source,
                    event.utm_source,
                    event.utm_medium,
                    event.utm_campaign,
                    event.utm_content,
                    event.utm_term,
                    event.browser,
                    event.browser_version,
                    event.os,
                    event.os_version,
                    event.device_type,
                    event.screen_size,
                    event.country_code,
                    event.region,
                    event.city,
                    event.props,
                    event.revenue_amount,
                    event.revenue_currency,
                ],
            )
            .map_err(BufferError::Insert)?;
        }

        // Flush from DuckDB to Parquet files
        let flushed = self
            .storage
            .flush_events(&conn)
            .map_err(BufferError::Flush)?;
        drop(conn);

        tracing::info!(count = flushed, "Flushed events to Parquet");
        Ok(flushed)
    }
}

#[derive(Debug)]
pub enum BufferError {
    Insert(duckdb::Error),
    Flush(crate::storage::parquet::FlushError),
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Insert(e) => write!(f, "Insert error: {e}"),
            Self::Flush(e) => write!(f, "Flush error: {e}"),
        }
    }
}

impl std::error::Error for BufferError {}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn make_test_event(site_id: &str, pathname: &str) -> Event {
        Event {
            site_id: site_id.to_string(),
            visitor_id: "test-visitor".to_string(),
            timestamp: NaiveDate::from_ymd_opt(2024, 1, 15)
                .unwrap()
                .and_hms_opt(10, 0, 0)
                .unwrap(),
            event_name: "pageview".to_string(),
            pathname: pathname.to_string(),
            hostname: None,
            referrer: None,
            referrer_source: None,
            utm_source: None,
            utm_medium: None,
            utm_campaign: None,
            utm_content: None,
            utm_term: None,
            browser: None,
            browser_version: None,
            os: None,
            os_version: None,
            device_type: None,
            screen_size: None,
            country_code: None,
            region: None,
            city: None,
            props: None,
            revenue_amount: None,
            revenue_currency: None,
        }
    }

    fn setup_buffer(threshold: usize) -> (EventBuffer, tempfile::TempDir) {
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path());
        let conn = Arc::new(Mutex::new(conn));
        let buffer = EventBuffer::new(threshold, conn, storage);
        (buffer, dir)
    }

    #[test]
    fn test_push_single_event() {
        let (buffer, _dir) = setup_buffer(100);
        let result = buffer.push(make_test_event("example.com", "/")).unwrap();
        assert!(result.is_none(), "Should not flush below threshold");
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn test_push_triggers_flush_at_threshold() {
        let (buffer, _dir) = setup_buffer(3);

        buffer.push(make_test_event("example.com", "/")).unwrap();
        buffer
            .push(make_test_event("example.com", "/about"))
            .unwrap();
        let result = buffer
            .push(make_test_event("example.com", "/contact"))
            .unwrap();

        assert!(result.is_some(), "Should flush at threshold");
        assert_eq!(result.unwrap(), 3);
        assert!(buffer.is_empty(), "Buffer should be empty after flush");
    }

    #[test]
    fn test_manual_flush() {
        let (buffer, _dir) = setup_buffer(100);

        buffer.push(make_test_event("example.com", "/")).unwrap();
        buffer
            .push(make_test_event("example.com", "/about"))
            .unwrap();

        let flushed = buffer.flush().unwrap();
        assert_eq!(flushed, 2);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_flush_empty_buffer() {
        let (buffer, _dir) = setup_buffer(100);
        let flushed = buffer.flush().unwrap();
        assert_eq!(flushed, 0);
    }

    #[test]
    fn test_buffer_len_and_is_empty() {
        let (buffer, _dir) = setup_buffer(100);
        assert!(buffer.is_empty());
        assert_eq!(buffer.len(), 0);

        buffer.push(make_test_event("example.com", "/")).unwrap();
        assert!(!buffer.is_empty());
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn test_multiple_sites_in_buffer() {
        let (buffer, dir) = setup_buffer(100);

        buffer.push(make_test_event("site-a.com", "/")).unwrap();
        buffer.push(make_test_event("site-b.com", "/")).unwrap();

        let flushed = buffer.flush().unwrap();
        assert_eq!(flushed, 2);

        // Verify both site directories exist
        let storage = ParquetStorage::new(dir.path());
        assert!(storage
            .partition_dir("site-a.com", "2024-01-15")
            .join("0001.parquet")
            .exists());
        assert!(storage
            .partition_dir("site-b.com", "2024-01-15")
            .join("0001.parquet")
            .exists());
    }
}
