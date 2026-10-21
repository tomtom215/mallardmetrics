//! Saved segments: named filter sets, stored per site.
//!
//! A segment is a filter string in the syntax of the `filters` query parameter
//! (`countries==DE;browsers!=Firefox`), kept on the server so every dashboard
//! user sees the same list and a share link can be restricted to one.
//!
//! Validation is the existing parser, [`parse_filters`]: whatever it rejects a
//! segment rejects, and the stored text is the *canonical rendering of what it
//! parsed*, not the caller's spelling, so a stored segment always re-parses to
//! the same filters. Counts and sizes are capped because the list is
//! attacker-influenced input from any admin session.

use crate::api::errors::{ApiError, ApiJson, ApiQuery};
use crate::api::persist::{JsonList, random_hex, storage_unwritable};
use crate::api::stats::{parse_filters, validate_site_id};
use crate::ingest::handler::AppState;
use crate::query::Filter;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::path::Path as FsPath;
use std::sync::Arc;

/// Most segments one site may have.
pub const MAX_SEGMENTS_PER_SITE: usize = 50;
/// Most segments an instance may hold, across all sites.
pub const MAX_SEGMENTS_TOTAL: usize = 500;
/// Longest segment name, in characters.
pub const MAX_NAME_CHARS: usize = 64;
/// Most conditions in one segment. Below the 12 a request may carry
/// ([`crate::api::stats::MAX_FILTERS`]) so a viewer of a share link restricted
/// to a segment still has room to add filters of their own.
pub const MAX_SEGMENT_FILTERS: usize = 8;
/// Longest canonical filter string, in bytes.
pub const MAX_FILTER_BYTES: usize = 4096;

/// A saved segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub id: String,
    pub site_id: String,
    pub name: String,
    /// Canonical filter string, e.g. `countries==DE;browsers!=Firefox`.
    pub filters: String,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// Why a segment operation failed.
#[derive(Debug)]
pub enum SegmentError {
    /// The name or filters are not acceptable (400).
    Invalid(String),
    /// A cap was reached or the name is taken (409).
    Conflict(String),
    /// No such segment (404).
    NotFound,
    /// The store could not be written (500).
    Io(std::io::Error),
}

