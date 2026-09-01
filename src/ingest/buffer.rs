use crate::storage::parquet::ParquetStorage;
use chrono::NaiveDateTime;
use duckdb::Connection;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A single analytics event ready for storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
// UTM fields intentionally share the `utm_` prefix: that is the standardised
// naming for UTM parameters, and renaming would break the Parquet schema.
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

/// Thread-safe event buffer that accumulates events and flushes them to
/// DuckDB (and onward to Parquet) when a threshold is reached.
pub struct EventBuffer {
    events: Mutex<Vec<Event>>,
    flush_threshold: usize,
    /// Hard cap on buffered events. 0 = unlimited.
    max_buffered: usize,
    conn: Arc<Mutex<Connection>>,
    storage: ParquetStorage,
    /// Held exclusively while the hot and cold tiers disagree. See
    /// [`TierLock`](crate::storage::TierLock).
    tier_lock: crate::storage::TierLock,
    /// Events discarded because the buffer was at capacity.
    pub dropped_events: Arc<AtomicU64>,
}

impl EventBuffer {
    pub fn new(
        flush_threshold: usize,
        max_buffered: usize,
        conn: Arc<Mutex<Connection>>,
        storage: ParquetStorage,
        tier_lock: crate::storage::TierLock,
    ) -> Self {
        Self {
            events: Mutex::new(Vec::with_capacity(flush_threshold.min(4096))),
            flush_threshold,
            max_buffered,
            conn,
            storage,
            tier_lock,
            dropped_events: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The tier lock this buffer flushes under, so callers that make the tiers
    /// inconsistent by other means (erasure) can take the same one.
    pub const fn tier_lock(&self) -> &crate::storage::TierLock {
        &self.tier_lock
    }

    /// Returns the DuckDB writer connection.
    pub const fn conn(&self) -> &Arc<Mutex<Connection>> {
        &self.conn
    }

    /// Returns the Parquet storage handle.
    pub const fn storage(&self) -> &ParquetStorage {
        &self.storage
    }

    /// Add an event to the buffer, flushing if the threshold is reached.
    ///
    /// Returns `Ok(Some(n))` when a flush wrote `n` events, `Ok(None)` when the
    /// event was merely buffered.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::AtCapacity`] when the buffer is full — which only
    /// happens if flushes have been failing — and propagates flush errors.
    pub fn push(&self, event: Event) -> Result<Option<usize>, BufferError> {
        let should_flush = {
            let mut events = self.events.lock();
            if self.max_buffered > 0 && events.len() >= self.max_buffered {
                // Refusing the newest event (rather than growing without bound)
                // keeps memory flat when flushes are failing — e.g. a full disk.
                self.dropped_events.fetch_add(1, Ordering::Relaxed);
                return Err(BufferError::AtCapacity(self.max_buffered));
            }
            events.push(event);
            events.len() >= self.flush_threshold
        };

        if should_flush {
            Ok(Some(self.flush()?))
        } else {
            Ok(None)
        }
    }

    /// Current number of buffered events.
    pub fn len(&self) -> usize {
        self.events.lock().len()
    }

    /// True when nothing is buffered.
    pub fn is_empty(&self) -> bool {
        self.events.lock().is_empty()
    }

    /// Put drained events back at the front of the buffer after a failed flush.
    ///
    /// Anything beyond `max_buffered` is discarded and counted, so a persistent
    /// flush failure cannot grow the buffer without bound.
    fn restore(&self, events: Vec<Event>) {
        let mut buf = self.events.lock();
        let mut restored = events;
        restored.append(&mut buf);
        if self.max_buffered > 0 && restored.len() > self.max_buffered {
            let overflow = restored.len() - self.max_buffered;
            // Keep the oldest events: they are the ones closest to being durable,
            // and dropping from the tail keeps the buffer chronologically dense.
            restored.truncate(self.max_buffered);
            self.dropped_events
                .fetch_add(overflow as u64, Ordering::Relaxed);
            tracing::warn!(
                overflow,
                max_buffered = self.max_buffered,
                "Event buffer at capacity after a failed flush; dropped newest events"
            );
        }
        *buf = restored;
    }

    /// Flush all buffered events to DuckDB and onward to Parquet.
    ///
    /// # Atomicity
    ///
    /// Events are drained from the buffer before any insert begins, so a
    /// concurrent flush can never process them twice. If the insert fails they
    /// are restored for the next attempt.
    ///
    /// If the inserts succeed but the Parquet write fails, the events are
    /// already durable in the DuckDB table (and visible through `events_all`),
    /// and will be written to Parquet by the next flush.
    ///
    /// # Errors
    ///
    /// Returns an error if the DuckDB insert or the Parquet write fails.
    pub fn flush(&self) -> Result<usize, BufferError> {
        let events: Vec<Event> = {
            let mut buf = self.events.lock();
            if buf.is_empty() {
                return Ok(0);
            }
            std::mem::take(&mut *buf)
        };

        // Taken before the connection lock, and in the same order by every
        // reader, so the two locks cannot deadlock against each other. Held for
        // the whole insert-and-copy sequence: between the Parquet write and the
        // matching DELETE the same rows exist in both tiers, and a query that
        // ran in that window would count them twice.
        let _tier = self.tier_lock.write();
        let conn = self.conn.lock();

        // Bulk-insert with DuckDB's Appender, which bypasses per-row SQL parsing.
        //
        // The appender borrows `conn`, so the insert runs inside a closure whose
        // return ends that borrow — otherwise the connection cannot be released
        // before restoring the buffer on failure.
        //
        // The whole batch is one explicit transaction. The appender commits
        // whenever an internal chunk fills, so without it a failure part-way
        // through would leave the earlier chunks durably inserted while
        // `restore` puts every event back for the next attempt — silently
        // duplicating everything up to the failure point.
        let insert_result = (|| -> Result<(), duckdb::Error> {
            conn.execute_batch("BEGIN TRANSACTION")?;
            let mut appender = conn.appender("events")?;
            for event in &events {
                // The timestamp is appended as a typed chrono value rather than a
                // formatted string. The old "%Y-%m-%d %H:%M:%S" round-trip silently
                // truncated every event to whole seconds, which collapsed the
                // ordering of events arriving within the same second — exactly the
                // ordering that sessionize, window_funnel and sequence_match need.
                appender.append_row(duckdb::params![
                    event.site_id,
                    event.visitor_id,
                    event.timestamp,
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
                ])?;
            }
            appender.flush()?;
            drop(appender);
            conn.execute_batch("COMMIT")
        })();

        if let Err(e) = insert_result {
            // Undo whatever the transaction had staged so the restored events
            // are the only copy of the batch.
            if let Err(rollback) = conn.execute_batch("ROLLBACK") {
                tracing::error!(error = %rollback, "Could not roll back a failed event insert");
            }
            drop(conn);
            self.restore(events);
            return Err(BufferError::Insert(e));
        }

        let flushed = self
            .storage
            .flush_events(&conn)
            .map_err(BufferError::Flush)?;
        drop(conn);

        if flushed > 0 {
            tracing::debug!(count = flushed, "Flushed events to Parquet");
        }
        Ok(flushed)
    }
}

#[derive(Debug)]
pub enum BufferError {
    Insert(duckdb::Error),
    Flush(crate::storage::parquet::FlushError),
    /// The buffer is full because flushes are failing.
    AtCapacity(usize),
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Insert(e) => write!(f, "Insert error: {e}"),
            Self::Flush(e) => write!(f, "Flush error: {e}"),
            Self::AtCapacity(cap) => write!(
                f,
                "Event buffer is at capacity ({cap} events); flushes are not draining"
            ),
        }
    }
}

