//! Online backup: a consistent snapshot of both storage tiers while serving.
//!
//! The database file cannot be copied from outside the process: DuckDB holds it
//! open, a copy taken mid-write is torn, and a second process cannot open it to
//! take its own snapshot. So the server takes the snapshot itself.
//!
//! What makes it consistent is the [`TierLock`]. Held exclusively, it keeps
//! flush, compaction, retention and erasure from changing either tier, and keeps
//! queries from observing the copy half done. The hot table is small (events not
//! yet flushed), so the lock is short; the Parquet half is hard links.
//!
//! Hard links are sound here because a published Parquet file is never modified:
//! compaction writes a new file and removes the old names, erasure removes
//! names. A link therefore keeps exactly the bytes that existed at snapshot
//! time, at no copy cost and almost no disk, even for a large cold tier. When the
//! backup directory is on another filesystem the link fails and the file is
//! copied instead.
//!
//! A backup is written to `<name>.partial` and renamed into place last, so a
//! directory without that suffix is a complete one.

use crate::storage::TierLock;
use duckdb::Connection;
use parking_lot::Mutex;
use std::fs;
use std::path::{Path, PathBuf};

/// State files kept beside the database that a restore also needs.
///
/// Without `.secret` every visitor gets a new ID, without `admin.json` the
/// restored instance is unconfigured and claimable, without `api_keys.json`
/// every API key stops working. The `extensions/` directory is left out: it is
/// downloaded again on start.
const STATE_FILES: &[&str] = &[
    ".secret",
    "admin.json",
    "api_keys.json",
    "share_links.json",
    "segments.json",
];

/// File name of the copied database inside a backup directory.
pub const BACKUP_DB_FILE: &str = "mallard.duckdb";
/// Directory name of the copied Parquet tree inside a backup directory.
pub const BACKUP_EVENTS_DIR: &str = "events";
/// Name of the manifest written beside them.
pub const BACKUP_MANIFEST: &str = "MANIFEST.json";

/// What a backup contains.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BackupReport {
    /// The finished backup directory.
    pub path: PathBuf,
    /// Parquet files in the snapshot.
    pub parquet_files: usize,
    /// Total size of those files.
    pub parquet_bytes: u64,
    /// Files linked rather than copied (the rest were copied).
    pub linked: usize,
    /// Rows in the copied hot table.
    pub hot_rows: u64,
    /// State files copied (see `STATE_FILES`).
    pub state_files: usize,
}

/// Why a backup could not be made.
#[derive(Debug)]
pub enum BackupError {
    /// The name is not a single safe path component.
    BadName(String),
    /// A backup of that name already exists; backups are never overwritten.
    Exists(PathBuf),
    /// Copying the database failed.
    Database(duckdb::Error),
    /// Reading the Parquet tree or writing the backup failed.
    Io(std::io::Error),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName(name) => write!(f, "invalid backup name {name:?}"),
            Self::Exists(path) => write!(f, "{} already exists", path.display()),
            Self::Database(e) => write!(f, "could not copy the database: {e}"),
            Self::Io(e) => write!(f, "could not write the backup: {e}"),
        }
    }
}

impl std::error::Error for BackupError {}

impl From<duckdb::Error> for BackupError {
    fn from(e: duckdb::Error) -> Self {
        Self::Database(e)
    }
}

impl From<std::io::Error> for BackupError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// A single path component with no way to leave the backup root.
fn is_safe_name(name: &str) -> bool {
    // `<name>.partial` is where an unfinished backup is written, and an old one
    // is removed before a new attempt; a finished backup that ended in that
    // suffix would be deleted by the next request for the name without it.
    !name.ends_with(".partial")
        && !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.contains("..")
}

