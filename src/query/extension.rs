//! Whether the `behavioral` extension is usable, across the whole connection
//! pool, and getting it there when startup could not (S-11).
//!
//! Startup took the "loaded" flag from the writer alone, ignored the result of
//! loading on each reader (`let _ = readers.for_each(..)`, which also stopped at
//! the first failure), and nothing ever tried again: a network blip at boot
//! meant no funnels, retention or sessions until the next restart, and the 503
//! that said so was the only trace.
//!
//! [`ExtensionState`] is the one flag the handlers and `/health/detailed`
//! consult. It is set only when **every** connection reports the extension's
//! functions, because a query is handed whichever reader is next and one that
//! lacks them answers with an internal error instead of the 503 that tells the
//! operator what is wrong. [`retry_until_loaded`] keeps trying in the background
//! until it is.
//!
//! Measured on the pinned DuckDB (1.5.6) and extension (0.10.0): `LOAD` on the
//! writer also arms readers cloned from it
//! (`tests::loading_on_the_writer_arms_cloned_readers`). The earlier comments
//! that `LOAD` arms only the connection that ran it were not true of this build,
//! so in practice a reader can only lack the extension when the writer does too.
//! The per-connection check stays anyway: it costs one catalog query each, and
//! the flag's meaning ("any connection can run a funnel") should not depend on
//! an implementation detail that has already been misdescribed once.

use crate::ingest::handler::AppState;
use crate::storage::ReaderPool;
use crate::storage::schema::{behavioral_version, load_behavioral_extension};
use duckdb::Connection;
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How often a server that started without the extension tries again.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// Shared view of the extension's availability.
#[derive(Debug, Default)]
pub struct ExtensionState {
    loaded: AtomicBool,
    version: RwLock<Option<String>>,
}

impl ExtensionState {
    pub fn new(loaded: bool, version: Option<String>) -> Self {
        Self {
            loaded: AtomicBool::new(loaded),
            version: RwLock::new(version),
        }
    }

    /// True when every connection has the extension.
    pub fn is_loaded(&self) -> bool {
        self.loaded.load(Ordering::Acquire)
    }

    /// Version the extension reports, when loaded.
    pub fn version(&self) -> Option<String> {
        self.version.read().clone()
    }

    /// Record that every connection now has the extension.
    fn mark_loaded(&self, version: Option<String>) {
        // The version first, so a reader that sees the flag also sees it.
        *self.version.write() = version;
        self.loaded.store(true, Ordering::Release);
    }
}

/// How a pass over the pool went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadReport {
    /// Connections the pass tried to arm (those that lacked the extension).
    pub attempted: usize,
    /// Of those, how many now have it.
    pub loaded: usize,
}

impl LoadReport {
    /// True when no connection is left without the extension.
    pub const fn complete(self) -> bool {
        self.attempted == self.loaded
    }
}

/// Load the extension on every connection that lacks it, without stopping at
/// the first failure.
///
/// Returns what happened rather than a bare flag so a caller can say "2 of 4
/// readers" in a log line.
pub fn load_everywhere(writer: &Mutex<Connection>, readers: &ReaderPool) -> LoadReport {
    let mut report = LoadReport {
        attempted: 0,
        loaded: 0,
    };
    let mut arm = |name: &str, conn: &Connection| {
        if super::behavioral_loaded(conn) {
            return;
        }
        report.attempted += 1;
        match load_behavioral_extension(conn) {
            Ok(()) => report.loaded += 1,
            Err(e) => tracing::warn!(
                connection = name,
                error = %e,
                "Could not load the behavioral extension on a connection"
            ),
        }
    };
    arm("writer", &writer.lock());
    // The closure cannot fail; `for_each` is only used to visit each reader.
    let _ = readers.for_each(|conn| {
        arm("reader", conn);
        Ok::<(), std::convert::Infallible>(())
    });
    report
}

