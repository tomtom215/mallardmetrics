use crate::storage::parquet::ParquetStorage;
use chrono::NaiveDateTime;
use duckdb::Connection;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Upper bound on the flush rounds `shutdown_flush` will run. Each round
/// writes at most the storage layer's per-flush partition cap, so this only
/// matters if the hot table holds an absurd number of partitions or flushing
/// keeps failing; either way shutdown must still terminate.
const MAX_SHUTDOWN_ROUNDS: usize = 1024;

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
    /// Flushes that failed after the triggering event was already safe (in the
    /// retry buffer or the hot table). Exposed so `/metrics` can show a
    /// persistently failing flush without failing the requests that hit it.
    pub flush_failures: Arc<AtomicU64>,
    /// Set by [`shutdown_flush`](Self::shutdown_flush); later pushes are refused.
    closed: AtomicBool,
    /// Rows may remain in the hot table that a previous flush did not move to
    /// Parquet (a failure, or the per-flush partition cap). They are durable
    /// and queryable, but only a flush moves them, and `flush` returns early on
    /// an empty buffer unless this is set (SI-26).
    hot_dirty: AtomicBool,
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
            flush_failures: Arc::new(AtomicU64::new(0)),
            closed: AtomicBool::new(false),
            hot_dirty: AtomicBool::new(false),
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
    /// A flush that fails after this event was accepted does **not** fail the
    /// call. The event is either back in the retry buffer or already committed
    /// to the hot table, so reporting an error would make the one unlucky
    /// request that tipped the threshold answer 5xx for an event that is safe,
    /// and a client that retries would then duplicate it (F-12, SI-26). The
    /// failure is logged and counted in [`flush_failures`](Self::flush_failures).
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::AtCapacity`] when the buffer is full (which only
    /// happens if flushes have been failing) or when this event was discarded
    /// while restoring a failed flush, and [`BufferError::Closed`] after
    /// [`shutdown_flush`](Self::shutdown_flush).
    pub fn push(&self, event: Event) -> Result<Option<usize>, BufferError> {
        let should_flush = {
            let mut events = self.events.lock();
            // Checked under the lock `shutdown_flush` drains with, so an event
            // is either drained by it or refused here, never silently stranded.
            if self.closed.load(Ordering::Acquire) {
                return Err(BufferError::Closed);
            }
            if self.max_buffered > 0 && events.len() >= self.max_buffered {
                // Refusing the newest event (rather than growing without bound)
                // keeps memory flat when flushes are failing — e.g. a full disk.
                self.dropped_events.fetch_add(1, Ordering::Relaxed);
                return Err(BufferError::AtCapacity(self.max_buffered));
            }
            events.push(event);
            events.len() >= self.flush_threshold
        };

        if !should_flush {
            return Ok(None);
        }

        let dropped_before = self.dropped_events.load(Ordering::Relaxed);
        match self.flush() {
            Ok(n) => Ok(Some(n)),
            Err(e @ (BufferError::Flush(_) | BufferError::Insert(_))) => {
                self.flush_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!(error = %e, "Flush failed; the event stays buffered or in the hot table");
                if self.dropped_events.load(Ordering::Relaxed) > dropped_before {
                    // Restoring the failed batch overflowed the cap; this event
                    // may be among those discarded, so it is not safe to say Ok.
                    Err(BufferError::AtCapacity(self.max_buffered))
                } else {
                    Ok(None)
                }
            }
            Err(other) => Err(other),
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
    /// and the next flush writes them to Parquet even if no new event arrives.
    ///
    /// Compaction runs after the tier lock is released: it only reads immutable
    /// files and takes the lock just for the final swap (F-07).
    ///
    /// # Errors
    ///
    /// Returns an error if the DuckDB insert or the Parquet write fails.
    pub fn flush(&self) -> Result<usize, BufferError> {
        // Nothing buffered and nothing stranded in the hot table: do not queue
        // for the exclusive lock just to find that out.
        if self.events.lock().is_empty() && !self.hot_dirty.load(Ordering::Acquire) {
            return Ok(0);
        }

        let (flushed, touched) = self.flush_exclusive()?;
        self.compact(&touched);
        if flushed > 0 {
            tracing::debug!(count = flushed, "Flushed events to Parquet");
        }
        Ok(flushed)
    }

    /// Flush on the way out. Safe to call after the server has stopped
    /// accepting requests and drained the ones in flight (SI-03).
    ///
    /// Closes the buffer first, so a request that somehow still arrives is
    /// refused with [`BufferError::Closed`] instead of being buffered after the
    /// final drain and lost. Then flushes until neither the buffer nor the hot
    /// table holds anything, waiting out a periodic flush that is mid-flight
    /// (it holds the same locks). Idempotent.
    ///
    /// Returns the number of events written to Parquet by this call.
    ///
    /// # Errors
    ///
    /// Returns the first flush error. Events that could not reach Parquet are
    /// still committed to the hot table, which survives the restart.
    pub fn shutdown_flush(&self) -> Result<usize, BufferError> {
        self.closed.store(true, Ordering::Release);
        // A periodic flush may be mid-flight with a drained batch the buffer no
        // longer shows. It holds both locks, so waiting for them is how this
        // call learns that batch has landed (or failed and set `hot_dirty`).
        drop((self.tier_lock.write(), self.conn.lock()));
        let mut total = 0;
        for _ in 0..MAX_SHUTDOWN_ROUNDS {
            total += self.flush()?;
            if self.events.lock().is_empty() && !self.hot_dirty.load(Ordering::Acquire) {
                break;
            }
        }
        Ok(total)
    }

    /// The part of a flush that runs under the exclusive tier lock. Returns the
    /// number of events written and the partitions that received a file.
    fn flush_exclusive(&self) -> Result<(usize, Vec<(String, String)>), BufferError> {
        // Taken before the connection lock, and in the same order by every
        // reader, so the two locks cannot deadlock against each other. Held for
        // the whole insert-and-copy sequence: between the Parquet write and the
        // matching DELETE the same rows exist in both tiers, and a query that
        // ran in that window would count them twice.
        //
        // The buffer is drained only after both locks are held: draining first
        // left a batch outside the `max_buffered` accounting for as long as a
        // long dashboard query kept the flusher waiting (SI-26).
        let _tier = self.tier_lock.write();
        let conn = self.conn.lock();

        // Finish any flush a crash interrupted before inserting new rows: the
        // recovery rule relies on the hot table holding only pre-crash data.
        if let Err(e) = self.storage.ensure_recovered(&conn) {
            tracing::error!(error = %e, "Could not check for interrupted flushes");
        }

        let events: Vec<Event> = {
            let mut buf = self.events.lock();
            if buf.is_empty() && !self.hot_dirty.load(Ordering::Acquire) {
                return Ok((0, Vec::new()));
            }
            std::mem::take(&mut *buf)
        };

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
        let insert_result = if events.is_empty() {
            Ok(())
        } else {
            (|| -> Result<(), duckdb::Error> {
                conn.execute_batch("BEGIN TRANSACTION")?;
                let mut appender = conn.appender("events")?;
                for event in &events {
                    // The timestamp is appended as a typed chrono value rather
                    // than a formatted string. The old "%Y-%m-%d %H:%M:%S"
                    // round-trip silently truncated every event to whole
                    // seconds, which collapsed the ordering of events arriving
                    // within the same second — exactly the ordering that
                    // sessionize, window_funnel and sequence_match need.
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
            })()
        };

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

        // From here the events are durable in the hot table; anything that goes
        // wrong now leaves them there for the next flush.
        match self.storage.flush_events_deferred(&conn) {
            Ok(report) => {
                self.hot_dirty
                    .store(report.deferred_partitions > 0, Ordering::Release);
                Ok((report.flushed, report.touched))
            }
            Err(e) => {
                self.hot_dirty.store(true, Ordering::Release);
                Err(BufferError::Flush(e))
            }
        }
    }

    /// Compact the partitions a flush just wrote, outside the tier lock.
    fn compact(&self, touched: &[(String, String)]) {
        if touched.is_empty() {
            return;
        }
        // A separate connection: the merge only reads Parquet, and running it
        // on the writer connection would hold the writer mutex (and so every
        // flush) for the length of the merge. Never fall back to the writer
        // here: compaction takes the tier lock for its swap, and tier-then-
        // writer is the order every other path uses.
        let cloned = self.conn.lock().try_clone();
        let conn = match cloned {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "Could not open a connection for compaction; skipping");
                return;
            }
        };
        for (site_id, date) in touched {
            if let Err(e) = self
                .storage
                .compact_partition(&conn, &self.tier_lock, site_id, date)
            {
                // Compaction is an optimisation; the flush already succeeded.
                tracing::warn!(site_id, date, error = %e, "Parquet compaction failed");
            }
        }
    }
}

#[derive(Debug)]
pub enum BufferError {
    Insert(duckdb::Error),
    Flush(crate::storage::parquet::FlushError),
    /// The buffer is full because flushes are failing.
    AtCapacity(usize),
    /// The buffer was closed by `shutdown_flush`.
    Closed,
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
            Self::Closed => write!(f, "Event buffer is closed: the server is shutting down"),
        }
    }
}

impl std::error::Error for BufferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Insert(e) => Some(e),
            Self::Flush(e) => Some(e),
            Self::AtCapacity(_) | Self::Closed => None,
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

/// Failure-path, shutdown and tiering tests that need a real file-backed
/// database and separate reader connections (L28).
#[cfg(test)]
mod tiering_tests {
    use super::*;
    use crate::storage::{ReaderPool, TierLock};
    use chrono::NaiveDate;

    fn event(site: &str, minute: u32, path: &str) -> Event {
        Event {
            site_id: site.to_string(),
            visitor_id: "v".to_string(),
            timestamp: NaiveDate::from_ymd_opt(2024, 1, 15)
                .unwrap()
                .and_hms_opt(10, minute % 60, 0)
                .unwrap(),
            event_name: "pageview".to_string(),
            pathname: path.to_string(),
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

    struct Rig {
        _dir: tempfile::TempDir,
        events_dir: std::path::PathBuf,
        writer: Arc<Mutex<Connection>>,
        buffer: Arc<EventBuffer>,
        tier: TierLock,
    }

    fn rig(threshold: usize, cap: usize, compact_after: usize) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("t.duckdb")).unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        let events_dir = dir.path().join("events");
        std::fs::create_dir_all(&events_dir).unwrap();
        crate::storage::schema::setup_query_view(&conn, &events_dir).unwrap();
        let writer = Arc::new(Mutex::new(conn));
        let tier = TierLock::new();
        let storage = ParquetStorage::new(&events_dir, compact_after);
        let buffer = Arc::new(EventBuffer::new(
            threshold,
            cap,
            Arc::clone(&writer),
            storage,
            tier.clone(),
        ));
        Rig {
            _dir: dir,
            events_dir,
            writer,
            buffer,
            tier,
        }
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn test_a_failed_threshold_flush_does_not_fail_the_request() {
        // F-12 / SI-26: the push that tipped the threshold used to return Err
        // although its event was safely restored, so the client saw a 5xx and
        // retried a duplicate. Reproduced before the fix: `push` returned
        // Err(Insert(..)).
        let r = rig(2, 0, 0);
        r.writer.lock().execute_batch("DROP TABLE events").unwrap();
        assert!(r.buffer.push(event("a.com", 0, "/1")).unwrap().is_none());
        let tipped = r.buffer.push(event("a.com", 1, "/2"));
        assert!(matches!(tipped, Ok(None)), "{tipped:?}");
        assert_eq!(r.buffer.len(), 2, "both events are retained for the retry");
        assert_eq!(r.buffer.flush_failures.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_a_failed_parquet_stage_keeps_the_event_and_is_retried_without_new_traffic() {
        // The event commits to the hot table, the Parquet stage then fails. The
        // request must succeed, and the stranded rows must reach Parquet on a
        // later flush even though the buffer is empty by then (SI-26: `flush`
        // returned early on an empty buffer).
        let r = rig(2, 0, 0);
        // A plain file where the site directory must go makes the stage fail.
        std::fs::write(r.events_dir.join("site_id=a.com"), b"squatter").unwrap();

        r.buffer.push(event("a.com", 0, "/1")).unwrap();
        let tipped = r.buffer.push(event("a.com", 1, "/2"));
        assert!(matches!(tipped, Ok(None)), "{tipped:?}");
        assert_eq!(r.buffer.flush_failures.load(Ordering::Relaxed), 1);
        assert!(r.buffer.is_empty());
        assert_eq!(
            count(&r.writer.lock(), "events"),
            2,
            "durable in the hot table"
        );
        assert_eq!(
            count(&r.writer.lock(), "events_all"),
            2,
            "and still queryable"
        );

        // The buffer is empty; a periodic flush must still retry.
        assert!(
            r.buffer.flush().is_err(),
            "still blocked: the squatter remains"
        );
        std::fs::remove_file(r.events_dir.join("site_id=a.com")).unwrap();
        assert_eq!(r.buffer.flush().unwrap(), 2);
        assert_eq!(count(&r.writer.lock(), "events"), 0);
        assert_eq!(count(&r.writer.lock(), "events_all"), 2);
        // And once drained, an empty flush is a no-op again.
        assert_eq!(r.buffer.flush().unwrap(), 0);
    }

    #[test]
    fn test_shutdown_flush_writes_everything_and_refuses_later_events() {
        // SI-03 (buffer side).
        let r = rig(1000, 0, 0);
        for i in 0..5 {
            r.buffer.push(event("a.com", i, "/x")).unwrap();
        }
        assert_eq!(r.buffer.shutdown_flush().unwrap(), 5);
        assert_eq!(count(&r.writer.lock(), "events"), 0);
        assert_eq!(count(&r.writer.lock(), "events_all"), 5);

        assert!(matches!(
            r.buffer.push(event("a.com", 9, "/late")),
            Err(BufferError::Closed)
        ));
        // Safe to call again, and from the shutdown path after the drain.
        assert_eq!(r.buffer.shutdown_flush().unwrap(), 0);
    }

    #[test]
    fn test_shutdown_flush_waits_for_a_flush_that_is_mid_flight() {
        // A periodic flush holds the tier lock with a drained batch the buffer
        // no longer shows. shutdown_flush must not return "nothing to do" while
        // that batch has not landed.
        let r = rig(1000, 0, 0);
        r.buffer.push(event("a.com", 0, "/x")).unwrap();

        let in_flight = r.tier.write(); // stands in for the running flush
        let done = Arc::new(AtomicBool::new(false));
        let handle = {
            let (buffer, done) = (Arc::clone(&r.buffer), Arc::clone(&done));
            std::thread::spawn(move || {
                let n = buffer.shutdown_flush().unwrap();
                done.store(true, Ordering::SeqCst);
                n
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            !done.load(Ordering::SeqCst),
            "shutdown raced past the flush"
        );
        drop(in_flight);
        assert_eq!(handle.join().unwrap(), 1);
    }

    #[test]
    fn test_compaction_runs_outside_the_tier_lock_and_queries_stay_exact() {
        // End to end with real reader connections: pushes flush on every event
        // and compact every three files while a reader counts `events_all`.
        // A count that ever drops means a window with rows in neither tier; one
        // above what was pushed means rows in both.
        let r = rig(1, 0, 3);
        let readers = ReaderPool::new(&r.writer, 2);
        assert_eq!(
            readers.len(),
            2,
            "the bug needs independent read connections"
        );

        let pushed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let producer = {
            let (buffer, pushed, done) = (
                Arc::clone(&r.buffer),
                Arc::clone(&pushed),
                Arc::clone(&done),
            );
            std::thread::spawn(move || {
                for i in 0..40u32 {
                    pushed.fetch_add(1, Ordering::SeqCst);
                    buffer.push(event("a.com", i, "/x")).unwrap();
                }
                done.store(true, Ordering::SeqCst);
            })
        };

        let mut last = 0i64;
        let mut queries = 0u32;
        while !done.load(Ordering::SeqCst) {
            let n = {
                let _tier = r.tier.read();
                count(&readers.acquire().lock(), "events_all")
            };
            let ceiling = i64::try_from(pushed.load(Ordering::SeqCst)).unwrap();
            assert!(n <= ceiling, "saw {n} rows but only {ceiling} were pushed");
            assert!(
                n >= last,
                "the count went from {last} to {n}: rows vanished"
            );
            last = n;
            queries += 1;
        }
        producer.join().unwrap();
        assert!(queries > 0);
        assert_eq!(count(&readers.acquire().lock(), "events_all"), 40);

        let files: Vec<_> =
            std::fs::read_dir(r.events_dir.join("site_id=a.com").join("date=2024-01-15"))
                .unwrap()
                .flatten()
                .collect();
        assert!(
            files.len() < 40,
            "compaction never ran: {} files",
            files.len()
        );
        assert!(
            files
                .iter()
                .all(|f| !f.file_name().to_string_lossy().contains(".del")
                    && !f.file_name().to_string_lossy().ends_with(".tmp")),
            "no leftovers: {files:?}"
        );
    }
}