/// Create `<backups_root>/<name>` holding a consistent snapshot of both tiers.
///
/// Blocking: takes the tier write lock for the whole copy, then the connection,
/// in that order (a flush does the same; the opposite order can deadlock against
/// one). Call from `spawn_blocking`, and do not hold the connection yourself.
///
/// # Errors
///
/// Fails without leaving a finished backup behind: the partial directory is
/// removed on error. An existing backup of the same name is an error, never
/// overwritten.
pub fn create_backup(
    conn: &Mutex<Connection>,
    tier: &TierLock,
    data_dir: &Path,
    events_dir: &Path,
    backups_root: &Path,
    name: &str,
) -> Result<BackupReport, BackupError> {
    if !is_safe_name(name) {
        return Err(BackupError::BadName(name.to_string()));
    }
    let final_dir = backups_root.join(name);
    if final_dir.exists() {
        return Err(BackupError::Exists(final_dir));
    }
    let partial = backups_root.join(format!("{name}.partial"));
    // A partial directory is from an earlier failed attempt; it is never a backup.
    if partial.exists() {
        fs::remove_dir_all(&partial)?;
    }
    fs::create_dir_all(&partial)?;
    // The backup holds the secret and the credential hashes, so it is as
    // sensitive as the data directory: owner only, whatever the umask says.
    restrict_to_owner(&partial)?;

    match snapshot_into(conn, tier, data_dir, events_dir, backups_root, &partial) {
        Ok(mut report) => {
            fs::rename(&partial, &final_dir)?;
            sync_dir(backups_root);
            report.path = final_dir;
            Ok(report)
        }
        Err(e) => {
            // Best effort: the error is the result, and a leftover partial
            // directory is replaced by the next attempt.
            let _ = fs::remove_dir_all(&partial);
            Err(e)
        }
    }
}

fn snapshot_into(
    conn: &Mutex<Connection>,
    tier: &TierLock,
    data_dir: &Path,
    events_dir: &Path,
    backups_root: &Path,
    dest: &Path,
) -> Result<BackupReport, BackupError> {
    let _exclusive = tier.write();
    let conn = conn.lock();

    // Fold the write-ahead log into the main file so the copy reads one state.
    conn.execute_batch("CHECKPOINT")?;
    let source: String = conn.query_row("SELECT current_database()", [], |row| row.get(0))?;
    let db_path = dest.join(BACKUP_DB_FILE);
    let attach = format!(
        "ATTACH '{}' AS mallard_backup; COPY FROM DATABASE \"{}\" TO mallard_backup;",
        db_path.to_string_lossy().replace('\'', "''"),
        source.replace('"', "\"\""),
    );
    conn.execute_batch(&attach)?;
    let hot_rows: u64 =
        conn.query_row("SELECT COUNT(*) FROM mallard_backup.events", [], |row| {
            row.get(0)
        })?;
    // Detach so the backup file is closed, checkpointed and complete.
    conn.execute_batch("DETACH mallard_backup")?;

    let mut stats = TreeStats::default();
    link_tree(
        events_dir,
        &dest.join(BACKUP_EVENTS_DIR),
        backups_root,
        &mut stats,
    )?;

    let mut state_files = 0;
    for name in STATE_FILES {
        match fs::copy(data_dir.join(name), dest.join(name)) {
            Ok(_) => {
                restrict_to_owner(&dest.join(name))?;
                state_files += 1;
            }
            // Not every deployment has all of them: no admin password yet, no
            // API keys, or the secret supplied through the environment.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }

    let report = BackupReport {
        path: dest.to_path_buf(),
        parquet_files: stats.files,
        parquet_bytes: stats.bytes,
        linked: stats.linked,
        hot_rows,
        state_files,
    };
    let manifest = serde_json::json!({
        "format": 1,
        "mallard_version": env!("CARGO_PKG_VERSION"),
        "created_at": chrono::Utc::now().to_rfc3339(),
        "hot_rows": report.hot_rows,
        "parquet_files": report.parquet_files,
        "parquet_bytes": report.parquet_bytes,
        "state_files": report.state_files,
    });
    fs::write(
        dest.join(BACKUP_MANIFEST),
        serde_json::to_vec_pretty(&manifest).map_err(std::io::Error::other)?,
    )?;
    sync_dir(dest);
    Ok(report)
}

#[derive(Default)]
struct TreeStats {
    files: usize,
    bytes: u64,
    linked: usize,
}

/// Mirror the published `.parquet` files under `src` into `dst`.
///
/// Staging files (`.tmp`, `.pending`, `.del`) are skipped: they are not part of
/// the readable tier and would restore as clutter. `skip` is never entered, so a
/// backup directory that lives inside the events directory does not back itself
/// up.
fn link_tree(src: &Path, dst: &Path, skip: &Path, stats: &mut TreeStats) -> std::io::Result<()> {
    let entries = match fs::read_dir(src) {
        Ok(entries) => entries,
        // No Parquet written yet: the backup has an empty events directory.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return fs::create_dir_all(dst);
        }
        Err(e) => return Err(e),
    };
    fs::create_dir_all(dst)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        let kind = entry.file_type()?;
        if path == skip {
            continue;
        }
        if kind.is_dir() {
            link_tree(&path, &target, skip, stats)?;
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "parquet") {
            stats.bytes += entry.metadata()?.len();
            stats.files += 1;
            if fs::hard_link(&path, &target).is_ok() {
                stats.linked += 1;
            } else {
                fs::copy(&path, &target)?;
            }
        }
    }
    Ok(())
}

