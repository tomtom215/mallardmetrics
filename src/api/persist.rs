//! A list of records kept in memory and mirrored to one JSON file.
//!
//! Share links and saved segments use the persistence pattern of
//! [`ApiKeyStore`](super::auth::ApiKeyStore) -- atomic, owner-only writes, stale
//! temporary files removed at start-up, an unreadable file moved aside rather
//! than overwritten -- so the pattern lives here once instead of being copied
//! per store.
//!
//! Unlike the key store, a mutation is **persisted before it is published**:
//! the closure edits a copy of the list, the copy is written, and only then does
//! it replace the live list. A failed write therefore leaves memory and disk
//! exactly as they were, with nothing to roll back.

use super::auth::{remove_stale_temp_files, restrict_to_owner, write_private_file};
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::PathBuf;
use std::sync::Arc;

/// `bytes` random bytes from the operating system's CSPRNG, hex encoded.
///
/// Read straight from [`SysRng`](rand::rngs::SysRng) (`getrandom(2)` and its
/// equivalents) rather than a user-space generator seeded from it, because the
/// value is a credential. Fails closed: an unreadable entropy source is an
/// error, never a weaker value.
///
/// # Errors
///
/// An I/O error when the operating system cannot supply randomness.
pub fn random_hex(bytes: usize) -> std::io::Result<String> {
    use rand::TryRng;
    let mut buf = vec![0_u8; bytes];
    rand::rngs::SysRng
        .try_fill_bytes(&mut buf)
        .map_err(std::io::Error::other)?;
    Ok(hex::encode(buf))
}

/// The `500` for a store that could not be written. The change was not applied,
/// so the response and the stored state agree.
pub fn storage_unwritable(action: &str, error: &std::io::Error) -> axum::response::Response {
    use axum::response::IntoResponse;
    tracing::error!(error = %error, action, "Could not persist a store file");
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({
            "code": "internal_error",
            "error": format!(
                "Could not {action}: the data directory is not writable. Check its \
                 permissions and free space, then try again. Nothing was changed."
            )
        })),
    )
        .into_response()
}

/// Records mirrored to a JSON file (or held in memory only, for tests).
#[derive(Clone)]
pub struct JsonList<T> {
    items: Arc<Mutex<Vec<T>>>,
    path: Option<Arc<PathBuf>>,
    /// Serialises writers, so the last file written is the newest state. Held
    /// across the whole copy-edit-write-publish sequence.
    writer: Arc<Mutex<()>>,
}

impl<T> Default for JsonList<T> {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl<T> JsonList<T> {
    /// A list that is never written to disk.
    pub fn in_memory() -> Self {
        Self {
            items: Arc::new(Mutex::new(Vec::new())),
            path: None,
            writer: Arc::new(Mutex::new(())),
        }
    }

    /// Inspect the live records under the lock. Keep `f` short.
    pub fn read<R>(&self, f: impl FnOnce(&[T]) -> R) -> R {
        f(&self.items.lock())
    }
}

impl<T: Clone + Serialize + DeserializeOwned> JsonList<T> {
    /// Load the records at `path`, or start empty.
    ///
    /// A missing file is an empty list. A file that cannot be parsed is moved
    /// aside (`<name>.json.corrupt-<unix time>`) and the list starts empty: the
    /// next write would otherwise overwrite the only copy. For share links that
    /// fails closed -- an unreadable store authorises nothing.
    pub fn load(path: PathBuf, what: &str) -> Self {
        remove_stale_temp_files(&path);
        restrict_to_owner(&path);
        let items = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str::<Vec<T>>(&contents).unwrap_or_else(|e| {
                let aside =
                    path.with_extension(format!("json.corrupt-{}", chrono::Utc::now().timestamp()));
                match std::fs::rename(&path, &aside) {
                    Ok(()) => tracing::error!(
                        path = %path.display(),
                        moved_to = %aside.display(),
                        error = %e,
                        "Failed to parse {what}; moved it aside and starting empty"
                    ),
                    Err(rename_error) => tracing::error!(
                        path = %path.display(),
                        error = %e,
                        rename_error = %rename_error,
                        "Failed to parse {what} and could not move it aside; starting empty"
                    ),
                }
                Vec::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Could not read {what}; starting empty"
                );
                Vec::new()
            }
        };
        Self {
            items: Arc::new(Mutex::new(items)),
            path: Some(Arc::new(path)),
            writer: Arc::new(Mutex::new(())),
        }
    }

