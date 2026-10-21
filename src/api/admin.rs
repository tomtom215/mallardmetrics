//! Operator actions that act on the whole instance.

use crate::api::errors::{ApiError, ApiQuery};
use crate::ingest::handler::AppState;
use crate::storage::backup::{self, BackupError};
use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;
use std::sync::Arc;

/// Query parameters for [`create_backup`].
#[derive(Debug, Deserialize)]
pub struct BackupParams {
    /// Name of the backup directory. Default `mallard-<UTC timestamp>`.
    ///
    /// Only a name, never a path: backups are always written under
    /// `<data_dir>/backups`, so this endpoint cannot be used to write elsewhere.
    pub name: Option<String>,
}

/// POST /api/admin/backup — snapshot both storage tiers while serving.
///
/// Admin scope. Writes `<data_dir>/backups/<name>/` containing the database, the
/// Parquet files, the small state files a restore needs (`.secret`,
/// `admin.json`, `api_keys.json`, whichever exist) and a manifest, owner-only,
/// and answers with what it holds. Queries and
/// flushes pause while it runs (it takes the same lock a flush does); the
/// Parquet half is hard links, so the pause is short unless the hot table is
/// large. An existing backup of the same name is a `409`, never overwritten.
///
/// To restore, stop the server and copy the backup's `mallard.duckdb` and
/// `events/` over the data directory's (see the backup and restore page).
pub async fn create_backup(
    State(state): State<Arc<AppState>>,
    ApiQuery(params): ApiQuery<BackupParams>,
) -> Result<impl IntoResponse, ApiError> {
    let name = params
        .name
        .unwrap_or_else(|| format!("mallard-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ")));

    let report = {
        let state = Arc::clone(&state);
        let name = name.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&state.backup_dir)?;
            backup::create_backup(
                state.buffer.conn(),
                &state.tier_lock,
                &state.data_dir,
                &state.events_dir,
                &state.backup_dir,
                &name,
            )
        })
        .await
        .map_err(|e| ApiError::Internal(format!("Backup task panicked: {e}")))?
    }
    .map_err(|e| match e {
        BackupError::BadName(_) => ApiError::BadRequest(
            "name must be 1-128 characters of letters, digits, '-', '_' or '.', and must not \
             start with a dot"
                .to_string(),
        ),
        BackupError::Exists(_) => ApiError::Conflict(format!("A backup named {name:?} exists")),
        other => ApiError::Internal(format!("Backup failed: {other}")),
    })?;

    tracing::warn!(
        name = %name,
        parquet_files = report.parquet_files,
        parquet_bytes = report.parquet_bytes,
        hot_rows = report.hot_rows,
        "Backup created"
    );

    Ok((
        axum::http::StatusCode::CREATED,
        Json(serde_json::json!({
            "status": "created",
            "name": name,
            "path": report.path,
            "parquet_files": report.parquet_files,
            "parquet_bytes": report.parquet_bytes,
            "hot_rows": report.hot_rows,
            "state_files": report.state_files,
        })),
    ))
}
