use crate::storage::TierLock;
use crate::storage::schema::EVENT_COLUMNS;
use duckdb::Connection;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Most (site, date) partitions a single flush will write.
///
/// A flush holds the exclusive tier lock, and every partition costs a scan of
/// the hot table, a Parquet write, an fsync and a `DELETE`. With ingest open to
/// any `d` value, one POST per fake site would otherwise turn a flush into
/// thousands of file writes while every dashboard query waits (SI-08). The
/// rows over the cap are not lost: they stay in the hot table, which is
/// durable and visible through `events_all`, and the next flush takes the
/// next batch.
pub const DEFAULT_MAX_PARTITIONS_PER_FLUSH: usize = 256;

/// Suffix of a Parquet file that is fully written and fsynced but not yet
/// published, because the hot-table `DELETE` that hands its rows over has not
/// been confirmed. Deliberately not `.tmp`: [`ParquetStorage::cleanup_temp_files`]
/// deletes `.tmp` files without consulting the database, and a `.pending` file
/// can be the only copy of its rows.
const PENDING_SUFFIX: &str = ".pending";
/// Suffix of a compaction source that has been moved out of the read glob.
const DEL_SUFFIX: &str = ".del";
/// Name of the file a compaction writes its output to before the swap.
const COMPACT_TMP_NAME: &str = "compact.parquet.tmp";

/// Manages Parquet file storage with a date-partitioned layout.
///
/// Storage layout:
/// ```text
/// data/events/site_id=example.com/date=2024-01-15/0001.parquet      flush output
/// data/events/site_id=example.com/date=2024-01-15/0025.m1.parquet   merge of 24 flush files
/// ```
///
/// The `.mN` infix is the compaction level. Files only ever merge with files of
/// their own level, so a large merged file is never read again until enough
/// large files exist to merge among themselves (F-07).
#[derive(Clone)]
pub struct ParquetStorage {
    base_dir: PathBuf,
    /// Compact a partition once it holds at least this many files of one
    /// level. 0 = never.
    compact_after_files: usize,
    /// Cap on partitions written by one flush. See
    /// [`DEFAULT_MAX_PARTITIONS_PER_FLUSH`].
    max_partitions_per_flush: usize,
    /// One compaction at a time: two would write the same staging file.
    compaction_busy: Arc<Mutex<()>>,
    /// Set once interrupted flushes have been recovered in this process.
    recovered: Arc<AtomicBool>,
    #[cfg(test)]
    faults: Arc<Mutex<Faults>>,
}