impl std::error::Error for BufferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Insert(e) => Some(e),
            Self::Flush(e) => Some(e),
            Self::AtCapacity(_) => None,
        }
    }
}

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
        setup_buffer_capped(threshold, 0)
    }

    fn setup_buffer_capped(threshold: usize, cap: usize) -> (EventBuffer, tempfile::TempDir) {
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let storage = ParquetStorage::new(dir.path(), 0);
        let conn = Arc::new(Mutex::new(conn));
        (
            EventBuffer::new(
                threshold,
                cap,
                conn,
                storage,
                crate::storage::TierLock::new(),
            ),
            dir,
        )
    }

    #[test]
    fn test_push_single_event() {
        let (buffer, _dir) = setup_buffer(100);
        let result = buffer.push(make_test_event("example.com", "/")).unwrap();
        assert!(result.is_none(), "should not flush below threshold");
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

        assert_eq!(result, Some(3));
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_manual_flush() {
        let (buffer, _dir) = setup_buffer(100);
        buffer.push(make_test_event("example.com", "/")).unwrap();
        buffer
            .push(make_test_event("example.com", "/about"))
            .unwrap();
        assert_eq!(buffer.flush().unwrap(), 2);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_flush_empty_buffer() {
        let (buffer, _dir) = setup_buffer(100);
        assert_eq!(buffer.flush().unwrap(), 0);
    }

    #[test]
    fn test_buffer_len_and_is_empty() {
        let (buffer, _dir) = setup_buffer(100);
        assert!(buffer.is_empty());
        buffer.push(make_test_event("example.com", "/")).unwrap();
        assert!(!buffer.is_empty());
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn test_flush_failure_preserves_events() {
        let (buffer, _dir) = setup_buffer(100);
        buffer.push(make_test_event("example.com", "/")).unwrap();
        buffer
            .push(make_test_event("example.com", "/about"))
            .unwrap();

        {
            let conn = buffer.conn().lock();
            conn.execute_batch("DROP TABLE events").unwrap();
        }

        assert!(buffer.flush().is_err());
        assert_eq!(buffer.len(), 2, "events must survive a failed flush");
    }

    #[test]
    fn test_flush_failure_restores_all_events() {
        let (buffer, _dir) = setup_buffer(100);
        for i in 0..5 {
            buffer
                .push(make_test_event("example.com", &format!("/page-{i}")))
                .unwrap();
        }
        {
            let conn = buffer.conn().lock();
            conn.execute_batch("DROP TABLE events").unwrap();
        }
        let _ = buffer.flush();
        assert_eq!(buffer.len(), 5);
    }

    #[test]
    fn test_buffer_refuses_events_at_capacity() {
        // Without a cap, a persistently failing flush grows the retry buffer
        // until the process is OOM-killed.
        let (buffer, _dir) = setup_buffer_capped(1000, 3);
        for _ in 0..3 {
            buffer.push(make_test_event("example.com", "/")).unwrap();
        }
        let err = buffer
            .push(make_test_event("example.com", "/"))
            .unwrap_err();
        assert!(matches!(err, BufferError::AtCapacity(3)));
        assert_eq!(buffer.len(), 3, "the buffer must not grow past its cap");
        assert_eq!(buffer.dropped_events.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_restore_after_failure_respects_capacity() {
        let (buffer, _dir) = setup_buffer_capped(2, 4);
        // Break the table so every flush fails.
        {
            let conn = buffer.conn().lock();
            conn.execute_batch("DROP TABLE events").unwrap();
        }
        // Each pair triggers a failing flush whose events are restored.
        for _ in 0..10 {
            let _ = buffer.push(make_test_event("example.com", "/"));
        }
        assert!(
            buffer.len() <= 4,
            "buffer grew to {} despite a cap of 4",
            buffer.len()
        );
        assert!(buffer.dropped_events.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn test_unbounded_buffer_when_cap_is_zero() {
        let (buffer, _dir) = setup_buffer_capped(1000, 0);
        for _ in 0..50 {
            buffer.push(make_test_event("example.com", "/")).unwrap();
        }
        assert_eq!(buffer.len(), 50);
    }

    #[test]
    fn test_sub_second_timestamps_are_preserved() {
        // Regression: timestamps used to be formatted as "%Y-%m-%d %H:%M:%S",
        // truncating to whole seconds and destroying event ordering within a
        // second — which sessionize, window_funnel and sequence_match depend on.
        let (buffer, _dir) = setup_buffer(100);
        let base = NaiveDate::from_ymd_opt(2024, 1, 15)
            .unwrap()
            .and_hms_micro_opt(10, 0, 0, 250_000)
            .unwrap();
        let mut event = make_test_event("example.com", "/");
        event.timestamp = base;
        buffer.push(event).unwrap();
        // `push` only buffers below the flush threshold. `flush` runs the
        // appender — where the truncation used to happen — and then moves the
        // rows on to Parquet, so the assertion covers the whole write path and
        // has to read through the union view rather than the hot table.
        assert_eq!(buffer.flush().unwrap(), 1);

        let stored: String = {
            let conn = buffer.conn().lock();
            conn.query_row(
                "SELECT STRFTIME(timestamp, '%Y-%m-%d %H:%M:%S.%f') FROM events_all",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(
            stored.starts_with("2024-01-15 10:00:00.250"),
            "sub-second precision was lost: {stored}"
        );
    }

    #[test]
    fn test_event_ordering_within_one_second_is_preserved() {
        let (buffer, _dir) = setup_buffer(100);
        for (i, micros) in [100_000u32, 400_000, 900_000].iter().enumerate() {
            let mut event = make_test_event("example.com", &format!("/p{i}"));
            event.timestamp = NaiveDate::from_ymd_opt(2024, 1, 15)
                .unwrap()
                .and_hms_micro_opt(10, 0, 0, *micros)
                .unwrap();
            buffer.push(event).unwrap();
        }
        assert_eq!(buffer.flush().unwrap(), 3);

        let conn = buffer.conn().lock();
        let mut stmt = conn
            .prepare("SELECT pathname FROM events_all ORDER BY timestamp")
            .unwrap();
        let paths: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(paths, vec!["/p0", "/p1", "/p2"]);
    }

    #[test]
    fn test_multiple_sites_in_buffer() {
        let (buffer, dir) = setup_buffer(100);
        buffer.push(make_test_event("site-a.com", "/")).unwrap();
        buffer.push(make_test_event("site-b.com", "/")).unwrap();
        assert_eq!(buffer.flush().unwrap(), 2);

        let storage = ParquetStorage::new(dir.path(), 0);
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
    fn test_buffer_error_exposes_source() {
        let err = BufferError::AtCapacity(10);
        assert!(std::error::Error::source(&err).is_none());
        assert!(err.to_string().contains("capacity"));
    }
}