impl From<std::io::Error> for SegmentError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Render parsed filters in the form [`parse_filters`] reads.
pub fn canonical_filters(filters: &[Filter]) -> String {
    filters
        .iter()
        .map(|f| {
            format!(
                "{}{}{}",
                f.dimension.slug(),
                if f.negated { "!=" } else { "==" },
                f.value
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// Validate filter text for a segment and return it in canonical form.
///
/// # Errors
///
/// [`SegmentError::Invalid`] for anything [`parse_filters`] rejects, an empty
/// set, more than [`MAX_SEGMENT_FILTERS`] conditions, or text over
/// [`MAX_FILTER_BYTES`].
pub fn validate_filters(raw: &str) -> Result<String, SegmentError> {
    let filters = parse_filters(raw).map_err(|e| match e {
        ApiError::BadRequest(message) => SegmentError::Invalid(message),
        other => SegmentError::Invalid(other.to_string()),
    })?;
    if filters.is_empty() {
        return Err(SegmentError::Invalid(
            "A segment needs at least one filter".to_string(),
        ));
    }
    if filters.len() > MAX_SEGMENT_FILTERS {
        return Err(SegmentError::Invalid(format!(
            "A segment holds at most {MAX_SEGMENT_FILTERS} filters (got {})",
            filters.len()
        )));
    }
    let canonical = canonical_filters(&filters);
    if canonical.len() > MAX_FILTER_BYTES {
        return Err(SegmentError::Invalid(format!(
            "A segment's filters may total at most {MAX_FILTER_BYTES} bytes"
        )));
    }
    // The stored text is what every later reader parses. Refuse a value whose
    // canonical spelling would not read back as the same filters.
    match parse_filters(&canonical) {
        Ok(again) if again == filters => Ok(canonical),
        _ => Err(SegmentError::Invalid(
            "These filters cannot be stored faithfully; simplify the values".to_string(),
        )),
    }
}

/// Validate a segment name: 1 to [`MAX_NAME_CHARS`] characters, no control characters.
///
/// # Errors
///
/// [`SegmentError::Invalid`].
pub fn validate_name(raw: &str) -> Result<String, SegmentError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(SegmentError::Invalid(
            "Segment name must not be empty".to_string(),
        ));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(SegmentError::Invalid(format!(
            "Segment name must be at most {MAX_NAME_CHARS} characters"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(SegmentError::Invalid(
            "Segment name must not contain control characters".to_string(),
        ));
    }
    Ok(name.to_string())
}

/// The saved segments.
#[derive(Clone, Default)]
pub struct SegmentStore {
    list: JsonList<Segment>,
}

impl SegmentStore {
    /// A store that is never written to disk.
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// The store backed by `<data_dir>/segments.json`.
    pub fn load(data_dir: &FsPath) -> Self {
        Self {
            list: JsonList::load(data_dir.join("segments.json"), "segments.json"),
        }
    }

    /// Segments, optionally of one site, ordered by name.
    pub fn list(&self, site_id: Option<&str>) -> Vec<Segment> {
        let mut out: Vec<Segment> = self.list.read(|all| {
            all.iter()
                .filter(|s| site_id.is_none_or(|site| s.site_id == site))
                .cloned()
                .collect()
        });
        out.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// One segment by ID.
    pub fn get(&self, id: &str) -> Option<Segment> {
        self.list
            .read(|all| all.iter().find(|s| s.id == id).cloned())
    }

    /// Create a segment. Blocks on file I/O.
    ///
    /// # Errors
    ///
    /// [`SegmentError`].
    pub fn create(
        &self,
        site_id: &str,
        name: &str,
        filters: &str,
    ) -> Result<Segment, SegmentError> {
        validate_site_id(site_id).map_err(|e| SegmentError::Invalid(e.to_string()))?;
        let name = validate_name(name)?;
        let filters = validate_filters(filters)?;
        let id = random_hex(12)?;
        self.list.update(|all| {
            if all.len() >= MAX_SEGMENTS_TOTAL {
                return Err(SegmentError::Conflict(format!(
                    "This instance already holds {MAX_SEGMENTS_TOTAL} segments; delete one first"
                )));
            }
            if all.iter().filter(|s| s.site_id == site_id).count() >= MAX_SEGMENTS_PER_SITE {
                return Err(SegmentError::Conflict(format!(
                    "{site_id} already has {MAX_SEGMENTS_PER_SITE} segments; delete one first"
                )));
            }
            if name_taken(all, site_id, &name, None) {
                return Err(SegmentError::Conflict(format!(
                    "{site_id} already has a segment named {name:?}"
                )));
            }
            let now = chrono::Utc::now().naive_utc();
            let segment = Segment {
                id,
                site_id: site_id.to_string(),
                name,
                filters,
                created_at: now,
                updated_at: now,
            };
            all.push(segment.clone());
            Ok(segment)
        })
    }

    /// Change a segment's name and/or filters. Blocks on file I/O.
    ///
    /// Share links taken from the segment earlier keep the filters they were
    /// created with.
    ///
    /// # Errors
    ///
    /// [`SegmentError`].
    pub fn replace(
        &self,
        id: &str,
        name: Option<&str>,
        filters: Option<&str>,
    ) -> Result<Segment, SegmentError> {
        let name = name.map(validate_name).transpose()?;
        let filters = filters.map(validate_filters).transpose()?;
        self.list.update(|all| {
            let Some(index) = all.iter().position(|s| s.id == id) else {
                return Err(SegmentError::NotFound);
            };
            let site_id = all[index].site_id.clone();
            if let Some(name) = &name
                && name_taken(all, &site_id, name, Some(id))
            {
                return Err(SegmentError::Conflict(format!(
                    "{site_id} already has a segment named {name:?}"
                )));
            }
            let segment = &mut all[index];
            if let Some(name) = name {
                segment.name = name;
            }
            if let Some(filters) = filters {
                segment.filters = filters;
            }
            segment.updated_at = chrono::Utc::now().naive_utc();
            Ok(segment.clone())
        })
    }

    /// Delete a segment. Blocks on file I/O.
    ///
    /// # Errors
    ///
    /// [`SegmentError::NotFound`] or an I/O error.
    pub fn delete(&self, id: &str) -> Result<(), SegmentError> {
        self.list.update(|all| {
            let before = all.len();
            all.retain(|s| s.id != id);
            if all.len() == before {
                return Err(SegmentError::NotFound);
            }
            Ok(())
        })
    }
}

fn name_taken(all: &[Segment], site_id: &str, name: &str, except: Option<&str>) -> bool {
    all.iter().any(|s| {
        s.site_id == site_id
            && Some(s.id.as_str()) != except
            && s.name.to_lowercase() == name.to_lowercase()
    })
}

// ── HTTP ─────────────────────────────────────────────────────────────────

fn respond(error: SegmentError, action: &str) -> Response {
    match error {
        SegmentError::Invalid(m) => ApiError::BadRequest(m).into_response(),
        SegmentError::Conflict(m) => ApiError::Conflict(m).into_response(),
        SegmentError::NotFound => {
            ApiError::NotFound("Segment not found".to_string()).into_response()
        }
        SegmentError::Io(e) => storage_unwritable(action, &e),
    }
}

/// Run blocking store work off the async workers (LESSONS L19).
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, SegmentError> + Send + 'static,
) -> Result<T, SegmentError> {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| {
        tracing::error!(error = %e, "Segment store task failed");
        Err(SegmentError::Io(std::io::Error::other("task failed")))
    })
}

/// Query of `GET /api/segments`.
#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub site_id: Option<String>,
}

/// Body of `POST /api/segments`.
#[derive(Debug, Deserialize)]
pub struct CreateSegment {
    pub site_id: String,
    pub name: String,
    pub filters: String,
}

/// Body of `PUT /api/segments/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateSegment {
    pub name: Option<String>,
    pub filters: Option<String>,
}

fn view(segment: &Segment) -> serde_json::Value {
    serde_json::json!({
        "id": segment.id,
        "site_id": segment.site_id,
        "name": segment.name,
        "filters": segment.filters,
        "created_at": segment.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "updated_at": segment.updated_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    })
}

/// GET /api/segments — saved segments (read scope), optionally of one site.
pub async fn list_segments(
    State(state): State<Arc<AppState>>,
    ApiQuery(params): ApiQuery<ListParams>,
) -> Result<Response, ApiError> {
    if let Some(site) = &params.site_id {
        validate_site_id(site)?;
    }
    let segments = state.share.segments.list(params.site_id.as_deref());
    let body: Vec<_> = segments.iter().map(view).collect();
    Ok(Json(body).into_response())
}

/// POST /api/segments — create a segment (admin).
pub async fn create_segment(
    State(state): State<Arc<AppState>>,
    ApiJson(body): ApiJson<CreateSegment>,
) -> Response {
    let store = state.share.segments.clone();
    match blocking(move || store.create(&body.site_id, &body.name, &body.filters)).await {
        Ok(segment) => {
            tracing::info!(segment_id = %segment.id, site_id = %segment.site_id, "Segment created");
            (StatusCode::CREATED, Json(view(&segment))).into_response()
        }
        Err(e) => respond(e, "save the segment"),
    }
}

/// PUT /api/segments/{id} — rename or re-filter a segment (admin).
pub async fn update_segment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<UpdateSegment>,
) -> Response {
    if body.name.is_none() && body.filters.is_none() {
        return ApiError::BadRequest("Give a name, filters, or both".to_string()).into_response();
    }
    let store = state.share.segments.clone();
    match blocking(move || store.replace(&id, body.name.as_deref(), body.filters.as_deref())).await
    {
        Ok(segment) => Json(view(&segment)).into_response(),
        Err(e) => respond(e, "save the segment"),
    }
}

/// DELETE /api/segments/{id} — delete a segment (admin).
pub async fn delete_segment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let store = state.share.segments.clone();
    match blocking(move || store.delete(&id)).await {
        Ok(()) => Json(serde_json::json!({"status": "deleted"})).into_response(),
        Err(e) => respond(e, "delete the segment"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SegmentStore {
        SegmentStore::in_memory()
    }

    #[test]
    fn test_create_list_update_delete() {
        let s = store();
        let a = s.create("a.com", "Germany", "countries==DE").unwrap();
        assert_eq!(a.filters, "countries==DE");
        assert_eq!(s.list(Some("a.com")).len(), 1);
        assert!(s.list(Some("b.com")).is_empty());
        let b = s
            .replace(
                &a.id,
                Some("Germany, desktop"),
                Some("countries==DE;devices==desktop"),
            )
            .unwrap();
        assert_eq!(b.id, a.id);
        assert_eq!(b.filters, "countries==DE;devices==desktop");
        s.delete(&a.id).unwrap();
        assert!(s.get(&a.id).is_none());
        assert!(matches!(s.delete(&a.id), Err(SegmentError::NotFound)));
    }

    #[test]
    fn test_stored_text_is_the_canonical_parse() {
        let s = store();
        let seg = s
            .create("a.com", " x ", "  countries == DE ;; browsers!=Firefox  ")
            .unwrap();
        assert_eq!(seg.name, "x");
        assert_eq!(seg.filters, "countries==DE;browsers!=Firefox");
    }

    #[test]
    fn test_everything_the_filter_parser_rejects_is_rejected() {
        let s = store();
        for bad in [
            "",
            "   ",
            "nonsense",
            "countries=DE",
            "countries==",
            "bogus==x",
            "entry-pages==/a",
            "exit-pages!=/a",
        ] {
            assert!(
                matches!(s.create("a.com", "n", bad), Err(SegmentError::Invalid(_))),
                "{bad:?} must be rejected"
            );
        }
        let long = format!(
            "pages=={}",
            "x".repeat(crate::api::stats::MAX_FILTER_VALUE_LEN + 1)
        );
        assert!(matches!(
            s.create("a.com", "n", &long),
            Err(SegmentError::Invalid(_))
        ));
    }

    #[test]
    fn test_filter_count_and_size_are_capped() {
        let s = store();
        let nine: Vec<String> = (0..=MAX_SEGMENT_FILTERS)
            .map(|i| format!("pages==/p{i}"))
            .collect();
        assert!(matches!(
            s.create("a.com", "n", &nine.join(";")),
            Err(SegmentError::Invalid(_))
        ));
        let eight = nine[..MAX_SEGMENT_FILTERS].join(";");
        assert!(s.create("a.com", "n", &eight).is_ok());
        // Eight maximum-length values exceed the byte cap.
        let big: Vec<String> = (0..MAX_SEGMENT_FILTERS)
            .map(|i| {
                format!(
                    "pages==/{i}{}",
                    "x".repeat(crate::api::stats::MAX_FILTER_VALUE_LEN - 4)
                )
            })
            .collect();
        assert!(matches!(
            s.create("a.com", "big", &big.join(";")),
            Err(SegmentError::Invalid(m)) if m.contains("bytes")
        ));
    }

    #[test]
    fn test_name_rules() {
        let s = store();
        for bad in [
            "",
            "   ",
            "a\u{0}b",
            "line\nbreak",
            &"n".repeat(MAX_NAME_CHARS + 1),
        ] {
            assert!(
                matches!(
                    s.create("a.com", bad, "countries==DE"),
                    Err(SegmentError::Invalid(_))
                ),
                "{bad:?}"
            );
        }
        assert!(
            s.create("a.com", &"n".repeat(MAX_NAME_CHARS), "countries==DE")
                .is_ok()
        );
        // Multi-byte characters count as characters, not bytes.
        assert!(
            s.create("a.com", &"é".repeat(MAX_NAME_CHARS), "countries==FR")
                .is_ok()
        );
    }

    #[test]
    fn test_site_id_is_validated() {
        let s = store();
        assert!(matches!(
            s.create("../etc", "n", "countries==DE"),
            Err(SegmentError::Invalid(_))
        ));
    }

    #[test]
    fn test_duplicate_name_is_a_conflict_per_site() {
        let s = store();
        s.create("a.com", "Mobile", "devices==mobile").unwrap();
        assert!(matches!(
            s.create("a.com", "mobile", "devices==tablet"),
            Err(SegmentError::Conflict(_))
        ));
        // The same name on another site is fine.
        assert!(s.create("b.com", "Mobile", "devices==mobile").is_ok());
        let other = s.create("a.com", "Tablet", "devices==tablet").unwrap();
        assert!(matches!(
            s.replace(&other.id, Some("MOBILE"), None),
            Err(SegmentError::Conflict(_))
        ));
        // Renaming to its own name is not a conflict.
        assert!(s.replace(&other.id, Some("Tablet"), None).is_ok());
    }

    #[test]
    fn test_per_site_cap() {
        let s = store();
        for i in 0..MAX_SEGMENTS_PER_SITE {
            s.create("a.com", &format!("s{i}"), "countries==DE")
                .unwrap();
        }
        assert!(matches!(
            s.create("a.com", "one too many", "countries==DE"),
            Err(SegmentError::Conflict(m)) if m.contains("50")
        ));
        assert!(s.create("b.com", "other site", "countries==DE").is_ok());
    }

    #[test]
    fn test_total_cap() {
        let s = store();
        let sites = MAX_SEGMENTS_TOTAL / MAX_SEGMENTS_PER_SITE;
        for site in 0..sites {
            for i in 0..MAX_SEGMENTS_PER_SITE {
                s.create(
                    &format!("site{site}.com"),
                    &format!("s{i}"),
                    "countries==DE",
                )
                .unwrap();
            }
        }
        assert!(matches!(
            s.create("fresh.com", "n", "countries==DE"),
            Err(SegmentError::Conflict(m)) if m.contains("500")
        ));
    }

    #[test]
    fn test_update_needs_a_valid_value_and_a_known_id() {
        let s = store();
        let a = s.create("a.com", "n", "countries==DE").unwrap();
        assert!(matches!(
            s.replace(&a.id, None, Some("bogus==x")),
            Err(SegmentError::Invalid(_))
        ));
        assert_eq!(s.get(&a.id).unwrap().filters, "countries==DE");
        assert!(matches!(
            s.replace("missing", Some("x"), None),
            Err(SegmentError::NotFound)
        ));
    }

    #[test]
    fn test_persisted_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let a = SegmentStore::load(dir.path())
            .create("a.com", "Germany", "countries==DE")
            .unwrap();
        let again = SegmentStore::load(dir.path());
        assert_eq!(again.get(&a.id).unwrap(), a);
    }

    #[test]
    fn test_canonical_filters_round_trip_for_awkward_values() {
        for raw in [
            "pages==/a==b",
            "pages!=/a!=b",
            "referrers==https://x.example/?a=1&b=2",
            "pages==/caf\u{e9}",
            "utm-campaigns==spring, sale",
        ] {
            let canonical = validate_filters(raw).unwrap();
            assert_eq!(
                parse_filters(&canonical).unwrap(),
                parse_filters(raw).unwrap()
            );
        }
    }
}