/// Fault injection for the crash-safety tests. A real crash cannot be staged
/// inside a unit test, so each variant makes the code stop (or fail) at the
/// exact boundary where a power cut would leave the disk in a given state.
#[cfg(test)]
#[derive(Default)]
struct Faults {
    /// Stop the flush as if the process died right after this step.
    crash_after: Option<CrashPoint>,
    /// Fail the rename that publishes a flushed file.
    fail_publish: bool,
    /// Fail moving the n-th compaction source out of the glob.
    fail_source_move: Option<usize>,
    /// Fail recursive removal of partition directories.
    fail_dir_removal: bool,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CrashPoint {
    /// Staged file written and fsynced; the hot rows are still there.
    FlushAfterStage,
    /// Hot rows deleted and committed; file not yet published.
    FlushAfterDelete,
    /// Compaction sources moved aside; merged file not yet published.
    CompactAfterSourcesMoved,
    /// Merged file published; the moved-aside sources not yet unlinked.
    CompactAfterPublish,
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

/// Characters DuckDB treats as glob syntax in a file name.
const GLOB_CHARS: &[char] = &['*', '?', '[', ']'];

/// Check that a data directory can be embedded in a glob without changing what
/// the glob matches (F-11).
///
/// Only `'` is escaped when the path is interpolated into SQL, so a directory
/// such as `/data/[prod]` would silently make `events_all` match nothing, or
/// something else. Rejecting it up front turns a silent loss of the cold tier
/// into a startup error with a message.
///
/// # Errors
///
/// Returns a description of the offending character.
pub fn validate_data_dir(dir: &Path) -> Result<(), String> {
    let text = dir.to_string_lossy();
    text.find(GLOB_CHARS).map_or(Ok(()), |at| {
        Err(format!(
            "the data directory {text:?} contains the glob character {:?}; \
             DuckDB would treat it as a pattern when reading Parquet files. \
             Choose a directory without any of * ? [ ]",
            text[at..].chars().next().unwrap_or('?')
        ))
    })
}

/// `path` with `suffix` appended to its file name (`0001.parquet` ->
/// `0001.parquet.pending`).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Flush a file's contents to stable storage.
fn sync_file(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

/// Flush a directory entry (a rename or unlink) to stable storage.
///
/// DuckDB's Parquet writer closes its file without an fsync, and a rename is
/// only durable once its directory is synced, so after power loss a file could
/// be present but short, or absent although the rows it replaced were already
/// deleted (F-03). Directories cannot be opened for sync on Windows, where
/// NTFS journals metadata anyway.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// `0007` -> (7, level 0); `0007.m2` -> (7, level 2).
fn parse_stem(stem: &str) -> Option<(u32, u32)> {
    match stem.split_once(".m") {
        Some((number, level)) => Some((number.parse().ok()?, level.parse().ok()?)),
        None => Some((stem.parse().ok()?, 0)),
    }
}

/// A numbered Parquet file in a partition.
#[derive(Debug, Clone)]
struct PartFile {
    number: u32,
    level: u32,
    path: PathBuf,
}

/// What one flush did.
#[derive(Debug, Default, Clone)]
pub struct FlushReport {
    /// Rows moved from the hot table into Parquet.
    pub flushed: usize,
    /// Partitions that received a new file, for the caller to compact.
    pub touched: Vec<(String, String)>,
    /// Partitions left in the hot table because the per-flush cap was reached.
    pub deferred_partitions: usize,
}

/// Outcome of finishing flushes that a crash interrupted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Staged files published because their rows had already left the hot table.
    pub published: usize,
    /// Staged files dropped because their rows are still in the hot table.
    pub discarded: usize,
    /// Staged files that could not be read and were set aside as `.corrupt`.
    pub quarantined: usize,
}

/// What an erasure did, partition by partition.
#[derive(Debug, Default)]
pub struct ErasureReport {
    /// Partition directories removed.
    pub removed: u64,
    /// Dates whose directory could not be removed, with the reason.
    pub failed: Vec<(String, std::io::Error)>,
}

impl ParquetStorage {
    pub fn new(base_dir: &Path, compact_after_files: usize) -> Self {
        Self {
            base_dir: base_dir.to_path_buf(),
            compact_after_files,
            max_partitions_per_flush: DEFAULT_MAX_PARTITIONS_PER_FLUSH,
            compaction_busy: Arc::new(Mutex::new(())),
            recovered: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            faults: Arc::new(Mutex::new(Faults::default())),
        }
    }

    /// Set the per-flush partition cap. 0 is treated as 1: a flush that may
    /// write nothing would never drain.
    #[must_use]
    pub fn with_max_partitions_per_flush(mut self, max: usize) -> Self {
        self.max_partitions_per_flush = max.max(1);
        self
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

    /// Numbered files in a partition whose names end in `suffix`.
    fn parts_with_suffix(dir: &Path, suffix: &str) -> Vec<PartFile> {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let stem = name.to_str()?.strip_suffix(suffix)?;
                let (number, level) = parse_stem(stem)?;
                Some(PartFile {
                    number,
                    level,
                    path: entry.path(),
                })
            })
            .collect()
    }

    /// Published Parquet files in a partition (the ones the read glob sees).
    fn partition_parts(dir: &Path) -> Vec<PartFile> {
        let mut parts = Self::parts_with_suffix(dir, ".parquet");
        parts.sort_unstable_by_key(|p| p.number);
        parts
    }

    /// Highest file number in a partition, or 0 when empty.
    ///
    /// Staged (`.pending`) files count: a recovered file must never collide
    /// with one published after the crash.
    fn max_file_number(dir: &Path) -> u32 {
        Self::parts_with_suffix(dir, ".parquet")
            .into_iter()
            .chain(Self::parts_with_suffix(
                dir,
                &format!(".parquet{PENDING_SUFFIX}"),
            ))
            .map(|p| p.number)
            .max()
            .unwrap_or(0)
    }

    /// List the numbered Parquet files in a partition, sorted ascending.
    #[cfg(test)]
    fn partition_files(dir: &Path) -> Vec<(u32, PathBuf)> {
        Self::partition_parts(dir)
            .into_iter()
            .map(|p| (p.number, p.path))
            .collect()
    }

    /// The next available Parquet file path in a partition.
    fn next_file_path(&self, site_id: &str, date: &str) -> std::io::Result<PathBuf> {
        let dir = self.partition_dir(site_id, date);
        fs::create_dir_all(&dir)?;
        Ok(dir.join(format!("{:04}.parquet", Self::max_file_number(&dir) + 1)))
    }

    /// Make the next flushes stop as a power cut would: `after_delete` false
    /// stops once the staged file is durable, true once the hot rows are also
    /// deleted. `None` clears it. For the importer's crash tests.
    #[cfg(test)]
    pub(crate) fn inject_flush_crash(&self, after_delete: Option<bool>) {
        self.faults.lock().crash_after = after_delete.map(|d| {
            if d {
                CrashPoint::FlushAfterDelete
            } else {
                CrashPoint::FlushAfterStage
            }
        });
    }

    #[cfg(test)]
    fn crash_at(&self, point: CrashPoint) -> bool {
        self.faults.lock().crash_after == Some(point)
    }

    /// Write a query's result set to `staging` and make it durable.
    ///
    /// DuckDB's `COPY TO` streams straight into the destination, so a crash or
    /// a full disk mid-write leaves a truncated file behind. `events_all` globs
    /// every `*.parquet`, so one truncated file would fail every later query.
    /// The staging name does not end in `.parquet`, which keeps a leftover out
    /// of the glob. DuckDB closes the file without an fsync (F-03), so it is
    /// synced here before anything depends on it.
    fn stage_parquet(
        conn: &Connection,
        select_sql: &str,
        staging: &Path,
    ) -> Result<(), FlushError> {
        let _ = fs::remove_file(staging);
        let target = sql_quote(&staging.to_string_lossy());
        // `COPY ... TO` cannot be parameterised (L1); the path is built from
        // the configured data directory and validated site/date components.
        let copy_sql =
            format!("COPY ({select_sql}) TO '{target}' (FORMAT PARQUET, COMPRESSION ZSTD)");
        if let Err(e) = conn.execute_batch(&copy_sql) {
            let _ = fs::remove_file(staging);
            return Err(FlushError::Write(e));
        }
        if let Err(e) = sync_file(staging) {
            let _ = fs::remove_file(staging);
            return Err(FlushError::Durability(e));
        }
        Ok(())
    }

    /// Finish flushes that a crash interrupted. Call at startup, after
    /// opening the database and before building the query view.
    ///
    /// A flush stages its Parquet file as `NNNN.parquet.pending`, deletes the
    /// partition's rows from the hot table, and only then publishes the file.
    /// A crash therefore leaves one of two states, and comparing the staged
    /// file with the hot table says which:
    ///
    /// - every staged row is still in the hot table: the `DELETE` never
    ///   committed, so the staged file is a duplicate and is dropped;
    /// - some staged rows are not hot: the `DELETE` committed and the staged
    ///   file is the only copy, so it is published.
    ///
    /// The comparison is by row content (`EXCEPT ALL`), not by "does the
    /// partition have hot rows", so it stays correct if the process kept
    /// running and ingesting after a failed publish. A staged file that cannot
    /// be read is dropped if the partition still has hot rows (a crash
    /// mid-write) and otherwise set aside as `.corrupt`, never deleted.
    ///
    /// Run at startup, after opening the database and before building the
    /// query view ([`EventBuffer`] repeats it before its first insert as a
    /// safety net).
    ///
    /// [`EventBuffer`]: crate::ingest::buffer::EventBuffer
    ///
    /// # Errors
    ///
    /// Returns an error if the hot table cannot be queried. Individual files
    /// that cannot be published are logged and left in place for the next run.
    pub fn recover_pending_flushes(
        &self,
        conn: &Connection,
    ) -> Result<RecoveryReport, duckdb::Error> {
        let mut report = RecoveryReport::default();
        let columns = EVENT_COLUMNS.join(", ");

        for (site_id, date, dir) in self.partition_dirs() {
            for file in Self::parts_with_suffix(&dir, &format!(".parquet{PENDING_SUFFIX}")) {
                let path = sql_quote(&file.path.to_string_lossy());
                // Rows the staged file holds that the hot table does not. Bag
                // semantics: a row that appears twice must be hot twice.
                let missing = conn.query_row::<i64, _, _>(
                    &format!(
                        "SELECT COUNT(*) FROM (\
                           SELECT {columns} FROM read_parquet('{path}') \
                           EXCEPT ALL \
                           SELECT {columns} FROM events WHERE site_id = ? \
                             AND STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d') = ?)"
                    ),
                    duckdb::params![site_id, date],
                    |row| row.get(0),
                );
                match missing {
                    // Every staged row is still hot: the DELETE never
                    // committed, so this is a duplicate.
                    Ok(0) => match fs::remove_file(&file.path) {
                        Ok(()) => report.discarded += 1,
                        Err(e) => tracing::error!(
                            path = %file.path.display(), error = %e,
                            "Could not drop a staged Parquet file whose rows are still hot"
                        ),
                    },
                    // Rows exist only in the staged file: it is the only copy.
                    Ok(_) => {
                        match Self::publish_recovered(&dir, &file.path, file.number, file.level) {
                            Ok(()) => report.published += 1,
                            Err(e) => tracing::error!(
                                path = %file.path.display(), error = %e,
                                "A staged Parquet file holds the only copy of flushed events \
                                 and could not be published; it was left in place"
                            ),
                        }
                    }
                    // Not readable. If the partition still has hot rows, the
                    // crash came mid-write (before the DELETE) and the file is
                    // just an incomplete duplicate. If it has none, a file that
                    // was fsynced before the DELETE is unreadable, which must
                    // never be deleted: set it aside for an operator.
                    Err(read_error) => {
                        let hot_rows = Self::hot_rows_in_partition(conn, &site_id, &date)?;
                        if hot_rows > 0 && fs::remove_file(&file.path).is_ok() {
                            report.discarded += 1;
                        } else if hot_rows == 0
                            && fs::rename(&file.path, with_suffix(&file.path, ".corrupt")).is_ok()
                        {
                            tracing::error!(
                                path = %file.path.display(), error = %read_error,
                                "A staged Parquet file is unreadable and its rows are not in the \
                                 hot table; it was set aside as .corrupt"
                            );
                            report.quarantined += 1;
                        }
                    }
                }
            }
        }

        if report != RecoveryReport::default() {
            tracing::warn!(
                published = report.published,
                discarded = report.discarded,
                quarantined = report.quarantined,
                "Recovered Parquet flushes interrupted by a crash"
            );
        }
        if report.published > 0 {
            // New files may have appeared where the view was built hot-only.
            if let Err(e) = crate::storage::schema::ensure_query_view(conn, &self.base_dir) {
                tracing::error!(error = %e, "Could not refresh events_all after recovery");
            }
        }
        self.recovered.store(true, Ordering::Release);
        Ok(report)
    }

    /// [`recover_pending_flushes`](Self::recover_pending_flushes) once per
    /// process, for callers that did not run it at startup.
    ///
    /// # Errors
    ///
    /// As for `recover_pending_flushes`.
    pub fn ensure_recovered(&self, conn: &Connection) -> Result<(), duckdb::Error> {
        if self.recovered.load(Ordering::Acquire) {
            return Ok(());
        }
        self.recover_pending_flushes(conn).map(|_| ())
    }

    /// Publish a staged file found at startup.
    ///
    /// The caller has just read the file, so it is known to be valid Parquet
    /// before it joins the glob (a bad file there fails every query).
    fn publish_recovered(
        dir: &Path,
        staged: &Path,
        staged_number: u32,
        level: u32,
    ) -> Result<(), FlushError> {
        // Keep the number it was staged under unless a later flush has since
        // taken it.
        let name = |number: u32| {
            if level == 0 {
                format!("{number:04}.parquet")
            } else {
                format!("{number:04}.m{level}.parquet")
            }
        };
        let own = dir.join(name(staged_number));
        let target = if own.exists() {
            dir.join(name(Self::max_file_number(dir) + 1))
        } else {
            own
        };
        fs::rename(staged, target).map_err(FlushError::Rename)?;
        sync_dir(dir).map_err(FlushError::Durability)
    }

    fn hot_rows_in_partition(
        conn: &Connection,
        site_id: &str,
        date: &str,
    ) -> Result<i64, duckdb::Error> {
        conn.query_row(
            "SELECT COUNT(*) FROM events \
             WHERE site_id = ? AND STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d') = ?",
            duckdb::params![site_id, date],
            |row| row.get(0),
        )
    }

    /// Every `site_id=*/date=*` directory with its site and date.
    fn partition_dirs(&self) -> Vec<(String, String, PathBuf)> {
        let mut out = Vec::new();
        for site in fs::read_dir(&self.base_dir).into_iter().flatten().flatten() {
            let site_name = site.file_name();
            let Some(site_id) = site_name
                .to_str()
                .and_then(|n| n.strip_prefix("site_id="))
                .map(str::to_string)
            else {
                continue;
            };
            if !site.path().is_dir() {
                continue;
            }
            for date in fs::read_dir(site.path()).into_iter().flatten().flatten() {
                let date_name = date.file_name();
                let Some(date_str) = date_name
                    .to_str()
                    .and_then(|n| n.strip_prefix("date="))
                    .map(str::to_string)
                else {
                    continue;
                };
                if date.path().is_dir() {
                    out.push((site_id.clone(), date_str, date.path()));
                }
            }
        }
        out
    }

    /// Clean up after an interrupted write or compaction. Called at startup.
    ///
    /// - `*.parquet.tmp` is a write that never finished; it is deleted.
    /// - `*.parquet.del` is a compaction source that was moved out of the
    ///   glob. If the compaction's staging file is still present the swap did
    ///   not complete, so the sources are put back; otherwise the merged file
    ///   is already published and the leftovers are deleted (F-01).
    ///
    /// `*.parquet.pending` files are left alone: they may be the only copy of
    /// flushed events and are handled by
    /// [`recover_pending_flushes`](Self::recover_pending_flushes).
    ///
    /// Returns the number of files deleted.
    pub fn cleanup_temp_files(&self) -> std::io::Result<usize> {
        let mut removed = 0usize;
        for (site_id, date, dir) in self.partition_dirs() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            let ends = |p: &PathBuf, suffix: &str| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(suffix))
            };
            let temps: Vec<&PathBuf> = names.iter().filter(|p| ends(p, ".parquet.tmp")).collect();
            let dels: Vec<&PathBuf> = names.iter().filter(|p| ends(p, ".parquet.del")).collect();