    /// A copy of every record.
    pub fn snapshot(&self) -> Vec<T> {
        self.items.lock().clone()
    }

    /// Edit the list and persist the result.
    ///
    /// `f` runs on a copy. If it returns `Err`, or the write fails, the live
    /// list is untouched. Blocks on file I/O, so async callers use
    /// `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// Whatever `f` returns, or the I/O error from writing the file.
    pub fn update<R, E: From<std::io::Error>>(
        &self,
        f: impl FnOnce(&mut Vec<T>) -> Result<R, E>,
    ) -> Result<R, E> {
        let _serialised = self.writer.lock();
        let mut next = self.items.lock().clone();
        let out = f(&mut next)?;
        self.write(&next)?;
        *self.items.lock() = next;
        Ok(out)
    }

    /// Edit the live list in memory only (a revocation that must not wait for
    /// the disk). Nothing is written until the next [`update`](Self::update).
    ///
    /// Takes the writer lock, so it cannot land between an `update`'s copy and
    /// its publish and be overwritten by it.
    pub fn update_in_memory<R>(&self, f: impl FnOnce(&mut Vec<T>) -> R) -> R {
        let _serialised = self.writer.lock();
        f(&mut self.items.lock())
    }

    fn write(&self, items: &[T]) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let json = serde_json::to_vec_pretty(items).map_err(std::io::Error::other)?;
        write_private_file(path.as_ref(), &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct Row {
        id: u32,
    }

    #[derive(Debug)]
    enum TestError {
        Io,
        Rejected,
    }
    impl From<std::io::Error> for TestError {
        fn from(_: std::io::Error) -> Self {
            Self::Io
        }
    }

    #[test]
    fn test_update_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rows.json");
        let list = JsonList::<Row>::load(path.clone(), "rows.json");
        list.update(|v| {
            v.push(Row { id: 7 });
            Ok::<_, TestError>(())
        })
        .unwrap();
        let again = JsonList::<Row>::load(path, "rows.json");
        assert_eq!(again.snapshot(), vec![Row { id: 7 }]);
    }

    #[cfg(unix)]
    #[test]
    fn test_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rows.json");
        let list = JsonList::<Row>::load(path.clone(), "rows.json");
        list.update(|v| {
            v.push(Row { id: 1 });
            Ok::<_, TestError>(())
        })
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn test_corrupt_file_is_moved_aside_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rows.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        let list = JsonList::<Row>::load(path.clone(), "rows.json");
        assert!(list.snapshot().is_empty());
        assert!(!path.exists(), "the corrupt file must be moved away");
        let aside: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("corrupt"))
            .collect();
        assert_eq!(aside.len(), 1);
        assert_eq!(
            std::fs::read(aside[0].path()).unwrap(),
            b"{ this is not json"
        );
    }

    #[test]
    fn test_stale_temp_files_are_removed_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rows.json");
        let stale = dir.path().join("rows.tmp.99999.0");
        std::fs::write(&stale, b"left by a crash").unwrap();
        let _ = JsonList::<Row>::load(path, "rows.json");
        assert!(!stale.exists());
    }

    #[test]
    fn test_failed_closure_leaves_the_list_unchanged() {
        let list = JsonList::<Row>::in_memory();
        let result = list.update(|v| {
            v.push(Row { id: 1 });
            Err::<(), _>(TestError::Rejected)
        });
        assert!(matches!(result, Err(TestError::Rejected)));
        assert!(list.snapshot().is_empty());
    }

    #[test]
    fn test_failed_write_leaves_the_list_unchanged() {
        // A path below a regular file cannot be created, so the write fails.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let list = JsonList::<Row>::load(blocker.join("rows.json"), "rows.json");
        let result = list.update(|v| {
            v.push(Row { id: 1 });
            Ok::<_, TestError>(())
        });
        assert!(matches!(result, Err(TestError::Io)));
        assert!(
            list.snapshot().is_empty(),
            "a record that could not be stored must not be live"
        );
    }
}
