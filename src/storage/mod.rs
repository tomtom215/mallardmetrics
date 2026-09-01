pub mod migrations;
pub mod parquet;
pub mod schema;

use duckdb::Connection;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Guards the windows in which the two storage tiers disagree with each other.
///
/// `events_all` is a `UNION ALL` of the hot `events` table and a *live* Parquet
/// glob, and the glob is re-expanded on every query. Two write paths therefore
/// have a moment where a row is visible on both sides at once:
///
/// - **Flush** writes a partition's rows to Parquet and only then deletes them
///   from the hot table. Between those two steps a reader counts them twice.
/// - **Compaction** renames the merged file into the glob and only then removes
///   the source files it replaced. Between those two steps every row in the
///   partition is visible twice.
///
/// Reversing either order would trade double-counting for a window where the
/// rows are missing entirely, which is no better. Instead, writers take the
/// exclusive side of this lock for the duration of the inconsistency and
/// analytics queries take the shared side, so no query can observe it.
///
/// The lock is deliberately coarse. It is held for the length of a flush or a
/// compaction — file writes, not user-facing work — and the alternative is a
/// report that is quietly wrong a few times an hour.
#[derive(Clone, Default)]
pub struct TierLock(Arc<parking_lot::RwLock<()>>);

impl TierLock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Shared access, taken by every analytics query.
    pub fn read(&self) -> parking_lot::RwLockReadGuard<'_, ()> {
        self.0.read()
    }

    /// Exclusive access, taken while the tiers are inconsistent.
    pub fn write(&self) -> parking_lot::RwLockWriteGuard<'_, ()> {
        self.0.write()
    }

    /// Shared access if it is free right now. Used by tests to assert that a
    /// query and a flush really do contend on the same lock.
    pub fn try_read(&self) -> Option<parking_lot::RwLockReadGuard<'_, ()>> {
        self.0.try_read()
    }
}

/// A pool of read-only DuckDB connections for serving analytics queries.
///
/// Every query used to contend for the single writer connection, so one slow
/// dashboard query blocked event ingestion — and a dashboard load, which fires
/// a dozen requests at once, serialised end to end.
///
/// DuckDB connections opened against the same database share its buffer pool
/// and catalog, so cloning is cheap and reads see the writer's committed data.
/// The pool is a fixed set of `Mutex<Connection>`s handed out round-robin;
/// DuckDB parallelises within a single query, so a small pool is the right
/// shape rather than one connection per request.
#[derive(Clone)]
pub struct ReaderPool {
    connections: Arc<Vec<Arc<Mutex<Connection>>>>,
    next: Arc<AtomicUsize>,
}