            if !temps.is_empty() && !dels.is_empty() {
                // Crashed between moving the sources aside and publishing the
                // merged file: undo, so every row is visible exactly once.
                for del in &dels {
                    let restored = del.with_extension("");
                    match fs::rename(del, &restored) {
                        Ok(()) => tracing::warn!(
                            site_id, date, path = %restored.display(),
                            "Restored a compaction source after an interrupted compaction"
                        ),
                        Err(e) => tracing::error!(
                            path = %del.display(), error = %e,
                            "Could not restore a compaction source; its rows are \
                             missing from analytics until it is renamed back by hand"
                        ),
                    }
                }
            } else {
                for del in &dels {
                    if fs::remove_file(del).is_ok() {
                        removed += 1;
                    }
                }
            }
            for tmp in temps {
                if fs::remove_file(tmp).is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// Flush events from the DuckDB `events` table to partitioned Parquet files,
    /// then compact the partitions that received a file.
    ///
    /// The caller must hold the exclusive tier lock for the whole call (see
    /// [`TierLock`]), compaction included. Production code uses
    /// [`flush_events_deferred`](Self::flush_events_deferred) and compacts
    /// outside the lock instead.
    ///
    /// # Errors
    ///
    /// Returns an error if the partition query, the Parquet write, the rename, or
    /// the follow-up delete fails.
    pub fn flush_events(&self, conn: &Connection) -> Result<usize, FlushError> {
        let report = self.flush_events_deferred(conn)?;
        for (site_id, date) in &report.touched {
            if let Err(e) = self.compact_partition_locked(conn, site_id, date) {
                // Compaction is an optimisation; a failure must not lose data
                // or fail the flush that already succeeded.
                tracing::warn!(site_id, date, error = %e, "Parquet compaction failed");
            }
        }
        Ok(report.flushed)
    }

    /// Flush without compacting; returns what was written so the caller can
    /// compact after releasing the tier lock.
    ///
    /// Writes at most `max_partitions_per_flush` partitions, largest first, so
    /// real traffic is not starved by a flood of one-event partitions.
    ///
    /// The query view is refreshed if any file was written, even when a later
    /// partition fails: otherwise a view built while the cold tier was empty
    /// would keep hiding partitions that had already left the hot table (F-04).
    ///
    /// # Errors
    ///
    /// As for [`flush_events`](Self::flush_events).
    pub fn flush_events_deferred(&self, conn: &Connection) -> Result<FlushReport, FlushError> {
        let mut report = FlushReport::default();
        let outcome = self.flush_partitions(conn, &mut report);
        if !report.touched.is_empty()
            && let Err(e) = crate::storage::schema::ensure_query_view(conn, &self.base_dir)
        {
            // The rows are durable either way; what is at stake is whether
            // analytics can see them. Say so loudly and let the health
            // check read `cold_tier_status`.
            tracing::error!(
                error = %e,
                "Could not refresh the events_all view after a flush; \
                 analytics may be missing flushed events"
            );
        }
        outcome.map(|()| report)
    }

    fn flush_partitions(
        &self,
        conn: &Connection,
        report: &mut FlushReport,
    ) -> Result<(), FlushError> {
        let mut stmt = conn
            .prepare(
                "SELECT site_id, STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d') AS d, COUNT(*) AS cnt \
                 FROM events GROUP BY site_id, d ORDER BY cnt DESC, site_id, d",
            )
            .map_err(FlushError::Query)?;
        let partitions: Vec<(String, String, usize)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(FlushError::Query)?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);

        let mut written = 0usize;
        for (site_id, date, count) in &partitions {
            if !is_safe_path_component(site_id) {
                tracing::warn!(site_id, "Skipping flush for unsafe site_id");
                continue;
            }
            if written >= self.max_partitions_per_flush {
                report.deferred_partitions += 1;
                continue;
            }
            self.flush_partition(conn, site_id, date)?;
            written += 1;
            report.flushed += count;
            report.touched.push((site_id.clone(), date.clone()));
        }
        if report.deferred_partitions > 0 {
            tracing::warn!(
                deferred = report.deferred_partitions,
                cap = self.max_partitions_per_flush,
                "Flush hit its partition cap; the remaining partitions stay in the hot \
                 table until the next flush"
            );
        }
        Ok(())
    }

    /// Move one partition's rows from the hot table into a new Parquet file.
    ///
    /// Order matters, and so does what each crash point leaves behind:
    ///
    /// 1. write the rows to `NNNN.parquet.pending` and fsync it
    ///    (crash: duplicate file, dropped by recovery because the rows are
    ///    still hot);
    /// 2. delete the rows from the hot table (autocommit, so durable on return)
    ///    (crash: the only copy is the staged file, published by recovery);
    /// 3. rename to `NNNN.parquet` and fsync the directory.
    ///
    /// The tier lock hides the gap between 2 and 3 from queries. The old order
    /// (publish, then delete) left a crash window in which every row was
    /// counted twice until someone noticed (F-02).
    fn flush_partition(
        &self,
        conn: &Connection,
        site_id: &str,
        date: &str,
    ) -> Result<(), FlushError> {
        // `date` is produced by STRFTIME so it cannot contain a quote, but it
        // is escaped anyway rather than relying on that invariant holding.
        let escaped_site = sql_quote(site_id);
        let escaped_date = sql_quote(date);
        let predicate = format!(
            "site_id = '{escaped_site}' \
             AND STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d') = '{escaped_date}'"
        );

        let final_path = self
            .next_file_path(site_id, date)
            .map_err(FlushError::Rename)?;
        let staged = with_suffix(&final_path, PENDING_SUFFIX);

        Self::stage_parquet(
            conn,
            &format!("SELECT * FROM events WHERE {predicate}"),
            &staged,
        )?;
        #[cfg(test)]
        if self.crash_at(CrashPoint::FlushAfterStage) {
            return Err(FlushError::SimulatedCrash);
        }

        // If the delete fails the rows are still in the hot table and nothing
        // was published, so dropping the staged file restores the previous state.
        if let Err(e) = conn.execute_batch(&format!("DELETE FROM events WHERE {predicate}")) {
            if let Err(unlink) = fs::remove_file(&staged) {
                tracing::warn!(
                    path = %staged.display(), error = %unlink,
                    "Could not remove a staged file after a failed delete; startup recovery will"
                );
            }
            return Err(FlushError::Delete(e));
        }
        #[cfg(test)]
        if self.crash_at(CrashPoint::FlushAfterDelete) {
            return Err(FlushError::SimulatedCrash);
        }

        match self.publish(&staged, &final_path) {
            Ok(()) => Ok(()),
            Err(e) => Err(Self::reinstate(conn, &staged, e)),
        }
    }

    /// Publish a staged file and make the rename durable.
    // `self` is read only by the fault-injection hooks compiled into tests.
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn publish(&self, staged: &Path, final_path: &Path) -> std::io::Result<()> {
        #[cfg(test)]
        if self.faults.lock().fail_publish {
            return Err(std::io::Error::other("injected publish failure"));
        }
        fs::rename(staged, final_path)?;
        if let Some(parent) = final_path.parent() {
            // The rename is done; failing to sync it must not undo it.
            if let Err(e) = sync_dir(parent) {
                tracing::warn!(
                    dir = %parent.display(), error = %e,
                    "Could not fsync a partition directory after publishing a file"
                );
            }
        }
        Ok(())
    }

    /// Put a staged file's rows back into the hot table after publishing it
    /// failed. The rows were already deleted from the hot table, so without
    /// this they would be in neither tier until the next restart.
    fn reinstate(conn: &Connection, staged: &Path, cause: std::io::Error) -> FlushError {
        let columns = EVENT_COLUMNS.join(", ");
        let path = sql_quote(&staged.to_string_lossy());
        let restore = conn.execute_batch(&format!(
            "INSERT INTO events ({columns}) SELECT {columns} FROM read_parquet('{path}')"
        ));
        match restore {
            Ok(()) => {
                let _ = fs::remove_file(staged);
            }
            Err(e) => tracing::error!(
                path = %staged.display(), error = %e,
                "Could not return flushed rows to the hot table; they are preserved in the \
                 staged file and will be published by the next startup recovery"
            ),
        }
        FlushError::Rename(cause)
    }

    /// The set of files in a partition that should be merged next, if any.
    ///
    /// Files merge only with files of their own level. The lowest level with
    /// at least `compact_after_files` members goes first.
    fn plan_compaction(&self, dir: &Path) -> Option<(u32, Vec<PartFile>)> {
        if self.compact_after_files == 0 {
            return None;
        }
        // One file "merging" with itself would rewrite it forever.
        let threshold = self.compact_after_files.max(2);
        let mut by_level: BTreeMap<u32, Vec<PartFile>> = BTreeMap::new();
        for part in Self::partition_parts(dir) {
            by_level.entry(part.level).or_default().push(part);
        }
        by_level
            .into_iter()
            .find(|(_, files)| files.len() >= threshold)
    }

    /// Merge a partition's small files, for a caller that already holds the
    /// exclusive tier lock. See [`compact_partition`](Self::compact_partition).
    ///
    /// # Errors
    ///
    /// As for `compact_partition`.
    pub fn compact_partition_locked(
        &self,
        conn: &Connection,
        site_id: &str,
        date: &str,
    ) -> Result<usize, FlushError> {
        self.compact(conn, None, site_id, date)
    }

    /// Merge a partition's files of one level into a single file of the next.
    ///
    /// A 60-second flush interval writes ~1440 files per site per day, and
    /// DuckDB opens the footer of every matching file on every query.
    ///
    /// The expensive part, reading the sources and writing the merged file,
    /// runs **without** the tier lock. Sources are immutable once published, so
    /// readers and flushes proceed meanwhile. Only the swap takes the
    /// exclusive lock, and it is a handful of renames:
    ///
    /// 1. move each source to `NNNN.parquet.del` (out of the glob, atomic per file);
    /// 2. rename the merged file into place;
    /// 3. fsync the directory, then unlink the `.del` files.
    ///
    /// If any step fails the earlier ones are undone, so a failure never costs
    /// a row (F-01: the previous version deleted the merged file after some
    /// sources were already gone). A crash is resolved by
    /// [`cleanup_temp_files`](Self::cleanup_temp_files).
    ///
    /// The merged file is sorted by `timestamp`, which gives the one column
    /// queries range over useful row-group statistics.
    ///
    /// Returns the number of source files merged (0 if compaction was not due).
    ///
    /// # Errors
    ///
    /// Returns an error if reading, writing, or renaming the Parquet files fails.
    pub fn compact_partition(
        &self,
        conn: &Connection,
        tier: &TierLock,
        site_id: &str,
        date: &str,
    ) -> Result<usize, FlushError> {
        self.compact(conn, Some(tier), site_id, date)
    }