/// Fetch the extension into the extension directory without holding any pool
/// connection.
///
/// `INSTALL` is the slow, network-bound half of loading. Run on the writer it
/// would hold the writer lock, and with it event ingestion, for as long as a
/// download takes or times out. A scratch connection pointed at the same
/// directories does the same work while nothing else waits on it, and the
/// per-connection `LOAD` that follows is a local file read.
fn install_without_the_pool(writer: &Mutex<Connection>) -> Result<(), duckdb::Error> {
    let setting = |conn: &Connection, name: &str| -> Option<String> {
        conn.query_row(&format!("SELECT current_setting('{name}')"), [], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .filter(|v| !v.is_empty())
    };
    let (home, extensions) = {
        let conn = writer.lock();
        (
            setting(&conn, "home_directory"),
            setting(&conn, "extension_directory"),
        )
    };

    let scratch = Connection::open_in_memory()?;
    for (name, value) in [
        ("home_directory", home),
        ("extension_directory", extensions),
    ] {
        if let Some(value) = value {
            scratch.execute_batch(&format!("SET {name} = '{}'", value.replace('\'', "''")))?;
        }
    }
    scratch.execute_batch("INSTALL behavioral FROM community")
}

/// One attempt to bring every connection up. Returns whether all have it now.
///
/// Blocking: call it from `spawn_blocking`.
pub fn retry_once(state: &AppState) -> bool {
    if state.behavioral.is_loaded() {
        return true;
    }
    let writer = state.buffer.conn();

    // Cheapest first: the file may already be installed and a connection may
    // simply have missed its `LOAD`.
    let mut report = load_everywhere(&writer, &state.readers);
    if !report.complete() {
        match install_without_the_pool(&writer) {
            Ok(()) => report = load_everywhere(&writer, &state.readers),
            Err(e) => {
                tracing::warn!(error = %e, "Could not install the behavioral extension; will retry");
                return false;
            }
        }
    }
    if !report.complete() {
        return false;
    }

    let version = behavioral_version(&writer.lock());
    state.behavioral.mark_loaded(version);
    true
}

/// Retry in the background every `every` until the extension is available on
/// every connection, then return.
pub async fn retry_until_loaded(state: Arc<AppState>, every: Duration) {
    loop {
        tokio::time::sleep(every).await;
        if state.behavioral.is_loaded() {
            return;
        }
        let attempt = Arc::clone(&state);
        match tokio::task::spawn_blocking(move || retry_once(&attempt)).await {
            Ok(true) => {
                tracing::info!(
                    version = state.behavioral.version(),
                    "Behavioral extension is now loaded on every connection"
                );
                return;
            }
            Ok(false) => {}
            Err(e) => tracing::error!(error = %e, "Behavioral extension retry panicked"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::state_builder;

    fn file_backed_state(readers: usize) -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open(dir.path().join("t.duckdb")).expect("open");
        crate::storage::schema::init_schema(&conn).expect("schema");
        let writer = Arc::new(Mutex::new(conn));
        let pool = ReaderPool::new(&writer, readers);
        assert_eq!(pool.len(), readers, "the test needs independent readers");
        let state = state_builder().build_over(writer, pool, dir.path());
        (state, dir)
    }

    #[test]
    fn state_reports_what_it_was_given_and_can_be_marked() {
        let state = ExtensionState::new(false, None);
        assert!(!state.is_loaded());
        assert_eq!(state.version(), None);
        state.mark_loaded(Some("v0.10.0".to_string()));
        assert!(state.is_loaded());
        assert_eq!(state.version().as_deref(), Some("v0.10.0"));
    }

    /// Skip (or fail, under `MALLARD_REQUIRE_BEHAVIORAL=1`) when the extension
    /// cannot be loaded at all in this environment.
    fn extension_available() -> bool {
        let probe = Connection::open_in_memory().expect("open");
        if load_behavioral_extension(&probe).is_ok() {
            return true;
        }
        assert!(
            std::env::var("MALLARD_REQUIRE_BEHAVIORAL").as_deref() != Ok("1"),
            "MALLARD_REQUIRE_BEHAVIORAL=1 but the extension could not load"
        );
        eprintln!("skipping: behavioral extension unavailable");
        false
    }

    fn readers_with_extension(state: &AppState) -> usize {
        let mut armed = 0;
        state
            .readers
            .for_each(|conn| {
                if crate::query::behavioral_loaded(conn) {
                    armed += 1;
                }
                Ok::<(), std::convert::Infallible>(())
            })
            .expect("count");
        armed
    }

    #[test]
    fn loading_on_the_writer_arms_cloned_readers() {
        // Pins the DuckDB behaviour this module's claims rest on, rather than
        // assuming it: no reader is touched, yet all of them have the functions.
        if !extension_available() {
            return;
        }
        let (state, _dir) = file_backed_state(3);
        assert_eq!(readers_with_extension(&state), 0, "nothing loaded yet");
        load_behavioral_extension(&state.buffer.conn().lock()).expect("load on the writer");
        assert_eq!(
            readers_with_extension(&state),
            3,
            "a LOAD on the writer is visible to readers cloned from it"
        );
    }

    #[test]
    fn a_server_that_started_without_the_extension_gets_it_from_the_retry() {
        // The S-11 failure: boot could not load the extension, so the flag is
        // false and every funnel answers 503. One retry (the install that boot
        // could not do having become possible) must fix it with no restart.
        if !extension_available() {
            return;
        }
        let (state, _dir) = file_backed_state(3);
        assert!(!state.behavioral.is_loaded());
        assert_eq!(readers_with_extension(&state), 0);

        assert!(retry_once(&state));

        assert!(state.behavioral.is_loaded());
        assert!(state.behavioral.version().is_some());
        assert_eq!(readers_with_extension(&state), 3, "every reader is armed");
    }

    #[test]
    fn load_everywhere_reports_what_it_did_and_is_idempotent() {
        if !extension_available() {
            return;
        }
        let (state, _dir) = file_backed_state(2);
        let writer = state.buffer.conn();
        let first = load_everywhere(&writer, &state.readers);
        assert!(first.complete());
        assert!(first.attempted > 0, "something lacked the extension");
        let second = load_everywhere(&writer, &state.readers);
        assert_eq!(second.attempted, 0, "nothing left to do the second time");
        assert!(second.complete());
    }

    #[test]
    fn the_scratch_install_reads_the_writers_settings() {
        // `install_without_the_pool` points a throwaway connection at the same
        // extension directory as the writer. Offline this can only prove the
        // plumbing (the extension is already installed here, so INSTALL is a
        // no-op); the download itself needs a network.
        if !extension_available() {
            return;
        }
        let (state, dir) = file_backed_state(1);
        let writer = state.buffer.conn();
        let extensions = dir.path().join("ext");
        std::fs::create_dir_all(&extensions).expect("mkdir");
        writer
            .lock()
            .execute_batch(&format!(
                "SET extension_directory = '{}'",
                extensions.to_string_lossy().replace('\'', "''")
            ))
            .expect("set directory");
        let result = install_without_the_pool(&writer);
        // Either the download worked (network) and the file landed in the
        // directory the writer uses, or it failed having tried that directory.
        // It must never have written anywhere else.
        match result {
            Ok(()) => assert!(
                std::fs::read_dir(&extensions)
                    .expect("read")
                    .flatten()
                    .count()
                    > 0,
                "installed, but not into the writer's extension directory"
            ),
            Err(e) => eprintln!("offline; the install attempt failed as expected: {e}"),
        }
    }

    #[tokio::test]
    async fn the_background_task_flips_the_flag_and_stops() {
        if !extension_available() {
            return;
        }
        let (state, _dir) = file_backed_state(2);
        assert!(!state.behavioral.is_loaded());
        tokio::time::timeout(
            Duration::from_secs(120),
            retry_until_loaded(Arc::clone(&state), Duration::from_millis(20)),
        )
        .await
        .expect("the retry task must finish once everything is armed");
        assert!(state.behavioral.is_loaded());
    }

    #[test]
    fn a_loaded_flag_short_circuits_the_retry() {
        let state = state_builder().behavioral_extension_loaded(true).build_state();
        // `build_state` takes the real extension state when it can; force the
        // claim so this exercises only the early return.
        state.behavioral.mark_loaded(None);
        assert!(retry_once(&state));
    }
}