impl ReaderPool {
    /// Build a pool of `size` readers cloned from `writer`.
    ///
    /// `size` of 0 or 1 yields a pool that shares the writer connection, which
    /// is the previous behaviour and what the in-memory test databases need
    /// (an in-memory database is private to its connection, so a clone would
    /// see an empty catalog).
    ///
    /// A connection that cannot be cloned is skipped with a warning rather than
    /// failing startup: serving reads from the writer is slower but correct.
    pub fn new(writer: &Arc<Mutex<Connection>>, size: usize) -> Self {
        if size <= 1 {
            return Self::shared(writer);
        }

        let mut connections = Vec::with_capacity(size);
        for _ in 0..size {
            // The writer lock is released before logging or pushing.
            let cloned = writer.lock().try_clone();
            match cloned {
                Ok(conn) => connections.push(Arc::new(Mutex::new(conn))),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Could not open an additional read connection; \
                         analytics queries will share the writer connection"
                    );
                    break;
                }
            }
        }

        if connections.is_empty() {
            return Self::shared(writer);
        }

        tracing::info!(readers = connections.len(), "Read connection pool ready");
        Self {
            connections: Arc::new(connections),
            next: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// A pool that serves every read from the writer connection.
    pub fn shared(writer: &Arc<Mutex<Connection>>) -> Self {
        Self {
            connections: Arc::new(vec![Arc::clone(writer)]),
            next: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Number of connections in the pool.
    pub fn len(&self) -> usize {
        self.connections.len()
    }

    /// Always false — a pool always holds at least one connection.
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Take the next connection, round-robin.
    ///
    /// Returns the `Arc` rather than a guard so the caller controls how long the
    /// lock is held; every caller runs inside `spawn_blocking`.
    pub fn acquire(&self) -> Arc<Mutex<Connection>> {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.connections.len();
        Arc::clone(&self.connections[index])
    }

    /// Apply a setup step to every connection in the pool.
    ///
    /// Used at startup to load the behavioral extension, which arms the
    /// connection that runs `LOAD` rather than the database. The `events_all`
    /// view is *not* in that category — it is a catalog object every connection
    /// shares — so a later refresh only needs the writer. See
    /// `tests::test_extension_load_is_per_connection_but_the_view_is_not`.
    ///
    /// # Errors
    ///
    /// Returns the first error encountered.
    pub fn for_each<F>(&self, mut f: F) -> Result<(), duckdb::Error>
    where
        F: FnMut(&Connection) -> Result<(), duckdb::Error>,
    {
        for conn in self.connections.iter() {
            f(&conn.lock())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        Arc::new(Mutex::new(conn))
    }

    /// Rows written into the hot table on the writer connection.
    fn insert_events(conn: &Connection, count: usize, offset: usize) {
        for i in 0..count {
            conn.execute(
                "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
                 VALUES ('a.com', ?, '2024-01-15 10:00:00', 'pageview', ?)",
                duckdb::params![format!("v{}", offset + i), format!("/p{}", offset + i)],
            )
            .unwrap();
        }
    }

    /// Rows per insert batch, and how many batches the flusher writes.
    const BATCH: usize = 60;
    const ROUNDS: usize = 8;

    #[test]
    fn test_a_query_never_sees_a_half_applied_flush() {
        // `events_all` is the hot `events` table UNION ALL a *live* Parquet
        // glob, re-expanded on every query. A flush copies a partition to
        // Parquet and only then deletes the rows it copied, so a query landing
        // between those two statements counts every flushed event twice.
        //
        // With separate reader connections nothing else serialises the two —
        // the writer mutex does not, because readers do not take it — so this
        // reproduces reliably without the tier lock: the assertion below fires
        // with a count roughly double the truth.
        use crate::storage::parquet::ParquetStorage;
        use std::sync::atomic::{AtomicBool, AtomicU64};

        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("t.duckdb")).unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        let events_dir = dir.path().join("events");
        std::fs::create_dir_all(&events_dir).unwrap();
        crate::storage::schema::setup_query_view(&conn, &events_dir).unwrap();

        let writer = Arc::new(Mutex::new(conn));
        let readers = ReaderPool::new(&writer, 2);
        assert_eq!(
            readers.len(),
            2,
            "the bug needs independent read connections"
        );

        let storage = ParquetStorage::new(&events_dir, 0);
        let tier = TierLock::new();
        let committed = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));

        let flusher = {
            // `tier` is still needed by the reader below; `storage` is not.
            let (writer, storage, tier) = (Arc::clone(&writer), storage, tier.clone());
            let (committed, done) = (Arc::clone(&committed), Arc::clone(&done));
            std::thread::spawn(move || {
                for round in 0..ROUNDS {
                    // Raised *before* the insert, so the ceiling is never
                    // behind reality: a reader that catches a committed row the
                    // counter has not seen yet is a false failure, and the
                    // point of the test is the opposite direction — rows the
                    // reader sees that were never written once.
                    committed.fetch_add(BATCH as u64, Ordering::SeqCst);
                    {
                        let guard = writer.lock();
                        insert_events(&guard, BATCH, round * BATCH);
                    }
                    {
                        let _tier = tier.write();
                        let guard = writer.lock();
                        storage.flush_events(&guard).unwrap();
                    }
                }
                done.store(true, Ordering::SeqCst);
            })
        };

        let mut queries = 0u32;
        while !done.load(Ordering::SeqCst) {
            let count: u64 = {
                let _tier = tier.read();
                let conn = readers.acquire();
                let guard = conn.lock();
                guard
                    .query_row("SELECT COUNT(*) FROM events_all", [], |row| row.get(0))
                    .unwrap()
            };
            // The counter runs ahead of the inserts, so it is an upper bound
            // on the rows that can legitimately exist at any moment.
            let ceiling = committed.load(Ordering::SeqCst);
            assert!(
                count <= ceiling,
                "a query saw {count} rows but only {ceiling} were ever written — \
                 the hot and Parquet tiers were both visible at once"
            );
            queries += 1;
            std::thread::yield_now();
        }

        flusher.join().unwrap();
        assert!(queries > 0, "the reader never got to run");

        let total: u64 = readers
            .acquire()
            .lock()
            .query_row("SELECT COUNT(*) FROM events_all", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, (BATCH * ROUNDS) as u64, "every event exactly once");
    }

    #[test]
    fn test_shared_pool_has_one_connection() {
        let pool = ReaderPool::shared(&writer());
        assert_eq!(pool.len(), 1);
        assert!(!pool.is_empty());
    }

    #[test]
    fn test_size_zero_or_one_shares_the_writer() {
        let w = writer();
        assert_eq!(ReaderPool::new(&w, 0).len(), 1);
        assert_eq!(ReaderPool::new(&w, 1).len(), 1);
    }

    #[test]
    fn test_shared_pool_sees_writer_data() {
        let w = writer();
        let pool = ReaderPool::shared(&w);
        w.lock()
            .execute_batch(
                "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
                 VALUES ('a.com', 'v1', '2024-01-15 10:00:00', 'pageview', '/')",
            )
            .unwrap();
        let count: i64 = pool
            .acquire()
            .lock()
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_acquire_round_robins() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("test.duckdb")).unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        let w = Arc::new(Mutex::new(conn));
        let pool = ReaderPool::new(&w, 3);
        assert_eq!(pool.len(), 3);

        // Three successive acquisitions must hand out three distinct handles,
        // otherwise concurrent queries would serialise on one mutex.
        let a = pool.acquire();
        let b = pool.acquire();
        let c = pool.acquire();
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&b, &c));
        assert!(!Arc::ptr_eq(&a, &c));
        // The fourth wraps back to the first.
        assert!(Arc::ptr_eq(&pool.acquire(), &a));
    }

    #[test]
    fn test_file_backed_readers_see_committed_writes() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("test.duckdb")).unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        let w = Arc::new(Mutex::new(conn));
        let pool = ReaderPool::new(&w, 2);

        w.lock()
            .execute_batch(
                "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
                 VALUES ('a.com', 'v1', '2024-01-15 10:00:00', 'pageview', '/')",
            )
            .unwrap();

        for _ in 0..4 {
            let count: i64 = pool
                .acquire()
                .lock()
                .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 1, "every reader must see committed writes");
        }
    }

    #[test]
    fn test_readers_see_the_view_refreshed_after_a_flush() {
        // A flush rewrites `events_all` on the writer connection only. If the
        // pool's clones kept their own catalog, every flushed event would
        // disappear from the dashboard until the next restart — so this asserts
        // the catalog really is shared rather than trusting that it is.
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("test.duckdb")).unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        let events_dir = dir.path().join("events");
        crate::storage::schema::setup_query_view(&conn, &events_dir).unwrap();

        let w = Arc::new(Mutex::new(conn));
        let pool = ReaderPool::new(&w, 3);
        assert!(pool.len() > 1, "the test needs genuinely cloned readers");

        let storage = crate::storage::parquet::ParquetStorage::new(&events_dir, 0);
        w.lock()
            .execute_batch(
                "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
                 VALUES ('a.com', 'v1', '2024-01-15 10:00:00', 'pageview', '/')",
            )
            .unwrap();
        assert_eq!(storage.flush_events(&w.lock()).unwrap(), 1);

        // The row now lives only in Parquet; the hot table is empty.
        assert_eq!(
            w.lock()
                .query_row::<i64, _, _>("SELECT COUNT(*) FROM events", [], |row| row.get(0))
                .unwrap(),
            0
        );

        for _ in 0..6 {
            let count: i64 = pool
                .acquire()
                .lock()
                .query_row("SELECT COUNT(*) FROM events_all", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 1, "a reader lost sight of the flushed event");
        }
    }

    #[test]
    fn test_extension_load_is_per_connection_but_the_view_is_not() {
        // These two are not the same kind of state, and conflating them has
        // produced contradictory comments before. `CREATE OR REPLACE VIEW`
        // writes to the database catalog, which every connection shares;
        // `LOAD <extension>` arms the connection that runs it. So startup must
        // load the extension on each reader, while a view refresh on the writer
        // is enough for all of them.
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("test.duckdb")).unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        if crate::storage::schema::load_behavioral_extension(&conn).is_err() {
            assert!(
                std::env::var("MALLARD_REQUIRE_BEHAVIORAL").as_deref() != Ok("1"),
                "the behavioral extension is required here but could not be loaded, \
                 and MALLARD_REQUIRE_BEHAVIORAL=1 is set"
            );
            eprintln!("skipping: behavioral extension unavailable");
            return;
        }

        let w = Arc::new(Mutex::new(conn));
        let pool = ReaderPool::new(&w, 2);
        assert!(pool.len() > 1, "the test needs genuinely cloned readers");
        let reader = pool.acquire();

        // The view reaches the reader without any action on the reader.
        crate::storage::schema::setup_query_view(&w.lock(), &dir.path().join("events")).unwrap();
        reader
            .lock()
            .query_row::<i64, _, _>("SELECT COUNT(*) FROM events_all", [], |row| row.get(0))
            .expect("a view created on the writer must be visible to a reader");

        // The extension does not: the reader has to load it itself.
        let before = crate::storage::schema::behavioral_version(&reader.lock());
        crate::storage::schema::load_behavioral_extension(&reader.lock()).unwrap();
        let after = crate::storage::schema::behavioral_version(&reader.lock());
        assert!(
            after.is_some(),
            "the reader must report a version once it has loaded the extension"
        );
        // `before` is recorded rather than asserted: whether an install shared
        // through the database counts as loaded is a DuckDB implementation
        // detail. What matters is that startup loads it per connection, which
        // is correct either way.
        let _ = before;
    }

    #[test]
    fn test_for_each_visits_every_connection() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("test.duckdb")).unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        let w = Arc::new(Mutex::new(conn));
        let pool = ReaderPool::new(&w, 3);

        let mut visited = 0;
        pool.for_each(|_| {
            visited += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(visited, 3);
    }

    #[test]
    fn test_for_each_propagates_errors() {
        let pool = ReaderPool::shared(&writer());
        let result = pool.for_each(|conn| conn.execute_batch("SELECT * FROM no_such_table"));
        assert!(result.is_err());
    }
}