    fn compact(
        &self,
        conn: &Connection,
        tier: Option<&TierLock>,
        site_id: &str,
        date: &str,
    ) -> Result<usize, FlushError> {
        if self.compact_after_files == 0 || !is_safe_path_component(site_id) {
            return Ok(0);
        }
        // A second compaction of any partition waits for the next flush rather
        // than queueing behind this one.
        let Some(_busy) = self.compaction_busy.try_lock() else {
            return Ok(0);
        };
        let dir = self.partition_dir(site_id, date);
        let Some((level, sources)) = self.plan_compaction(&dir) else {
            return Ok(0);
        };

        let staging = dir.join(COMPACT_TMP_NAME);
        let list = sources
            .iter()
            .map(|s| format!("'{}'", sql_quote(&s.path.to_string_lossy())))
            .collect::<Vec<_>>()
            .join(", ");
        Self::stage_parquet(
            conn,
            &format!(
                "SELECT * FROM read_parquet([{list}], union_by_name=true, \
                 hive_partitioning=false) ORDER BY timestamp"
            ),
            &staging,
        )?;

        let moved = {
            let _exclusive = tier.map(TierLock::write);
            self.swap_in(&dir, &staging, &sources, level + 1)?
        };

        for aside in &moved {
            if let Err(e) = fs::remove_file(aside) {
                // Invisible to the glob; startup cleanup retries.
                tracing::warn!(path = %aside.display(), error = %e, "Could not remove a compacted source");
            }
        }
        tracing::info!(
            site_id,
            date,
            merged = sources.len(),
            level = level + 1,
            "Compacted Parquet partition"
        );
        Ok(sources.len())
    }

    /// The exclusive part of a compaction. Returns the moved-aside sources.
    ///
    /// On failure the sources are put back and the staging file is removed. If
    /// putting a source back fails, the staging file is deliberately **kept**:
    /// startup cleanup reads "staging file plus `.del` files" as an interrupted
    /// swap and restores the sources, whereas `.del` files alone mean the swap
    /// finished and are deleted.
    // `self` is read only by the fault-injection hooks compiled into tests.
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn swap_in(
        &self,
        dir: &Path,
        staging: &Path,
        sources: &[PartFile],
        new_level: u32,
    ) -> Result<Vec<PathBuf>, FlushError> {
        // Chosen before anything moves, so the number never repeats one a
        // source just vacated.
        let number = Self::max_file_number(dir) + 1;
        let mut moved: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(sources.len());
        let undo = |moved: &[(PathBuf, PathBuf)]| {
            let mut clean = true;
            for (original, aside) in moved.iter().rev() {
                if let Err(e) = fs::rename(aside, original) {
                    clean = false;
                    tracing::error!(
                        path = %aside.display(), error = %e,
                        "Could not put a compaction source back; its rows are missing from \
                         analytics until startup cleanup renames it back"
                    );
                }
            }
            if clean {
                let _ = fs::remove_file(staging);
            }
        };

        for (index, source) in sources.iter().enumerate() {
            let aside = with_suffix(&source.path, DEL_SUFFIX);
            #[cfg(test)]
            let injected = self.faults.lock().fail_source_move == Some(index);
            #[cfg(not(test))]
            let injected = {
                let _ = index;
                false
            };
            let outcome = if injected {
                Err(std::io::Error::other("injected source move failure"))
            } else {
                fs::rename(&source.path, &aside)
            };
            if let Err(e) = outcome {
                undo(&moved);
                return Err(FlushError::Rename(e));
            }
            moved.push((source.path.clone(), aside));
        }
        #[cfg(test)]
        if self.crash_at(CrashPoint::CompactAfterSourcesMoved) {
            return Err(FlushError::SimulatedCrash);
        }

        let merged = dir.join(format!("{number:04}.m{new_level}.parquet"));
        if let Err(e) = fs::rename(staging, &merged) {
            undo(&moved);
            return Err(FlushError::Rename(e));
        }
        if let Err(e) = sync_dir(dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "Could not fsync after a compaction swap");
        }
        #[cfg(test)]
        if self.crash_at(CrashPoint::CompactAfterPublish) {
            return Err(FlushError::SimulatedCrash);
        }
        Ok(moved.into_iter().map(|(_, aside)| aside).collect())
    }

    /// Delete Parquet partition directories older than `retention_days`.
    ///
    /// Returns the number of partition directories removed. **The caller must
    /// then rebuild the view** (or use
    /// [`cleanup_old_partitions_and_refresh`](Self::cleanup_old_partitions_and_refresh)):
    /// a view over a glob that now matches nothing fails every query (F-05).
    ///
    /// Every partition is attempted even if one fails.
    ///
    /// # Errors
    ///
    /// Returns an error if the base directory cannot be read, or if any
    /// partition could not be removed.
    pub fn cleanup_old_partitions(&self, retention_days: u32) -> std::io::Result<usize> {
        if retention_days == 0 {
            return Ok(0);
        }

        let cutoff =
            chrono::Utc::now().date_naive() - chrono::Duration::days(i64::from(retention_days));
        let cutoff_str = cutoff.format("%Y-%m-%d").to_string();
        let mut removed = 0usize;
        let mut failures: Vec<String> = Vec::new();

        match fs::read_dir(&self.base_dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        }

        for (site_id, date, dir) in self.partition_dirs() {
            // ISO-8601 dates compare correctly as strings.
            if date.as_str() >= cutoff_str.as_str() {
                continue;
            }
            match self.remove_partition_dir(&dir) {
                Ok(()) => removed += 1,
                Err(e) => {
                    tracing::error!(site_id, date, error = %e, "Could not remove an expired partition");
                    failures.push(format!("{site_id}/{date}: {e}"));
                }
            }
        }
        self.remove_empty_site_dirs();

        if failures.is_empty() {
            Ok(removed)
        } else {
            Err(std::io::Error::other(format!(
                "{} expired partition(s) could not be removed: {}",
                failures.len(),
                failures.join("; ")
            )))
        }
    }

    /// [`cleanup_old_partitions`](Self::cleanup_old_partitions) followed by a
    /// rebuild of `events_all`, so removing the last partition cannot leave the
    /// view erroring. The caller holds the exclusive tier lock.
    ///
    /// # Errors
    ///
    /// Returns the cleanup error if there was one, else any view error.
    pub fn cleanup_old_partitions_and_refresh(
        &self,
        conn: &Connection,
        retention_days: u32,
    ) -> std::io::Result<usize> {
        let cleaned = self.cleanup_old_partitions(retention_days);
        if retention_days > 0 {
            // Rebuilt even after a partial failure: some partitions may be gone.
            crate::storage::schema::setup_query_view(conn, &self.base_dir)
                .map_err(std::io::Error::other)?;
        }
        cleaned
    }

    /// Remove a partition directory and everything in it, including staged,
    /// `.del` and temporary files, which can hold the same personal data.
    // `self` is read only by the fault-injection hooks compiled into tests.
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn remove_partition_dir(&self, dir: &Path) -> std::io::Result<()> {
        #[cfg(test)]
        if self.faults.lock().fail_dir_removal {
            return Err(std::io::Error::other("injected removal failure"));
        }
        fs::remove_dir_all(dir)
    }

    /// Remove `site_id=*` directories that no longer contain any partition, so
    /// the site stops being advertised and cleanup stops scanning it (F-10).
    /// `remove_dir` refuses a non-empty directory, which is the safety check.
    fn remove_empty_site_dirs(&self) {
        for site in fs::read_dir(&self.base_dir).into_iter().flatten().flatten() {
            let is_site = site
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("site_id="));
            if is_site && site.path().is_dir() {
                let _ = fs::remove_dir(site.path());
            }
        }
    }

    /// Delete the on-disk partitions for a site across an inclusive date range,
    /// reporting which could not be removed.
    ///
    /// # Errors
    ///
    /// Returns an error if `site_id` is not a safe path component.
    pub fn erase_partitions_report(
        &self,
        site_id: &str,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
    ) -> std::io::Result<ErasureReport> {
        if !is_safe_path_component(site_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unsafe site_id",
            ));
        }
        let mut report = ErasureReport::default();
        let mut current = start;
        while current <= end {
            let date_str = current.format("%Y-%m-%d").to_string();
            let dir = self.partition_dir(site_id, &date_str);
            // `Path::exists` maps every error to `false`, which would report a
            // directory we merely cannot stat as already gone.
            match fs::symlink_metadata(&dir) {
                Ok(_) => match self.remove_partition_dir(&dir) {
                    Ok(()) => {
                        report.removed += 1;
                        tracing::info!(site_id, date = %date_str, "Removed Parquet partition");
                    }
                    Err(e) => {
                        tracing::error!(
                            site_id, date = %date_str, error = %e,
                            "Failed to remove Parquet partition"
                        );
                        report.failed.push((date_str, e));
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => report.failed.push((date_str, e)),
            }
            let Some(next) = current.succ_opt() else {
                break;
            };
            current = next;
        }
        self.remove_empty_site_dirs();
        Ok(report)
    }

    /// Delete the on-disk partitions for a site across an inclusive date range.
    ///
    /// Returns the number of partition directories removed.
    ///
    /// # Errors
    ///
    /// Returns an error if `site_id` is not a safe path component, or if any
    /// partition could not be removed. An erasure that leaves files behind
    /// must not look like success: the data is still on disk (F-09). Use
    /// [`erase_partitions_report`](Self::erase_partitions_report) for the detail.
    pub fn erase_partitions(
        &self,
        site_id: &str,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
    ) -> std::io::Result<u64> {
        let report = self.erase_partitions_report(site_id, start, end)?;
        if report.failed.is_empty() {
            Ok(report.removed)
        } else {
            let detail = report
                .failed
                .iter()
                .map(|(date, e)| format!("{date}: {e}"))
                .collect::<Vec<_>>()
                .join("; ");
            Err(std::io::Error::other(format!(
                "{} partition(s) of {site_id} could not be removed ({} were): {detail}",
                report.failed.len(),
                report.removed
            )))
        }
    }

    /// The published Parquet files of one site for an inclusive date range.
    ///
    /// This is the input to [`scoped_events_sql`]: a request that names its
    /// site and dates can read just those files instead of re-expanding a glob
    /// over every site and day (F-06). Costs one `read_dir` per day in range.
    /// Returns nothing for an unsafe `site_id`.
    ///
    /// [`scoped_events_sql`]: crate::storage::schema::scoped_events_sql
    pub fn files_for_range(
        &self,
        site_id: &str,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
    ) -> Vec<PathBuf> {
        if !is_safe_path_component(site_id) {
            return Vec::new();
        }
        let mut files = Vec::new();
        let mut current = start;
        while current <= end {
            let dir = self.partition_dir(site_id, &current.format("%Y-%m-%d").to_string());
            files.extend(Self::partition_parts(&dir).into_iter().map(|p| p.path));
            let Some(next) = current.succ_opt() else {
                break;
            };
            current = next;
        }
        files
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
    /// A file could not be made durable (`fsync`).
    Durability(std::io::Error),
    /// A test stopped the operation at an injected crash point.
    #[cfg(test)]
    SimulatedCrash,
}

impl std::fmt::Display for FlushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Query(e) => write!(f, "Query error: {e}"),
            Self::Write(e) => write!(f, "Write error: {e}"),
            Self::Delete(e) => write!(f, "Delete error: {e}"),
            Self::Rename(e) => write!(f, "Rename error: {e}"),
            Self::Durability(e) => write!(f, "Durability error: {e}"),
            #[cfg(test)]
            Self::SimulatedCrash => write!(f, "simulated crash"),
        }
    }
}