/// Make a file or directory readable by its owner only.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Flush a directory's entries to disk. Failure only weakens durability, and
/// there is nothing useful to do about it, so it is ignored.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;

    fn fixture() -> (tempfile::TempDir, Mutex<Connection>, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("live.duckdb")).unwrap();
        schema::init_schema(&conn).unwrap();
        let events = dir.path().join("events");
        (dir, Mutex::new(conn), events)
    }

    fn insert(conn: &Connection, site: &str, ts: &str) {
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
             VALUES (?, 'v', CAST(? AS TIMESTAMP), 'pageview', '/')",
            duckdb::params![site, ts],
        )
        .unwrap();
    }

    fn write_parquet(conn: &Connection, events: &Path, site: &str, date: &str, file: &str) {
        let dir = events.join(format!("site_id={site}/date={date}"));
        fs::create_dir_all(&dir).unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM events WHERE site_id = '{site}') TO '{}' (FORMAT PARQUET)",
            dir.join(file).display()
        ))
        .unwrap();
    }

    #[test]
    fn test_backup_holds_both_tiers_and_opens_on_its_own() {
        let (dir, conn, events) = fixture();
        insert(&conn.lock(), "a.com", "2024-01-01 10:00:00");
        insert(&conn.lock(), "a.com", "2024-01-01 11:00:00");
        write_parquet(&conn.lock(), &events, "a.com", "2024-01-01", "0001.parquet");
        conn.lock().execute("DELETE FROM events", []).unwrap();
        insert(&conn.lock(), "a.com", "2024-01-02 09:00:00");

        let roots = dir.path().join("backups");
        fs::create_dir_all(&roots).unwrap();
        let report =
            create_backup(&conn, &TierLock::new(), dir.path(), &events, &roots, "b1").unwrap();

        assert_eq!(report.hot_rows, 1);
        assert_eq!(report.parquet_files, 1);
        assert_eq!(report.path, roots.join("b1"));
        assert!(!roots.join("b1.partial").exists());
        assert!(report.path.join(BACKUP_MANIFEST).exists());

        // The backup stands alone: open it with no reference to the live files.
        let restored = Connection::open(report.path.join(BACKUP_DB_FILE)).unwrap();
        let hot: i64 = restored
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hot, 1);
        let cold: i64 = restored
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM read_parquet('{}/**/*.parquet')",
                    report.path.join(BACKUP_EVENTS_DIR).display()
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cold, 2, "the flushed rows must be in the backup");
    }

    #[test]
    fn test_backup_is_unaffected_by_later_changes_to_the_live_tiers() {
        let (dir, conn, events) = fixture();
        insert(&conn.lock(), "a.com", "2024-01-01 10:00:00");
        write_parquet(&conn.lock(), &events, "a.com", "2024-01-01", "0001.parquet");
        let roots = dir.path().join("backups");
        fs::create_dir_all(&roots).unwrap();
        let report =
            create_backup(&conn, &TierLock::new(), dir.path(), &events, &roots, "b1").unwrap();

        // Erase and compact away the live copy, as the server may do at any time.
        fs::remove_dir_all(events.join("site_id=a.com")).unwrap();
        conn.lock().execute("DELETE FROM events", []).unwrap();

        let linked = report
            .path
            .join(BACKUP_EVENTS_DIR)
            .join("site_id=a.com/date=2024-01-01/0001.parquet");
        assert!(linked.exists(), "a link must outlive the original name");
        let restored = Connection::open(report.path.join(BACKUP_DB_FILE)).unwrap();
        let rows: i64 = restored
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn test_staging_files_are_not_backed_up() {
        let (dir, conn, events) = fixture();
        insert(&conn.lock(), "a.com", "2024-01-01 10:00:00");
        write_parquet(&conn.lock(), &events, "a.com", "2024-01-01", "0001.parquet");
        let partition = events.join("site_id=a.com/date=2024-01-01");
        for junk in [
            "0002.parquet.tmp",
            "0003.parquet.pending",
            "0004.parquet.del",
        ] {
            fs::write(partition.join(junk), b"x").unwrap();
        }
        let roots = dir.path().join("backups");
        fs::create_dir_all(&roots).unwrap();
        let report =
            create_backup(&conn, &TierLock::new(), dir.path(), &events, &roots, "b1").unwrap();
        assert_eq!(report.parquet_files, 1);
        let names: Vec<String> = fs::read_dir(
            report
                .path
                .join(BACKUP_EVENTS_DIR)
                .join("site_id=a.com/date=2024-01-01"),
        )
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
        assert_eq!(names, vec!["0001.parquet".to_string()]);
    }

    #[test]
    fn test_an_existing_backup_is_never_overwritten() {
        let (dir, conn, events) = fixture();
        let roots = dir.path().join("backups");
        fs::create_dir_all(roots.join("b1")).unwrap();
        fs::write(roots.join("b1/keep"), b"precious").unwrap();
        let err =
            create_backup(&conn, &TierLock::new(), dir.path(), &events, &roots, "b1").unwrap_err();
        assert!(matches!(err, BackupError::Exists(_)), "{err}");
        assert_eq!(fs::read(roots.join("b1/keep")).unwrap(), b"precious");
    }

    #[test]
    fn test_unsafe_names_are_refused() {
        let (dir, conn, events) = fixture();
        let roots = dir.path().join("backups");
        fs::create_dir_all(&roots).unwrap();
        for bad in [
            "",
            "..",
            "../x",
            "a/b",
            ".hidden",
            "a b",
            "a\\b",
            "x..y",
            "ok.partial",
        ] {
            let err = create_backup(&conn, &TierLock::new(), dir.path(), &events, &roots, bad)
                .unwrap_err();
            assert!(matches!(err, BackupError::BadName(_)), "{bad:?}: {err}");
        }
    }

    #[test]
    fn test_a_failed_backup_leaves_no_partial_directory() {
        let (dir, conn, events) = fixture();
        // The events path is a file, so reading the Parquet tree fails after the
        // database half has already been copied.
        fs::write(&events, b"not a directory").unwrap();
        let roots = dir.path().join("backups");
        fs::create_dir_all(&roots).unwrap();

        let result = create_backup(&conn, &TierLock::new(), dir.path(), &events, &roots, "b1");
        assert!(matches!(result, Err(BackupError::Io(_))), "{result:?}");
        assert!(!roots.join("b1").exists());
        assert!(!roots.join("b1.partial").exists());
    }

    #[test]
    fn test_a_backup_and_a_flush_do_not_deadlock() {
        // A flush takes the tier lock and then the connection. A backup that took
        // them the other way round would deadlock against it, and the failure
        // would be a hang, so the work runs on a thread and the test waits with
        // a timeout instead of joining.
        let (dir, conn, events) = fixture();
        let conn = std::sync::Arc::new(conn);
        let tier = TierLock::new();
        let roots = dir.path().join("backups");
        fs::create_dir_all(&roots).unwrap();
        let dir_path = dir.path().to_path_buf();

        let flusher = {
            let (conn, tier) = (std::sync::Arc::clone(&conn), tier.clone());
            std::thread::spawn(move || {
                for _ in 0..300 {
                    let _tier = tier.write();
                    let _conn = conn.lock();
                    std::thread::yield_now();
                }
            })
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let backer = {
            let conn = std::sync::Arc::clone(&conn);
            std::thread::spawn(move || {
                for i in 0..20 {
                    create_backup(
                        &conn,
                        &tier,
                        dir_path.as_path(),
                        &events,
                        &roots,
                        &format!("b{i}"),
                    )
                    .unwrap();
                }
                let _ = done_tx.send(());
            })
        };
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_secs(60))
                .is_ok(),
            "backups deadlocked against a flush-ordered locker"
        );
        backer.join().unwrap();
        flusher.join().unwrap();
        drop(dir);
    }

    #[test]
    fn test_a_backup_directory_inside_the_events_directory_is_not_backed_up() {
        let (dir, conn, events) = fixture();
        insert(&conn.lock(), "a.com", "2024-01-01 10:00:00");
        write_parquet(&conn.lock(), &events, "a.com", "2024-01-01", "0001.parquet");
        let roots = events.join("backups");
        fs::create_dir_all(&roots).unwrap();

        let first =
            create_backup(&conn, &TierLock::new(), dir.path(), &events, &roots, "b1").unwrap();
        let second =
            create_backup(&conn, &TierLock::new(), dir.path(), &events, &roots, "b2").unwrap();

        assert_eq!(first.parquet_files, 1);
        assert_eq!(
            second.parquet_files, 1,
            "the first backup's files must not be counted in the second"
        );
        assert!(!second.path.join(BACKUP_EVENTS_DIR).join("backups").exists());
    }

    #[test]
    fn test_state_files_are_copied_and_the_backup_is_owner_only() {
        let (dir, conn, events) = fixture();
        fs::write(dir.path().join(".secret"), b"visitor-secret").unwrap();
        fs::write(dir.path().join("admin.json"), b"{}").unwrap();
        // api_keys.json is absent: no keys have been created.
        let roots = dir.path().join("backups");
        fs::create_dir_all(&roots).unwrap();

        let report =
            create_backup(&conn, &TierLock::new(), dir.path(), &events, &roots, "b1").unwrap();

        assert_eq!(report.state_files, 2);
        assert_eq!(
            fs::read(report.path.join(".secret")).unwrap(),
            b"visitor-secret"
        );
        assert!(report.path.join("admin.json").exists());
        assert!(!report.path.join("api_keys.json").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&report.path), 0o700, "the backup directory");
            assert_eq!(mode(&report.path.join(".secret")), 0o600, "the secret");
        }
    }

    #[test]
    fn test_an_in_memory_database_can_be_backed_up() {
        // Production databases are files, but the in-process test fixtures are
        // not, and a backup that only works for one of them cannot be tested
        // through the HTTP layer.
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        schema::init_schema(&conn).unwrap();
        insert(&conn, "a.com", "2024-01-01 10:00:00");
        let roots = dir.path().join("backups");
        fs::create_dir_all(&roots).unwrap();
        let conn = Mutex::new(conn);
        let report = create_backup(
            &conn,
            &TierLock::new(),
            dir.path(),
            &dir.path().join("events"),
            &roots,
            "m1",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(report.hot_rows, 1);
    }

    #[test]
    fn test_no_parquet_yet_gives_an_empty_events_directory() {
        let (dir, conn, events) = fixture();
        insert(&conn.lock(), "a.com", "2024-01-01 10:00:00");
        let roots = dir.path().join("backups");
        fs::create_dir_all(&roots).unwrap();
        let report =
            create_backup(&conn, &TierLock::new(), dir.path(), &events, &roots, "b1").unwrap();
        assert_eq!(report.parquet_files, 0);
        assert_eq!(report.hot_rows, 1);
        assert!(report.path.join(BACKUP_EVENTS_DIR).is_dir());
    }
}