impl std::error::Error for FlushError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Query(e) | Self::Write(e) | Self::Delete(e) => Some(e),
            Self::Rename(e) | Self::Durability(e) => Some(e),
            #[cfg(test)]
            Self::SimulatedCrash => None,
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
                .compact_partition_locked(&conn, "example.com", "2024-01-15")
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

/// Crash-safety, durability and failure-path tests.
///
/// These use file-backed databases and fresh connections for the "restart",
/// because an in-memory fixture shares one catalog and hides exactly the
/// two-tier inconsistencies under test (L28).
#[cfg(test)]
mod safety_tests {
    use super::*;
    use crate::storage::schema::{ColdTierStatus, cold_tier_status, setup_query_view};

    struct Env {
        _dir: tempfile::TempDir,
        db_path: PathBuf,
        events: PathBuf,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let events = dir.path().join("events");
            fs::create_dir_all(&events).unwrap();
            Self {
                db_path: dir.path().join("t.duckdb"),
                events,
                _dir: dir,
            }
        }

        /// Open the database as a freshly started process would.
        fn open(&self) -> Connection {
            let conn = Connection::open(&self.db_path).unwrap();
            crate::storage::schema::init_schema(&conn).unwrap();
            conn
        }

        fn storage(&self, compact_after: usize) -> ParquetStorage {
            ParquetStorage::new(&self.events, compact_after)
        }
    }

    fn ins(conn: &Connection, site: &str, ts: &str) {
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname) \
             VALUES (?, 'v', CAST(? AS TIMESTAMP), 'pageview', '/')",
            duckdb::params![site, ts],
        )
        .unwrap();
    }

    fn total(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM events_all", [], |r| r.get(0))
            .unwrap()
    }

    fn hot(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    const P: (&str, &str) = ("a.com", "2024-01-15");

    // ---- F-02 / F-03: flush ordering and crash recovery -------------------

    #[test]
    fn test_crash_after_staging_is_recovered_without_duplicates() {
        let env = Env::new();
        {
            let conn = env.open();
            let storage = env.storage(0);
            ins(&conn, P.0, "2024-01-15 10:00:00");
            storage.faults.lock().crash_after = Some(CrashPoint::FlushAfterStage);
            assert!(matches!(
                storage.flush_events(&conn),
                Err(FlushError::SimulatedCrash)
            ));
        } // process dies: connection dropped, nothing cleaned up
        let conn = env.open();
        let storage = env.storage(0);
        let dir = storage.partition_dir(P.0, P.1);
        assert_eq!(names(&dir), ["0001.parquet.pending"]);

        let report = storage.recover_pending_flushes(&conn).unwrap();
        assert_eq!(
            report,
            RecoveryReport {
                published: 0,
                discarded: 1,
                quarantined: 0
            }
        );
        setup_query_view(&conn, &env.events).unwrap();
        assert_eq!(total(&conn), 1, "the row must be counted exactly once");
        assert_eq!(
            hot(&conn),
            1,
            "the delete never committed; the row is still hot"
        );
        assert!(names(&dir).is_empty());
    }

    #[test]
    fn test_crash_after_delete_publishes_the_staged_file() {
        // The dangerous half: the hot rows are gone, so the staged file is the
        // only copy. Recovery must publish it, not clean it up.
        let env = Env::new();
        {
            let conn = env.open();
            let storage = env.storage(0);
            ins(&conn, P.0, "2024-01-15 10:00:00");
            ins(&conn, P.0, "2024-01-15 11:00:00");
            storage.faults.lock().crash_after = Some(CrashPoint::FlushAfterDelete);
            assert!(storage.flush_events(&conn).is_err());
            assert_eq!(hot(&conn), 0);
        }
        let conn = env.open();
        let storage = env.storage(0);
        // The legacy startup cleanup must not touch a staged file.
        storage.cleanup_temp_files().unwrap();
        assert_eq!(
            names(&storage.partition_dir(P.0, P.1)),
            ["0001.parquet.pending"]
        );

        let report = storage.recover_pending_flushes(&conn).unwrap();
        assert_eq!(report.published, 1);
        setup_query_view(&conn, &env.events).unwrap();
        assert_eq!(total(&conn), 2, "no row may be lost");
        assert_eq!(hot(&conn), 0);
        assert_eq!(names(&storage.partition_dir(P.0, P.1)), ["0001.parquet"]);
    }

    #[test]
    fn test_a_staged_files_number_is_never_reused_by_a_later_flush() {
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        ins(&conn, P.0, "2024-01-15 10:00:00");
        storage.faults.lock().crash_after = Some(CrashPoint::FlushAfterDelete);
        let _ = storage.flush_events(&conn);
        storage.faults.lock().crash_after = None;
        // A later flush must not reuse the staged file's number.
        ins(&conn, P.0, "2024-01-15 11:00:00");
        // (A real restart would recover first; here we flush directly to
        // prove numbering alone is collision-free.)
        storage.flush_events(&conn).unwrap();
        let dir = storage.partition_dir(P.0, P.1);
        assert_eq!(names(&dir), ["0001.parquet.pending", "0002.parquet"]);
        let report = storage.recover_pending_flushes(&conn).unwrap();
        assert_eq!(report.published, 1);
        assert_eq!(names(&dir), ["0001.parquet", "0002.parquet"]);
        setup_query_view(&conn, &env.events).unwrap();
        assert_eq!(total(&conn), 2);
    }

    #[test]
    fn test_unreadable_staged_file_is_quarantined_not_deleted_or_published() {
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        let dir = storage.partition_dir(P.0, P.1);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("0001.parquet.pending"), b"not parquet").unwrap();
        let report = storage.recover_pending_flushes(&conn).unwrap();
        assert_eq!(report.quarantined, 1);
        assert_eq!(names(&dir), ["0001.parquet.pending.corrupt"]);
        setup_query_view(&conn, &env.events).unwrap();
        assert_eq!(total(&conn), 0, "a bad file must never join the glob");
    }

    #[test]
    fn test_a_staged_file_is_published_even_if_the_partition_has_new_hot_rows() {
        // If a publish and its reinstate both fail the process keeps running
        // and ingesting. A later restart must not read "the partition has hot
        // rows" as "the staged file is a duplicate" and delete the only copy.
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        ins(&conn, P.0, "2024-01-15 10:00:00");
        storage.faults.lock().crash_after = Some(CrashPoint::FlushAfterDelete);
        let _ = storage.flush_events(&conn);
        ins(&conn, P.0, "2024-01-15 11:00:00"); // arrives afterwards, same partition
        let report = storage.recover_pending_flushes(&conn).unwrap();
        assert_eq!((report.published, report.discarded), (1, 0));
        setup_query_view(&conn, &env.events).unwrap();
        assert_eq!(total(&conn), 2);
    }

    #[test]
    fn test_a_half_written_staged_file_is_dropped_not_quarantined() {
        // A crash during the COPY leaves an unreadable staged file while the
        // hot rows are intact; that is garbage, not evidence.
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        ins(&conn, P.0, "2024-01-15 10:00:00");
        let dir = storage.partition_dir(P.0, P.1);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("0001.parquet.pending"), b"PAR1 truncated").unwrap();
        let report = storage.recover_pending_flushes(&conn).unwrap();
        assert_eq!((report.discarded, report.quarantined), (1, 0));
        assert!(names(&dir).is_empty());
        assert_eq!(hot(&conn), 1);
    }

    #[test]
    fn test_failed_publish_returns_the_rows_to_the_hot_table() {
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        ins(&conn, P.0, "2024-01-15 10:00:00");
        ins(&conn, P.0, "2024-01-15 11:00:00");
        storage.faults.lock().fail_publish = true;
        assert!(matches!(
            storage.flush_events(&conn),
            Err(FlushError::Rename(_))
        ));
        setup_query_view(&conn, &env.events).unwrap();
        assert_eq!(
            hot(&conn),
            2,
            "the rows must be back where queries see them"
        );
        assert_eq!(total(&conn), 2);
        assert!(names(&storage.partition_dir(P.0, P.1)).is_empty());

        storage.faults.lock().fail_publish = false;
        assert_eq!(storage.flush_events(&conn).unwrap(), 2);
        assert_eq!(total(&conn), 2);
    }

    #[test]
    fn test_flush_publishes_and_syncs_a_real_file() {
        // fsync itself is invisible from user space; this at least proves the
        // sync calls succeed on the real files and directory the flush uses.
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        ins(&conn, P.0, "2024-01-15 10:00:00");
        storage.flush_events(&conn).unwrap();
        let dir = storage.partition_dir(P.0, P.1);
        sync_file(&dir.join("0001.parquet")).unwrap();
        sync_dir(&dir).unwrap();
        assert_eq!(names(&dir), ["0001.parquet"]);
    }

    // ---- F-01 / F-07: compaction ------------------------------------------

    fn flush_n(conn: &Connection, storage: &ParquetStorage, hours: std::ops::Range<u32>) {
        for h in hours {
            ins(conn, P.0, &format!("2024-01-15 {h:02}:00:00"));
            storage.flush_events_deferred(conn).unwrap();
        }
    }

    #[test]
    fn test_compaction_that_cannot_move_a_source_loses_no_rows() {
        // Reproduced on the previous code with an immutable second source: the
        // first source was already unlinked, so deleting the merged file lost
        // that source's row (2 of 3 rows visible afterwards).
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(3);
        let tier = TierLock::new();
        setup_query_view(&conn, &env.events).unwrap();
        flush_n(&conn, &storage, 10..13);
        let dir = storage.partition_dir(P.0, P.1);
        let before = names(&dir);
        assert_eq!(before.len(), 3);

        storage.faults.lock().fail_source_move = Some(1);
        assert!(storage.compact_partition(&conn, &tier, P.0, P.1).is_err());
        assert_eq!(
            names(&dir),
            before,
            "sources restored, no merged file, no leftovers"
        );
        assert_eq!(total(&conn), 3);

        storage.faults.lock().fail_source_move = None;
        assert_eq!(
            storage.compact_partition(&conn, &tier, P.0, P.1).unwrap(),
            3
        );
        assert_eq!(total(&conn), 3);
        assert_eq!(names(&dir).len(), 1);
    }

    #[test]
    fn test_crash_with_sources_moved_aside_is_undone_on_restart() {
        let env = Env::new();
        {
            let conn = env.open();
            let storage = env.storage(3);
            flush_n(&conn, &storage, 10..13);
            storage.faults.lock().crash_after = Some(CrashPoint::CompactAfterSourcesMoved);
            assert!(
                storage
                    .compact_partition(&conn, &TierLock::new(), P.0, P.1)
                    .is_err()
            );
        }
        let conn = env.open();
        let storage = env.storage(3);
        let dir = storage.partition_dir(P.0, P.1);
        assert!(names(&dir).iter().any(|n| n.contains(".del")));
        storage.cleanup_temp_files().unwrap();
        assert_eq!(
            names(&dir),
            ["0001.parquet", "0002.parquet", "0003.parquet"]
        );
        setup_query_view(&conn, &env.events).unwrap();
        assert_eq!(total(&conn), 3);
    }

    #[test]
    fn test_crash_after_the_merged_file_is_published_finishes_on_restart() {
        let env = Env::new();
        {
            let conn = env.open();
            let storage = env.storage(3);
            flush_n(&conn, &storage, 10..13);
            storage.faults.lock().crash_after = Some(CrashPoint::CompactAfterPublish);
            assert!(
                storage
                    .compact_partition(&conn, &TierLock::new(), P.0, P.1)
                    .is_err()
            );
        }
        let conn = env.open();
        let storage = env.storage(3);
        let dir = storage.partition_dir(P.0, P.1);
        // Before cleanup the leftovers are already invisible to the glob.
        setup_query_view(&conn, &env.events).unwrap();
        assert_eq!(total(&conn), 3, "never double-counted, even before cleanup");
        assert_eq!(storage.cleanup_temp_files().unwrap(), 3);
        assert_eq!(names(&dir), ["0004.m1.parquet"]);
        assert_eq!(total(&conn), 3);
    }

    #[test]
    fn test_compaction_merges_levels_without_rereading_the_big_file() {
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(3);
        let dir = storage.partition_dir(P.0, P.1);

        flush_n(&conn, &storage, 10..13);
        storage.compact_partition_locked(&conn, P.0, P.1).unwrap();
        assert_eq!(names(&dir), ["0004.m1.parquet"]);
        let first_merge = fs::read(dir.join("0004.m1.parquet")).unwrap();

        // Two more flushes and the level-1 file is not a candidate: nothing at
        // level 0 has reached the threshold, and one level-1 file is alone.
        flush_n(&conn, &storage, 13..15);
        assert_eq!(
            storage.compact_partition_locked(&conn, P.0, P.1).unwrap(),
            0
        );
        flush_n(&conn, &storage, 15..16);
        assert_eq!(
            storage.compact_partition_locked(&conn, P.0, P.1).unwrap(),
            3
        );
        // The first merged file is byte-for-byte what it was: never re-read.
        assert_eq!(fs::read(dir.join("0004.m1.parquet")).unwrap(), first_merge);
        assert_eq!(names(&dir).len(), 2);

        // A third group of three makes three level-1 files, which merge.
        flush_n(&conn, &storage, 16..19);
        storage.compact_partition_locked(&conn, P.0, P.1).unwrap();
        let after = names(&dir);
        assert_eq!(after.len(), 3, "{after:?}");
        flush_n(&conn, &storage, 19..22);
        // Level 0 first (one group per call), leaving four level-1 files...
        assert_eq!(
            storage.compact_partition_locked(&conn, P.0, P.1).unwrap(),
            3
        );
        assert_eq!(names(&dir).len(), 4, "{:?}", names(&dir));
        // ...which then merge into one level-2 file.
        assert_eq!(
            storage.compact_partition_locked(&conn, P.0, P.1).unwrap(),
            4
        );
        assert_eq!(names(&dir).len(), 1, "{:?}", names(&dir));
        assert!(names(&dir)[0].contains(".m2."));
        setup_query_view(&conn, &env.events).unwrap();
        assert_eq!(total(&conn), 12, "no row lost across two levels");
    }

    #[test]
    fn test_compacted_file_is_sorted_by_timestamp() {
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(3);
        // Flushed out of order on purpose.
        for h in [15u32, 9, 12] {
            ins(&conn, P.0, &format!("2024-01-15 {h:02}:00:00"));
            storage.flush_events_deferred(&conn).unwrap();
        }
        storage.compact_partition_locked(&conn, P.0, P.1).unwrap();
        let file = storage.partition_dir(P.0, P.1).join("0004.m1.parquet");
        let mut stmt = conn
            .prepare(&format!(
                "SELECT STRFTIME(timestamp, '%H') FROM read_parquet('{}')",
                file.display()
            ))
            .unwrap();
        let hours: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(hours, ["09", "12", "15"]);
    }

    #[test]
    fn test_compaction_does_not_hold_the_tier_lock_while_merging() {
        // F-07: the merge reads and rewrites the whole level. It must run with
        // a reader holding the lock, and block only at the swap.
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(3);
        let tier = TierLock::new();
        flush_n(&conn, &storage, 10..13);
        let dir = storage.partition_dir(P.0, P.1);

        let reader = tier.read();
        let worker = {
            let (storage, tier) = (storage, tier.clone());
            let conn = conn.try_clone().unwrap();
            std::thread::spawn(move || storage.compact_partition(&conn, &tier, P.0, P.1))
        };
        // The merged file is staged while we still hold the shared lock, and
        // nothing has moved yet: only the swap needs exclusivity.
        let staged = dir.join(COMPACT_TMP_NAME);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !staged.exists() {
            assert!(std::time::Instant::now() < deadline, "merge never finished");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!worker.is_finished(), "the swap must wait for the reader");
        assert_eq!(
            names(&dir),
            [
                "0001.parquet",
                "0002.parquet",
                "0003.parquet",
                COMPACT_TMP_NAME
            ]
        );
        drop(reader);
        assert_eq!(worker.join().unwrap().unwrap(), 3);
        assert_eq!(names(&dir), ["0004.m1.parquet"]);
    }

    #[test]
    fn test_compaction_never_resurrects_an_erased_partition() {
        // Erasure removes the directory while a merge is staged elsewhere. The
        // swap must notice the sources are gone instead of publishing a merged
        // copy of data that was just erased.
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(3);
        flush_n(&conn, &storage, 10..13);
        let dir = storage.partition_dir(P.0, P.1);
        let sources = ParquetStorage::partition_parts(&dir);
        ParquetStorage::stage_parquet(
            &conn,
            &format!(
                "SELECT * FROM read_parquet('{}')",
                sources[0].path.display()
            ),
            &dir.join(COMPACT_TMP_NAME),
        )
        .unwrap();
        fs::remove_file(&sources[1].path).unwrap();
        let outcome = storage.swap_in(&dir, &dir.join(COMPACT_TMP_NAME), &sources, 1);
        assert!(outcome.is_err());
        assert!(
            names(&dir).iter().all(|n| !n.contains(".m1.")),
            "no merged file may be published: {:?}",
            names(&dir)
        );
    }

    #[test]
    fn test_cleanup_deletes_a_lone_temp_file_and_keeps_pending() {
        let env = Env::new();
        let storage = env.storage(0);
        let dir = storage.partition_dir(P.0, P.1);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("0001.parquet"), b"x").unwrap();
        fs::write(dir.join("compact.parquet.tmp"), b"x").unwrap();
        fs::write(dir.join("0002.parquet.pending"), b"x").unwrap();
        assert_eq!(storage.cleanup_temp_files().unwrap(), 1);
        assert_eq!(names(&dir), ["0001.parquet", "0002.parquet.pending"]);
    }

    // ---- F-04 / F-05 / F-11: the view -------------------------------------

    #[test]
    fn test_unmatched_glob_error_has_the_expected_text() {
        // setup_query_view falls back to hot-only only on this message.
        let conn = Connection::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = conn
            .execute_batch(&format!(
                "SELECT * FROM read_parquet('{}/site_id=*/date=*/*.parquet')",
                dir.path().display()
            ))
            .unwrap_err();
        assert!(
            err.to_string()
                .contains(crate::storage::schema::NO_FILES_MARKER_FOR_TESTS),
            "{err}"
        );
    }

    #[test]
    fn test_corrupt_file_is_an_error_not_a_silent_hot_only_view() {
        // Reproduced on the previous code: setup_query_view returned Ok(()) and
        // every Parquet event vanished from analytics.
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        ins(&conn, P.0, "2024-01-15 10:00:00");
        storage.flush_events(&conn).unwrap();
        ins(&conn, P.0, "2024-01-15 11:00:00");
        let bad = storage.partition_dir(P.0, P.1).join("0009.parquet");
        fs::write(&bad, b"junk").unwrap();

        assert!(setup_query_view(&conn, &env.events).is_err());
        assert_eq!(
            cold_tier_status(&conn, &env.events),
            ColdTierStatus::Degraded
        );
        // The hot tier still answers while the operator fixes the file.
        assert_eq!(total(&conn), 1);

        fs::remove_file(&bad).unwrap();
        setup_query_view(&conn, &env.events).unwrap();
        assert_eq!(
            cold_tier_status(&conn, &env.events),
            ColdTierStatus::Healthy
        );
        assert_eq!(total(&conn), 2);
    }

    #[test]
    fn test_empty_cold_tier_is_not_an_error() {
        let env = Env::new();
        let conn = env.open();
        setup_query_view(&conn, &env.events).unwrap();
        assert_eq!(cold_tier_status(&conn, &env.events), ColdTierStatus::Empty);
        assert_eq!(total(&conn), 0);
    }

    #[test]
    fn test_partial_flush_failure_still_refreshes_the_view() {
        // F-04: the view was built while the cold tier was empty. Partition A
        // is flushed (and deleted from the hot table), partition B then fails.
        // The old code skipped the rebuild on that error path, leaving A in
        // neither tier as far as queries could tell.
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        setup_query_view(&conn, &env.events).unwrap();
        assert_eq!(cold_tier_status(&conn, &env.events), ColdTierStatus::Empty);

        // A has more rows so it is flushed first.
        ins(&conn, "a.com", "2024-01-15 10:00:00");
        ins(&conn, "a.com", "2024-01-15 11:00:00");
        ins(&conn, "b.com", "2024-01-15 10:00:00");
        // Make B impossible to write: a plain file squats on its site directory.
        fs::write(env.events.join("site_id=b.com"), b"squatter").unwrap();

        assert!(storage.flush_events(&conn).is_err());
        assert_eq!(hot(&conn), 1, "only b.com is still hot");
        assert_eq!(
            total(&conn),
            3,
            "a.com's rows must be visible through events_all"
        );
        assert_eq!(
            cold_tier_status(&conn, &env.events),
            ColdTierStatus::Healthy
        );
    }

    #[test]
    fn test_retention_removing_everything_leaves_a_working_view() {
        // Reproduced: querying events_all after retention emptied the tree
        // failed with "No files found that match the pattern".
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        ins(&conn, P.0, "2020-01-15 10:00:00");
        storage.flush_events(&conn).unwrap();
        assert_eq!(total(&conn), 1);

        assert_eq!(
            storage
                .cleanup_old_partitions_and_refresh(&conn, 30)
                .unwrap(),
            1
        );
        assert_eq!(total(&conn), 0);
        assert_eq!(cold_tier_status(&conn, &env.events), ColdTierStatus::Empty);

        // And the cold tier comes back when data does.
        ins(&conn, P.0, "2020-01-15 10:00:00");
        storage.flush_events(&conn).unwrap();
        assert_eq!(total(&conn), 1);
        assert_eq!(
            cold_tier_status(&conn, &env.events),
            ColdTierStatus::Healthy
        );
    }

    #[test]
    fn test_flush_does_not_rebuild_a_view_that_already_reads_parquet() {
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        ins(&conn, P.0, "2024-01-15 10:00:00");
        storage.flush_events(&conn).unwrap();
        // Poison the definition in a way a rebuild would repair.
        conn.execute_batch(&format!(
            "CREATE OR REPLACE VIEW events_all AS SELECT * FROM events UNION ALL \
             SELECT * FROM read_parquet('{}/site_id=*/date=*/*.parquet', hive_partitioning=false)",
            env.events.display()
        ))
        .unwrap();
        ins(&conn, P.0, "2024-01-15 11:00:00");
        storage.flush_events(&conn).unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM duckdb_views() WHERE view_name = 'events_all'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !sql.contains("union_by_name"),
            "the view was rebuilt: {sql}"
        );
        assert_eq!(total(&conn), 2);
    }

    #[test]
    fn test_data_dir_with_glob_characters_is_rejected() {
        for bad in ["/data/[prod]", "/data/ev*nts", "/data/what?"] {
            assert!(validate_data_dir(Path::new(bad)).is_err(), "{bad}");
        }
        assert!(validate_data_dir(Path::new("/data/events-2024_a.b")).is_ok());
        assert!(validate_data_dir(Path::new("/data/it's")).is_ok());

        // And the view refuses rather than silently matching nothing.
        let parent = tempfile::tempdir().unwrap();
        let odd = parent.path().join("[prod]");
        fs::create_dir_all(&odd).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        let err = setup_query_view(&conn, &odd).unwrap_err();
        assert!(err.to_string().contains("glob character"), "{err}");
        assert_eq!(
            total(&conn),
            0,
            "hot-only view installed so ingest keeps working"
        );
    }

    // ---- F-09 / F-10: erasure and retention -------------------------------

    #[test]
    fn test_erasure_does_not_report_success_when_a_partition_remains() {
        // Reproduced with an immutable file: erase_partitions returned Ok(0)
        // while the personal data was still on disk.
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        ins(&conn, P.0, "2024-01-15 10:00:00");
        storage.flush_events(&conn).unwrap();
        let day = chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();

        storage.faults.lock().fail_dir_removal = true;
        let err = storage.erase_partitions(P.0, day, day).unwrap_err();
        assert!(err.to_string().contains("2024-01-15"), "{err}");
        let report = storage.erase_partitions_report(P.0, day, day).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.failed.len(), 1);
        assert!(storage.partition_dir(P.0, P.1).exists());

        storage.faults.lock().fail_dir_removal = false;
        assert_eq!(storage.erase_partitions(P.0, day, day).unwrap(), 1);
        assert!(!storage.partition_dir(P.0, P.1).exists());
    }

    #[test]
    fn test_erasure_removes_staged_and_aside_files_with_the_partition() {
        let env = Env::new();
        let storage = env.storage(0);
        let dir = storage.partition_dir(P.0, P.1);
        fs::create_dir_all(&dir).unwrap();
        for n in ["0001.parquet", "0002.parquet.pending", "0003.parquet.del"] {
            fs::write(dir.join(n), b"x").unwrap();
        }
        let day = chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        assert_eq!(storage.erase_partitions(P.0, day, day).unwrap(), 1);
        assert!(!dir.exists());
    }

    #[test]
    fn test_emptied_site_directories_are_removed_but_populated_ones_stay() {
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        ins(&conn, "old.com", "2020-01-15 10:00:00");
        ins(&conn, "mixed.com", "2020-01-15 10:00:00");
        let recent = chrono::Utc::now().format("%Y-%m-%d").to_string();
        ins(&conn, "mixed.com", &format!("{recent} 10:00:00"));
        storage.flush_events(&conn).unwrap();
        assert_eq!(storage.known_site_ids(), ["mixed.com", "old.com"]);

        assert_eq!(storage.cleanup_old_partitions(30).unwrap(), 2);
        assert_eq!(storage.known_site_ids(), ["mixed.com"]);

        let day = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        let today = chrono::Utc::now().date_naive();
        storage.erase_partitions("mixed.com", day, today).unwrap();
        assert!(storage.known_site_ids().is_empty());
    }

    // ---- SI-08: bounded flush ---------------------------------------------

    #[test]
    fn test_flush_is_capped_and_the_remainder_is_never_lost() {
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0).with_max_partitions_per_flush(2);
        setup_query_view(&conn, &env.events).unwrap();
        // One busy site and four one-event fakes.
        for _ in 0..5 {
            ins(&conn, "real.com", "2024-01-15 10:00:00");
        }
        for i in 0..4 {
            ins(&conn, &format!("fake{i}.com"), "2024-01-15 10:00:00");
        }

        let report = storage.flush_events_deferred(&conn).unwrap();
        assert_eq!(report.touched.len(), 2);
        assert_eq!(report.deferred_partitions, 3);
        assert!(
            report
                .touched
                .contains(&("real.com".to_string(), "2024-01-15".to_string())),
            "the busiest partition goes first: {:?}",
            report.touched
        );
        assert_eq!(report.flushed, 6);
        assert_eq!(
            total(&conn),
            9,
            "deferred rows stay visible in the hot tier"
        );

        let mut rounds = 0;
        while hot(&conn) > 0 {
            storage.flush_events_deferred(&conn).unwrap();
            rounds += 1;
            assert!(rounds < 10);
        }
        assert_eq!(rounds, 2);
        assert_eq!(total(&conn), 9);
    }

    #[test]
    fn test_partition_cap_of_zero_still_drains() {
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0).with_max_partitions_per_flush(0);
        ins(&conn, "a.com", "2024-01-15 10:00:00");
        assert_eq!(storage.flush_events_deferred(&conn).unwrap().flushed, 1);
    }

    // ---- F-06: scoped reads ------------------------------------------------

    #[test]
    fn test_scoped_source_returns_exactly_what_the_global_view_does() {
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        for (site, day) in [("a.com", 14), ("a.com", 15), ("a.com", 16), ("b.com", 15)] {
            ins(&conn, site, &format!("2024-01-{day} 10:00:00"));
            ins(&conn, site, &format!("2024-01-{day} 23:30:00"));
            storage.flush_events(&conn).unwrap();
        }
        // A hot row inside the range, and one for another site.
        ins(&conn, "a.com", "2024-01-15 12:00:00");
        ins(&conn, "b.com", "2024-01-15 12:00:00");
        setup_query_view(&conn, &env.events).unwrap();

        let d = |n| chrono::NaiveDate::from_ymd_opt(2024, 1, n).unwrap();
        let files = storage.files_for_range("a.com", d(15), d(16));
        assert_eq!(files.len(), 2);
        let scoped = crate::storage::schema::scoped_events_sql(&files);

        let fingerprint = |source: &str| -> Vec<(String, String)> {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT site_id, CAST(timestamp AS VARCHAR) FROM {source} \
                     WHERE site_id = ? AND timestamp >= CAST(? AS TIMESTAMP) \
                     AND timestamp < CAST(? AS TIMESTAMP) ORDER BY timestamp"
                ))
                .unwrap();
            stmt.query_map(["a.com", "2024-01-15", "2024-01-17"], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
        };
        let global = fingerprint("events_all");
        assert_eq!(global.len(), 5, "{global:?}");
        assert_eq!(fingerprint(&scoped), global);

        // No files in range: the hot tier alone.
        let none = crate::storage::schema::scoped_events_sql(&[]);
        let hot_only: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {none}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(hot_only, hot(&conn));
    }

    #[test]
    fn test_scoped_files_ignore_unsafe_sites_and_staged_files() {
        let env = Env::new();
        let storage = env.storage(0);
        let dir = storage.partition_dir(P.0, P.1);
        fs::create_dir_all(&dir).unwrap();
        for n in ["0001.parquet", "0002.parquet.pending", "0003.parquet.del"] {
            fs::write(dir.join(n), b"x").unwrap();
        }
        let day = chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        assert_eq!(storage.files_for_range(P.0, day, day).len(), 1);
        assert!(storage.files_for_range("../etc", day, day).is_empty());
    }

    /// Timing harness for F-07, not a correctness test: `cargo test --lib
    /// compaction_lock_hold -- --ignored --nocapture`. Compares how long the
    /// exclusive tier lock is held by (a) the old design, which re-reads and
    /// rewrites the whole day under the lock, with (b) the new one, which
    /// merges only the 23 small files outside the lock and holds it for the
    /// rename swap alone. Debug build: compare the ratio, not the absolute ms.
    #[test]
    #[ignore = "timing harness"]
    fn compaction_lock_hold_old_vs_new() {
        use std::time::Instant;
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(24);
        let dir = storage.partition_dir(P.0, P.1);
        fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, rows: u32| {
            conn.execute_batch(&format!(
                "COPY (SELECT 'a.com' AS site_id, 'v' || (i % 997) AS visitor_id, \
                 TIMESTAMP '2024-01-15 00:00:00' + INTERVAL (i % 86400) SECOND AS timestamp, \
                 'pageview' AS event_name, '/p' || (i % 50) AS pathname, \
                 CAST(NULL AS VARCHAR) AS hostname, CAST(NULL AS VARCHAR) AS referrer, \
                 CAST(NULL AS VARCHAR) AS referrer_source, CAST(NULL AS VARCHAR) AS utm_source, \
                 CAST(NULL AS VARCHAR) AS utm_medium, CAST(NULL AS VARCHAR) AS utm_campaign, \
                 CAST(NULL AS VARCHAR) AS utm_content, CAST(NULL AS VARCHAR) AS utm_term, \
                 'Chrome' AS browser, CAST(NULL AS VARCHAR) AS browser_version, \
                 'Linux' AS os, CAST(NULL AS VARCHAR) AS os_version, \
                 'desktop' AS device_type, CAST(NULL AS VARCHAR) AS screen_size, \
                 'US' AS country_code, CAST(NULL AS VARCHAR) AS region, \
                 CAST(NULL AS VARCHAR) AS city, CAST(NULL AS VARCHAR) AS props, \
                 CAST(NULL AS DECIMAL(12,2)) AS revenue_amount, \
                 CAST(NULL AS VARCHAR) AS revenue_currency FROM range({rows}) t(i)) \
                 TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
                dir.join(name).display()
            ))
            .unwrap();
        };
        let big_rows: u32 = std::env::var("BIG_ROWS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000_000);
        // A day already compacted once: one big file at level 1, plus 24 fresh flushes.
        write("0001.m1.parquet", big_rows);
        for n in 2..26 {
            write(&format!("{n:04}.parquet"), 2_000);
        }
        let mut old_ms = Vec::new();
        let mut new_hold_ms = Vec::new();
        let mut new_total_ms = Vec::new();
        for _ in 0..3 {
            // (a) old: everything in the partition, unsorted, under the lock.
            let t = Instant::now();
            let tmp = dir.join("old.parquet.tmp");
            ParquetStorage::stage_parquet(
                &conn,
                &format!(
                    "SELECT * FROM read_parquet('{}/*.parquet', union_by_name=true, hive_partitioning=false)",
                    dir.display()
                ),
                &tmp,
            )
            .unwrap();
            old_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            fs::remove_file(&tmp).unwrap();

            // (b) new: merge level 0 outside the lock, swap under it.
            let parts = ParquetStorage::partition_parts(&dir);
            let sources: Vec<PartFile> = parts.into_iter().filter(|p| p.level == 0).collect();
            assert_eq!(sources.len(), 24);
            let t_total = Instant::now();
            let list = sources
                .iter()
                .map(|s| format!("'{}'", s.path.display()))
                .collect::<Vec<_>>()
                .join(", ");
            let staging = dir.join(COMPACT_TMP_NAME);
            ParquetStorage::stage_parquet(
                &conn,
                &format!("SELECT * FROM read_parquet([{list}], union_by_name=true, hive_partitioning=false) ORDER BY timestamp"),
                &staging,
            )
            .unwrap();
            let t_hold = Instant::now();
            let moved = storage.swap_in(&dir, &staging, &sources, 1).unwrap();
            new_hold_ms.push(t_hold.elapsed().as_secs_f64() * 1000.0);
            new_total_ms.push(t_total.elapsed().as_secs_f64() * 1000.0);
            for m in moved {
                fs::remove_file(m).unwrap();
            }
            // Restore the 24-small-file state for the next round.
            let merged: Vec<_> = ParquetStorage::partition_parts(&dir)
                .into_iter()
                .filter(|p| p.level == 1 && p.number > 1)
                .collect();
            for m in merged {
                fs::remove_file(m.path).unwrap();
            }
            for n in 2..26 {
                write(&format!("{n:04}.parquet"), 2_000);
            }
        }
        eprintln!("big file rows={big_rows}, 24 small files x 2000 rows (debug build)");
        eprintln!("OLD exclusive-lock hold (rewrite whole day): {old_ms:.0?} ms");
        eprintln!("NEW merge outside lock total:                {new_total_ms:.0?} ms");
        eprintln!("NEW exclusive-lock hold (renames):           {new_hold_ms:.2?} ms");
    }

    #[test]
    fn test_footer_cache_notices_a_replaced_file() {
        // F-06: the footer cache must never serve a stale footer. Replace a
        // cached file in place, with different content and size, and then
        // with different content of the same size.
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(0);
        ins(&conn, P.0, "2024-01-15 10:00:00");
        storage.flush_events(&conn).unwrap();
        let file = storage.partition_dir(P.0, P.1).join("0001.parquet");
        setup_query_view(&conn, &env.events).unwrap();
        let setting: String = conn
            .query_row(
                "SELECT CAST(current_setting('parquet_metadata_cache') AS VARCHAR)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(setting, "true");
        assert_eq!(total(&conn), 1);
        assert_eq!(total(&conn), 1, "second query served from the cache");

        let write = |rows: u32, minute: u32| {
            let _ = fs::remove_file(&file);
            conn.execute_batch(&format!(
                "COPY (SELECT 'a.com' AS site_id, 'v' AS visitor_id, \
                 TIMESTAMP '2024-01-15 10:{minute:02}:00' + INTERVAL (i) SECOND AS timestamp, \
                 'pageview' AS event_name, '/' AS pathname, \
                 CAST(NULL AS VARCHAR) AS hostname, CAST(NULL AS VARCHAR) AS referrer, \
                 CAST(NULL AS VARCHAR) AS referrer_source, CAST(NULL AS VARCHAR) AS utm_source, \
                 CAST(NULL AS VARCHAR) AS utm_medium, CAST(NULL AS VARCHAR) AS utm_campaign, \
                 CAST(NULL AS VARCHAR) AS utm_content, CAST(NULL AS VARCHAR) AS utm_term, \
                 CAST(NULL AS VARCHAR) AS browser, CAST(NULL AS VARCHAR) AS browser_version, \
                 CAST(NULL AS VARCHAR) AS os, CAST(NULL AS VARCHAR) AS os_version, \
                 CAST(NULL AS VARCHAR) AS device_type, CAST(NULL AS VARCHAR) AS screen_size, \
                 CAST(NULL AS VARCHAR) AS country_code, CAST(NULL AS VARCHAR) AS region, \
                 CAST(NULL AS VARCHAR) AS city, CAST(NULL AS VARCHAR) AS props, \
                 CAST(NULL AS DECIMAL(12,2)) AS revenue_amount, \
                 CAST(NULL AS VARCHAR) AS revenue_currency FROM range({rows}) t(i)) \
                 TO '{}' (FORMAT PARQUET)",
                file.display()
            ))
            .unwrap();
        };
        write(5, 0);
        assert_eq!(total(&conn), 5, "a different-size replacement was missed");
        // Same size, different content, written within the same second.
        write(5, 30);
        let min: String = conn
            .query_row(
                "SELECT CAST(MIN(timestamp) AS VARCHAR) FROM events_all",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            min, "2024-01-15 10:30:00",
            "a same-size replacement was missed"
        );
    }

    #[test]
    fn test_hive_view_returns_exactly_what_the_plain_glob_does() {
        // F-06: the view reads directory keys as hive columns for pruning. It
        // must return the same rows, columns and values as the plain glob it
        // replaced, including compacted files and a hot tier.
        let env = Env::new();
        let conn = env.open();
        let storage = env.storage(3);
        for (site, day, h) in [
            ("a.com", 14, 9),
            ("a.com", 14, 10),
            ("a.com", 14, 11),
            ("a.com", 15, 9),
            ("b.com", 15, 9),
            ("b.com", 16, 9),
            ("c-1.org", 16, 23),
        ] {
            ins(&conn, site, &format!("2024-01-{day} {h:02}:00:00"));
            storage.flush_events(&conn).unwrap();
        }
        ins(&conn, "a.com", "2024-01-15 12:00:00"); // hot
        setup_query_view(&conn, &env.events).unwrap();

        let columns = crate::storage::schema::EVENT_COLUMNS.join(", ");
        let plain = format!(
            "SELECT {columns} FROM events UNION ALL SELECT {columns} FROM \
             read_parquet('{}/site_id=*/date=*/*.parquet', union_by_name=true, \
             hive_partitioning=false)",
            env.events.display()
        );
        let dump = |source: &str, filter: &str| -> Vec<String> {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT CAST(t AS VARCHAR) FROM (SELECT * FROM {source}) t {filter} ORDER BY 1"
                ))
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let view_cols: Vec<String> = conn
            .prepare("SELECT column_name FROM (DESCRIBE events_all)")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            view_cols,
            crate::storage::schema::EVENT_COLUMNS,
            "no hive column may leak"
        );
        let want = dump(&format!("({plain})"), "");
        assert_eq!(want.len(), 8);
        assert_eq!(dump("events_all", ""), want);
        for site in ["a.com", "b.com", "c-1.org", "nope.com"] {
            let filter = format!("WHERE t.site_id = '{site}'");
            assert_eq!(
                dump("events_all", &filter),
                dump(&format!("({plain})"), &filter),
                "{site}"
            );
        }
    }
}
