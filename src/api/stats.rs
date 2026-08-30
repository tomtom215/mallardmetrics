use crate::api::errors::ApiError;
use crate::ingest::handler::AppState;
use crate::query::cache::{CacheKey, cache_key};
use crate::query::{
    Filter, UNKNOWN_VALUE, breakdowns, events, export, flow, funnel, metrics, realtime, retention,
    revenue, sequences, timeseries,
};
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::{Json, http::HeaderValue};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ── Shared parameter handling ────────────────────────────────────────────

/// Maximum span of an explicit date range, in days.
///
/// Public because it is part of the API's contract: a request exceeding it is
/// rejected with `400`, and the documentation for [`StatsParams`] refers to it.
pub const MAX_RANGE_DAYS: i64 = 366;
/// Default rows returned by a breakdown.
pub const DEFAULT_LIMIT: usize = 10;
/// Hard cap on a breakdown's `limit`.
pub const MAX_BREAKDOWN_LIMIT: usize = 1000;
/// Default rows returned by a raw export.
pub const DEFAULT_EXPORT_LIMIT: usize = 100_000;

/// Query parameters accepted by every stats endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct StatsParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    /// Rows to return, where the endpoint returns a list.
    pub limit: Option<usize>,
    /// Segment filters, e.g. `browsers==Chrome;countries!=US`.
    pub filters: Option<String>,
}

fn default_period() -> String {
    "30d".to_string()
}

/// A canonical, order-independent rendering of a filter set for cache keys.
///
/// Derived from the *parsed* filters rather than the raw string, so two
/// spellings of the same segment share a cache entry, and sorted so the order
/// they were written in does not matter. Two different segments cannot collide:
/// the cache key that consumes this is itself length-prefixed.
pub fn filters_cache_key(filters: &[Filter]) -> String {
    let mut parts: Vec<String> = filters
        .iter()
        .map(|f| {
            format!(
                "{}{}{}",
                f.dimension.slug(),
                if f.negated { "!=" } else { "==" },
                f.value
            )
        })
        .collect();
    parts.sort_unstable();
    parts.dedup();
    parts.join(";")
}

/// Maximum filters accepted on one request.
///
/// Each filter adds a predicate and a bound parameter; a request carrying
/// hundreds would be a way to make the server build pathological SQL.
pub const MAX_FILTERS: usize = 12;

/// Maximum length of one filter value.
pub const MAX_FILTER_VALUE_LEN: usize = 512;

/// Parse the `filters` query parameter.
///
/// Format: `dimension==value` or `dimension!=value`, joined with `;`.
/// Dimension names are the breakdown slugs, so a dashboard can turn a breakdown
/// row into a filter without a second vocabulary. The value `(unknown)` is what
/// a breakdown displays for `NULL`, and matches `NULL` here for the same reason.
///
/// `;` separates filters and `,` does not, because values legitimately contain
/// commas — a UTM campaign or a page title, for instance.
///
/// # Errors
///
/// Returns `400` for an unknown dimension, a dimension that cannot be filtered,
/// a missing operator, an empty value, or too many filters.
pub fn parse_filters(raw: &str) -> Result<Vec<Filter>, ApiError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let mut filters = Vec::new();
    for part in trimmed.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // `!=` is checked first: `==` would otherwise never be reached for it,
        // and splitting on `=` alone would mangle both.
        let (name, negated, value) = if let Some((n, v)) = part.split_once("!=") {
            (n, true, v)
        } else if let Some((n, v)) = part.split_once("==") {
            (n, false, v)
        } else {
            return Err(ApiError::BadRequest(format!(
                "Invalid filter {part:?}. Use 'dimension==value' or 'dimension!=value'."
            )));
        };

        let name = name.trim();
        let value = value.trim();
        if value.is_empty() {
            return Err(ApiError::BadRequest(format!(
                "Filter {name:?} has no value. Use '{name}=={UNKNOWN_VALUE}' to \
                 match events where it was not recorded."
            )));
        }
        if value.len() > MAX_FILTER_VALUE_LEN {
            return Err(ApiError::BadRequest(format!(
                "Filter value for {name:?} exceeds {MAX_FILTER_VALUE_LEN} characters"
            )));
        }

        let Some(dimension) = breakdowns::Dimension::from_slug(name) else {
            return Err(ApiError::BadRequest(format!(
                "Unknown filter dimension {name:?}. Available: {}",
                breakdowns::Dimension::SLUGS.join(", ")
            )));
        };
        if !Filter::is_filterable(dimension) {
            return Err(ApiError::BadRequest(format!(
                "{name:?} cannot be filtered on: entry and exit pages are derived \
                 from a session rather than stored on an event, so there is no \
                 per-event value to match. Filter on 'pages' instead."
            )));
        }

        filters.push(Filter {
            dimension,
            negated,
            value: value.to_string(),
        });
    }

    if filters.len() > MAX_FILTERS {
        return Err(ApiError::BadRequest(format!(
            "At most {MAX_FILTERS} filters are accepted (got {})",
            filters.len()
        )));
    }
    Ok(filters)
}

/// Validate a `site_id`.
///
/// Must be non-empty, at most 256 bytes, and contain only ASCII alphanumerics
/// or `.`, `-`, `_`, `:`. The same rule guards ingestion, so anything accepted
/// at ingest is queryable, and nothing accepted here can escape a partition
/// directory.
///
/// # Errors
///
/// Returns a `BadRequest` describing the violation.
pub fn validate_site_id(site_id: &str) -> Result<(), ApiError> {
    if site_id.is_empty() {
        return Err(ApiError::BadRequest(
            "site_id must not be empty".to_string(),
        ));
    }
    if site_id.len() > 256 {
        return Err(ApiError::BadRequest(
            "site_id must be at most 256 characters".to_string(),
        ));
    }
    if !site_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
    {
        return Err(ApiError::BadRequest(
            "site_id may only contain alphanumeric characters, '.', '-', '_', ':'".to_string(),
        ));
    }
    Ok(())
}

/// A resolved, validated time range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateRange {
    /// Inclusive lower bound.
    pub start: String,
    /// Exclusive upper bound.
    pub end: String,
    /// Span in days, used to pick a time-series granularity.
    pub days: i64,
}

impl StatsParams {
    /// Resolve the request's time range.
    ///
    /// An explicit `start_date`/`end_date` pair wins over `period`; both are
    /// parsed as `YYYY-MM-DD` and capped at [`MAX_RANGE_DAYS`] so a request
    /// cannot ask for an unbounded partition scan.
    ///
    /// # Errors
    ///
    /// Returns a `BadRequest` for a malformed date, an inverted range, an
    /// over-long range, or an unknown period.
    pub fn date_range(&self) -> Result<DateRange, ApiError> {
        if let (Some(start_str), Some(end_str)) = (&self.start_date, &self.end_date) {
            let start = parse_date(start_str, "start_date")?;
            let end = parse_date(end_str, "end_date")?;
            let days = (end - start).num_days();
            if days < 0 {
                return Err(ApiError::BadRequest(
                    "end_date must be on or after start_date".to_string(),
                ));
            }
            if days > MAX_RANGE_DAYS {
                return Err(ApiError::BadRequest(format!(
                    "Date range must not exceed {MAX_RANGE_DAYS} days"
                )));
            }
            // The stored end bound is exclusive, so an inclusive end_date of
            // 2024-01-31 must query up to 2024-02-01. Without this the final
            // day of every explicit range was silently omitted.
            let exclusive_end = end
                .succ_opt()
                .ok_or_else(|| ApiError::BadRequest("end_date is out of range".to_string()))?;
            return Ok(DateRange {
                start: start.to_string(),
                end: exclusive_end.to_string(),
                days: days + 1,
            });
        }

        let now = chrono::Utc::now().date_naive();
        let (start, days) = match self.period.as_str() {
            "day" | "today" => (now, 1),
            "7d" => (now - chrono::Days::new(7), 8),
            "30d" => (now - chrono::Days::new(30), 31),
            "90d" => (now - chrono::Days::new(90), 91),
            "12mo" => (now - chrono::Days::new(365), 366),
            other => {
                return Err(ApiError::BadRequest(format!(
                    "Invalid period: {other}. Use 'day', '7d', '30d', '90d', '12mo', \
                     or supply start_date and end_date."
                )));
            }
        };
        Ok(DateRange {
            start: start.to_string(),
            end: (now + chrono::Days::new(1)).to_string(),
            days,
        })
    }

    /// Validate the site ID and resolve the range in one step.
    ///
    /// # Errors
    ///
    /// Propagates validation failures.
    pub fn validated(&self) -> Result<DateRange, ApiError> {
        validate_site_id(&self.site_id)?;
        self.date_range()
    }

    /// The requested row limit, clamped to `max`.
    ///
    /// # Errors
    ///
    /// Returns a `BadRequest` when the caller asks for more than `max`, rather
    /// than silently returning fewer rows than requested.
    pub fn limit_or(&self, default: usize, max: usize) -> Result<usize, ApiError> {
        match self.limit {
            None => Ok(default),
            Some(0) => Err(ApiError::BadRequest("limit must be at least 1".to_string())),
            Some(n) if n > max => Err(ApiError::BadRequest(format!("limit must not exceed {max}"))),
            Some(n) => Ok(n),
        }
    }
}

fn parse_date(raw: &str, field: &str) -> Result<NaiveDate, ApiError> {
    NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map_err(|_| ApiError::BadRequest(format!("Invalid {field} format. Use YYYY-MM-DD.")))
}

// ── Query execution helpers ──────────────────────────────────────────────

/// Run a blocking DuckDB query on a reader connection, under the concurrency cap.
///
/// Every analytics endpoint goes through here, so the concurrency limit and the
/// blocking-pool handoff apply uniformly. Previously the semaphore guarded only
/// four of the thirteen endpoints, which meant it could not actually bound
/// concurrent load.
///
/// # Errors
///
/// Returns `TooManyRequests` when the cap is reached, or the query's own error.
async fn run_query<T, F>(state: &Arc<AppState>, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&duckdb::Connection) -> Result<T, duckdb::Error> + Send + 'static,
{
    let semaphore = Arc::clone(&state.query_semaphore);
    let _permit = semaphore.try_acquire().map_err(|_| {
        ApiError::TooManyRequests(
            "Too many concurrent queries. Please retry in a moment.".to_string(),
        )
    })?;

    let readers = state.readers.clone();
    tokio::task::spawn_blocking(move || {
        let conn = readers.acquire();
        let guard = conn.lock();
        f(&guard)
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))?
    .map_err(ApiError::DatabaseError)
}

/// Run a cached, JSON-serialisable query.
///
/// Caching now covers every read endpoint. It previously applied to two of
/// them, so a dashboard load re-ran eleven uncached queries against the
/// database on every refresh.
async fn cached_query<T, F>(state: &Arc<AppState>, key: CacheKey, f: F) -> Result<Json<T>, ApiError>
where
    T: Serialize + serde::de::DeserializeOwned + Send + 'static,
    F: FnOnce(&duckdb::Connection) -> Result<T, duckdb::Error> + Send + 'static,
{
    if let Some(cached) = state.query_cache.get(key.as_str())
        && let Ok(value) = serde_json::from_str::<T>(&cached)
    {
        return Ok(Json(value));
    }
    let result = run_query(state, f).await?;
    if let Ok(serialized) = serde_json::to_string(&result) {
        state.query_cache.insert(key, serialized);
    }
    Ok(Json(result))
}

/// Map a behavioral-extension query failure to a 503 rather than a 500.
fn behavioral_error(state: &AppState, feature: &str, err: ApiError) -> ApiError {
    if state.behavioral_extension_loaded {
        err
    } else {
        ApiError::behavioral_required(feature)
    }
}

// ── Endpoints ────────────────────────────────────────────────────────────

/// GET /api/sites — site IDs that have data.
///
/// The dashboard previously had no way to discover them, so an operator had to
/// remember and retype each site ID.
pub async fn list_sites(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let configured = state.allowed_sites.clone();
    let storage = state.buffer.storage().clone();

    let mut sites = run_query(&state, move |conn| {
        let mut stmt = conn.prepare("SELECT DISTINCT site_id FROM events_all ORDER BY site_id")?;
        let rows: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(Result::ok)
            .collect();
        Ok(rows)
    })
    .await
    // A site whose data lives only in Parquet is still discoverable from disk,
    // and disk is also the fallback if the view cannot be queried.
    .unwrap_or_default();

    sites.extend(storage.known_site_ids());
    sites.extend(configured);
    sites.sort_unstable();
    sites.dedup();

    Ok(Json(serde_json::json!({ "sites": sites })))
}

/// GET /api/stats/main — headline metrics.
pub async fn get_main_stats(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatsParams>,
) -> Result<Json<metrics::CoreMetrics>, ApiError> {
    let range = params.validated()?;
    let filters = parse_filters(params.filters.as_deref().unwrap_or_default())?;
    let filter_key = filters_cache_key(&filters);
    let scope = state
        .scope(&params.site_id, &range.start, &range.end)
        .with_filters(filters);
    let key = cache_key(
        "main",
        &params.site_id,
        &[&range.start, &range.end, &filter_key],
    );
    cached_query(&state, key, move |conn| {
        metrics::query_core_metrics(conn, &scope)
    })
    .await
}

/// GET /api/stats/sessions — session-level metrics.
pub async fn get_sessions(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatsParams>,
) -> Result<Json<metrics::SessionMetrics>, ApiError> {
    let range = params.validated()?;
    let filters = parse_filters(params.filters.as_deref().unwrap_or_default())?;
    let filter_key = filters_cache_key(&filters);
    let scope = state
        .scope(&params.site_id, &range.start, &range.end)
        .with_filters(filters);
    let key = cache_key(
        "sessions",
        &params.site_id,
        &[&range.start, &range.end, &filter_key],
    );
    cached_query(&state, key, move |conn| {
        metrics::query_session_metrics(conn, &scope)
    })
    .await
    .map_err(|e| behavioral_error(&state, "Session metrics", e))
}

/// GET /api/stats/timeseries — bucketed visitor and pageview counts.
pub async fn get_timeseries(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatsParams>,
) -> Result<Json<Vec<timeseries::TimeBucket>>, ApiError> {
    let range = params.validated()?;
    let granularity = timeseries::Granularity::for_span_days(range.days);
    let filters = parse_filters(params.filters.as_deref().unwrap_or_default())?;
    let filter_key = filters_cache_key(&filters);
    let scope = state
        .scope(&params.site_id, &range.start, &range.end)
        .with_filters(filters);
    let key = cache_key(
        "ts",
        &params.site_id,
        &[
            &range.start,
            &range.end,
            &format!("{granularity:?}"),
            &filter_key,
        ],
    );
    cached_query(&state, key, move |conn| {
        timeseries::query_timeseries(conn, &scope, granularity)
    })
    .await
}

/// GET /api/stats/breakdown/{dimension} — a breakdown by any dimension.
///
/// Replaces six near-identical handlers and, in doing so, exposes the ten
/// dimensions the ingest path had always been collecting with no way to read
/// them back: UTM parameters, region, city, browser and OS versions, screen
/// size, referrer, event name, and entry/exit pages.
pub async fn get_breakdown(
    State(state): State<Arc<AppState>>,
    Path(dimension): Path<String>,
    Query(params): Query<StatsParams>,
) -> Result<Json<Vec<breakdowns::BreakdownRow>>, ApiError> {
    let range = params.validated()?;
    let limit = params.limit_or(DEFAULT_LIMIT, MAX_BREAKDOWN_LIMIT)?;

    let dim = breakdowns::Dimension::from_slug(&dimension).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "Unknown breakdown dimension '{dimension}'. Available: {}",
            breakdowns::Dimension::SLUGS.join(", ")
        ))
    })?;

    if dim.requires_behavioral() && !state.behavioral_extension_loaded {
        return Err(ApiError::behavioral_required(
            "Entry and exit page breakdowns",
        ));
    }

    let filters = parse_filters(params.filters.as_deref().unwrap_or_default())?;
    let filter_key = filters_cache_key(&filters);
    let scope = state
        .scope(&params.site_id, &range.start, &range.end)
        .with_filters(filters);
    let key = cache_key(
        "bd",
        &params.site_id,
        &[
            &dimension,
            &range.start,
            &range.end,
            &limit.to_string(),
            &filter_key,
        ],
    );
    cached_query(&state, key, move |conn| {
        breakdowns::query_breakdown(conn, &scope, dim, limit)
    })
    .await
}

/// GET /api/stats/realtime — activity in the last few minutes.
pub async fn get_realtime(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatsParams>,
) -> Result<Json<realtime::RealtimeSnapshot>, ApiError> {
    validate_site_id(&params.site_id)?;
    let site_id = params.site_id.clone();
    let window = state.realtime_window_minutes;
    // Deliberately uncached: a cached "realtime" figure is not realtime.
    let snapshot = run_query(&state, move |conn| {
        realtime::query_realtime(conn, &site_id, window)
    })
    .await?;
    Ok(Json(snapshot))
}

/// GET /api/stats/revenue — revenue totals, per currency and dimension.
pub async fn get_revenue(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatsParams>,
) -> Result<Json<revenue::RevenueReport>, ApiError> {
    let range = params.validated()?;
    let filters = parse_filters(params.filters.as_deref().unwrap_or_default())?;
    let filter_key = filters_cache_key(&filters);
    let scope = state
        .scope(&params.site_id, &range.start, &range.end)
        .with_filters(filters);
    let key = cache_key(
        "revenue",
        &params.site_id,
        &[&range.start, &range.end, &filter_key],
    );
    cached_query(&state, key, move |conn| {
        revenue::query_revenue(conn, &scope)
    })
    .await
}

/// GET /api/stats/goals — conversions for every custom event.
pub async fn get_goals(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatsParams>,
) -> Result<Json<Vec<events::GoalConversion>>, ApiError> {
    let range = params.validated()?;
    let filters = parse_filters(params.filters.as_deref().unwrap_or_default())?;
    let filter_key = filters_cache_key(&filters);
    let scope = state
        .scope(&params.site_id, &range.start, &range.end)
        .with_filters(filters);
    let key = cache_key(
        "goals",
        &params.site_id,
        &[&range.start, &range.end, &filter_key],
    );
    cached_query(&state, key, move |conn| events::query_goals(conn, &scope)).await
}

/// GET /api/stats/properties — custom property keys seen in range.
pub async fn get_property_keys(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatsParams>,
) -> Result<Json<Vec<String>>, ApiError> {
    let range = params.validated()?;
    let filters = parse_filters(params.filters.as_deref().unwrap_or_default())?;
    let filter_key = filters_cache_key(&filters);
    let scope = state
        .scope(&params.site_id, &range.start, &range.end)
        .with_filters(filters);
    let key = cache_key(
        "propkeys",
        &params.site_id,
        &[&range.start, &range.end, &filter_key],
    );
    cached_query(&state, key, move |conn| {
        events::query_property_keys(conn, &scope)
    })
    .await
}

/// Parameters for a property-value breakdown.
#[derive(Debug, Deserialize)]
pub struct PropertyParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    /// The property key to break down.
    pub key: String,
    /// Restrict to one event name.
    pub event: Option<String>,
    /// Segment filters, e.g. `browsers==Chrome;countries!=US`.
    pub filters: Option<String>,
}

/// GET /api/stats/property-values — one custom property, broken down by value.
pub async fn get_property_values(
    State(state): State<Arc<AppState>>,
    Query(params): Query<PropertyParams>,
) -> Result<Json<Vec<events::PropertyValue>>, ApiError> {
    let common = StatsParams {
        site_id: params.site_id.clone(),
        period: params.period.clone(),
        start_date: params.start_date.clone(),
        end_date: params.end_date.clone(),
        limit: None,
        filters: params.filters.clone(),
    };
    let range = common.validated()?;

    if !events::is_valid_property_key(&params.key) {
        return Err(ApiError::BadRequest(
            "key may only contain alphanumeric characters, '_', '-', '.'".to_string(),
        ));
    }
    if let Some(event) = &params.event
        && (event.is_empty() || event.len() > 256)
    {
        return Err(ApiError::BadRequest(
            "event must be 1-256 characters".to_string(),
        ));
    }

    let filters = parse_filters(params.filters.as_deref().unwrap_or_default())?;
    let filter_key = filters_cache_key(&filters);
    let scope = state
        .scope(&params.site_id, &range.start, &range.end)
        .with_filters(filters);
    let key_name = params.key.clone();
    let event = params.event.clone();
    let cache = cache_key(
        "propvals",
        &params.site_id,
        &[
            &range.start,
            &range.end,
            &params.key,
            params.event.as_deref().unwrap_or("*"),
            &filter_key,
        ],
    );
    cached_query(&state, cache, move |conn| {
        events::query_property_values(conn, &scope, &key_name, event.as_deref())
    })
    .await
}

// ── Behavioral endpoints ─────────────────────────────────────────────────

/// Parameters for the funnel endpoint.
#[derive(Debug, Deserialize)]
pub struct FunnelParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    #[serde(default = "default_window")]
    pub window: String,
    /// Comma-separated steps, each `page:<path>` or `event:<name>`.
    pub steps: String,
    /// Comma-separated `window_funnel` modes.
    #[serde(default)]
    pub modes: String,
    /// Segment filters, e.g. `browsers==Chrome;countries!=US`.
    pub filters: Option<String>,
}

fn default_window() -> String {
    "1 day".to_string()
}

/// Turn a `page:` / `event:` step into a safe SQL boolean expression.
///
/// # Errors
///
/// Returns a `BadRequest` for an unrecognised prefix or an out-of-range value.
pub fn parse_funnel_step(step: &str) -> Result<String, ApiError> {
    let step = step.trim();
    if let Some(path) = step.strip_prefix("page:") {
        if path.is_empty() || path.len() > 256 {
            return Err(ApiError::BadRequest("Invalid page path".to_string()));
        }
        Ok(format!("pathname = '{}'", path.replace('\'', "''")))
    } else if let Some(name) = step.strip_prefix("event:") {
        if name.is_empty() || name.len() > 256 {
            return Err(ApiError::BadRequest("Invalid event name".to_string()));
        }
        Ok(format!("event_name = '{}'", name.replace('\'', "''")))
    } else {
        Err(ApiError::BadRequest(format!(
            "Invalid step format: '{step}'. Use 'page:/path' or 'event:name'."
        )))
    }
}

/// Validate a simple DuckDB interval such as `2 hours`.
///
/// Each unit has its own ceiling so a pathological window like `365 weeks`
/// (about seven years) is rejected while `52 weeks` is allowed.
pub fn is_safe_interval(s: &str) -> bool {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 2 {
        return false;
    }
    let Ok(n) = parts[0].parse::<u32>() else {
        return false;
    };
    if n == 0 {
        return false;
    }
    let max_n: u32 = match parts[1] {
        "second" | "seconds" => 86_400,
        "minute" | "minutes" => 1_440,
        "hour" | "hours" => 720,
        "day" | "days" => 365,
        "week" | "weeks" => 52,
        _ => return false,
    };
    n <= max_n
}

/// Parse a comma-separated step list into SQL conditions.
fn parse_steps(raw: &str, min: usize, max: usize) -> Result<Vec<String>, ApiError> {
    let steps: Vec<String> = raw
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(parse_funnel_step)
        .collect::<Result<Vec<_>, _>>()?;
    if steps.len() < min || steps.len() > max {
        return Err(ApiError::BadRequest(format!(
            "Provide between {min} and {max} steps (got {})",
            steps.len()
        )));
    }
    Ok(steps)
}

/// GET /api/stats/funnel — cumulative funnel analysis.
pub async fn get_funnel(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FunnelParams>,
) -> Result<Json<Vec<funnel::FunnelStep>>, ApiError> {
    let common = StatsParams {
        site_id: params.site_id.clone(),
        period: params.period.clone(),
        start_date: params.start_date.clone(),
        end_date: params.end_date.clone(),
        limit: None,
        filters: params.filters.clone(),
    };
    let range = common.validated()?;

    let window = params.window.trim().to_string();
    if !is_safe_interval(&window) {
        return Err(ApiError::BadRequest(
            "Invalid window interval. Use e.g. '1 day', '2 hours', '30 minutes'.".to_string(),
        ));
    }

    let modes = funnel::normalize_modes(&params.modes).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "Unknown funnel mode. Available: {}",
            funnel::VALID_MODES.join(", ")
        ))
    })?;

    let steps = parse_steps(&params.steps, funnel::MIN_STEPS, funnel::MAX_STEPS)?;

    if !state.behavioral_extension_loaded {
        return Err(ApiError::behavioral_required("Funnel analysis"));
    }

    let filters = parse_filters(params.filters.as_deref().unwrap_or_default())?;
    let filter_key = filters_cache_key(&filters);
    let scope = state
        .scope(&params.site_id, &range.start, &range.end)
        .with_filters(filters);
    let key = cache_key(
        "funnel",
        &params.site_id,
        &[
            &range.start,
            &range.end,
            &window,
            &modes,
            &params.steps,
            &filter_key,
        ],
    );
    cached_query(&state, key, move |conn| {
        let refs: Vec<&str> = steps.iter().map(String::as_str).collect();
        funnel::query_funnel(conn, &scope, &window, &modes, &refs)
    })
    .await
    .map_err(|e| behavioral_error(&state, "Funnel analysis", e))
}

/// Parameters for the retention endpoint.
#[derive(Debug, Deserialize)]
pub struct RetentionParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    #[serde(default = "default_num_weeks")]
    pub weeks: u32,
    /// Segment filters, e.g. `browsers==Chrome;countries!=US`.
    pub filters: Option<String>,
}

const fn default_num_weeks() -> u32 {
    4
}

/// A retention report plus the caveats needed to read it correctly.
#[derive(Debug, Serialize, Deserialize)]
pub struct RetentionResponse {
    pub cohorts: Vec<retention::RetentionCohort>,
    /// Whether the configured visitor-ID rotation can support these cohorts.
    pub identity_supports_cohorts: bool,
    /// Explanation shown when it cannot.
    pub caveat: Option<String>,
}

/// GET /api/stats/retention — weekly retention cohorts.
pub async fn get_retention(
    State(state): State<Arc<AppState>>,
    Query(params): Query<RetentionParams>,
) -> Result<Json<RetentionResponse>, ApiError> {
    let common = StatsParams {
        site_id: params.site_id.clone(),
        period: params.period.clone(),
        start_date: params.start_date.clone(),
        end_date: params.end_date.clone(),
        limit: None,
        filters: params.filters.clone(),
    };
    let range = common.validated()?;

    if !(retention::MIN_PERIODS..=retention::MAX_PERIODS).contains(&params.weeks) {
        return Err(ApiError::BadRequest(format!(
            "weeks must be between {} and {} (the behavioral extension accepts \
             2 to 32 conditions)",
            retention::MIN_PERIODS,
            retention::MAX_PERIODS
        )));
    }

    if !state.behavioral_extension_loaded {
        return Err(ApiError::behavioral_required("Retention cohorts"));
    }

    // A visitor whose ID rotates before the next cohort week is a different
    // visitor, so every figure past week 0 would be structurally zero. Reporting
    // that as a result would be misleading, so the caveat travels with the data.
    let identity_supports_cohorts =
        retention::rotation_supports_weeks(state.visitor_salt_rotation_days, params.weeks);
    let caveat = (!identity_supports_cohorts).then(|| {
        format!(
            "visitor_salt_rotation_days is {}, so visitor identities do not survive \
             the {} weeks this report spans. Returning visitors appear as new ones and \
             retention past week 0 will read as zero. Raise visitor_salt_rotation_days \
             to at least {} for meaningful cohorts.",
            state.visitor_salt_rotation_days,
            params.weeks,
            (params.weeks.saturating_sub(1)) * 7
        )
    });

    let filters = parse_filters(params.filters.as_deref().unwrap_or_default())?;
    let filter_key = filters_cache_key(&filters);
    let scope = state
        .scope(&params.site_id, &range.start, &range.end)
        .with_filters(filters);
    let weeks = params.weeks;
    let key = cache_key(
        "retention",
        &params.site_id,
        &[&range.start, &range.end, &weeks.to_string(), &filter_key],
    );
    let cohorts: Json<Vec<retention::RetentionCohort>> = cached_query(&state, key, move |conn| {
        retention::query_retention(conn, &scope, weeks)
    })
    .await
    .map_err(|e| behavioral_error(&state, "Retention cohorts", e))?;

    Ok(Json(RetentionResponse {
        cohorts: cohorts.0,
        identity_supports_cohorts,
        caveat,
    }))
}

/// Parameters for the sequence endpoint.
#[derive(Debug, Deserialize)]
pub struct SequenceParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    /// Comma-separated steps, each `page:<path>` or `event:<name>`.
    pub steps: String,
    /// Segment filters, e.g. `browsers==Chrome;countries!=US`.
    pub filters: Option<String>,
}

/// GET /api/stats/sequences — ordered pattern matching over event streams.
pub async fn get_sequences(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SequenceParams>,
) -> Result<Json<sequences::SequenceMatchResult>, ApiError> {
    let common = StatsParams {
        site_id: params.site_id.clone(),
        period: params.period.clone(),
        start_date: params.start_date.clone(),
        end_date: params.end_date.clone(),
        limit: None,
        filters: params.filters.clone(),
    };
    let range = common.validated()?;

    let steps = parse_steps(
        &params.steps,
        sequences::MIN_CONDITIONS,
        sequences::MAX_CONDITIONS,
    )?;

    if !state.behavioral_extension_loaded {
        return Err(ApiError::behavioral_required("Sequence analysis"));
    }

    let filters = parse_filters(params.filters.as_deref().unwrap_or_default())?;
    let filter_key = filters_cache_key(&filters);
    let scope = state
        .scope(&params.site_id, &range.start, &range.end)
        .with_filters(filters);
    let key = cache_key(
        "seq",
        &params.site_id,
        &[&range.start, &range.end, &params.steps, &filter_key],
    );
    cached_query(&state, key, move |conn| {
        let refs: Vec<&str> = steps.iter().map(String::as_str).collect();
        sequences::execute_sequence_match(conn, &scope, &refs)
    })
    .await
    .map_err(|e| behavioral_error(&state, "Sequence analysis", e))
}

/// Parameters for the flow endpoint.
#[derive(Debug, Deserialize)]
pub struct FlowParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    /// The page to analyse flow around.
    pub page: String,
    /// `forward` (default) or `backward`.
    #[serde(default = "default_direction")]
    pub direction: String,
    pub limit: Option<usize>,
    /// Segment filters, e.g. `browsers==Chrome;countries!=US`.
    pub filters: Option<String>,
}

fn default_direction() -> String {
    "forward".to_string()
}

/// GET /api/stats/flow — the pages around a given page.
pub async fn get_flow(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FlowParams>,
) -> Result<Json<Vec<flow::FlowNode>>, ApiError> {
    let common = StatsParams {
        site_id: params.site_id.clone(),
        period: params.period.clone(),
        start_date: params.start_date.clone(),
        end_date: params.end_date.clone(),
        limit: params.limit,
        filters: params.filters.clone(),
    };
    let range = common.validated()?;
    let limit = common.limit_or(flow::DEFAULT_LIMIT, flow::MAX_LIMIT)?;

    if params.page.is_empty() || params.page.len() > 256 {
        return Err(ApiError::BadRequest(
            "page must be 1-256 characters".to_string(),
        ));
    }
    let direction = flow::Direction::from_slug(&params.direction).ok_or_else(|| {
        ApiError::BadRequest("direction must be 'forward' or 'backward'".to_string())
    })?;

    if !state.behavioral_extension_loaded {
        return Err(ApiError::behavioral_required("Flow analysis"));
    }

    let filters = parse_filters(params.filters.as_deref().unwrap_or_default())?;
    let filter_key = filters_cache_key(&filters);
    let scope = state
        .scope(&params.site_id, &range.start, &range.end)
        .with_filters(filters);
    let page = params.page.clone();
    let key = cache_key(
        "flow",
        &params.site_id,
        &[
            &range.start,
            &range.end,
            &params.page,
            &params.direction,
            &limit.to_string(),
            &filter_key,
        ],
    );
    cached_query(&state, key, move |conn| {
        flow::query_flow(conn, &scope, &page, direction, limit)
    })
    .await
    .map_err(|e| behavioral_error(&state, "Flow analysis", e))
}

// ── Export ───────────────────────────────────────────────────────────────

/// Parameters for the export endpoint.
#[derive(Debug, Deserialize)]
pub struct ExportParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    /// `csv` (default) or `json`.
    #[serde(default = "default_export_format")]
    pub format: String,
    /// `daily` (default) or `raw`.
    #[serde(default = "default_export_kind")]
    pub kind: String,
    pub limit: Option<usize>,
    /// Segment filters, e.g. `browsers==Chrome;countries!=US`.
    pub filters: Option<String>,
}

fn default_export_format() -> String {
    "csv".to_string()
}

fn default_export_kind() -> String {
    "daily".to_string()
}

/// GET /api/stats/export — download analytics data.
pub async fn get_export(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ExportParams>,
) -> Result<impl IntoResponse, ApiError> {
    let common = StatsParams {
        site_id: params.site_id.clone(),
        period: params.period.clone(),
        start_date: params.start_date.clone(),
        end_date: params.end_date.clone(),
        limit: params.limit,
        filters: params.filters.clone(),
    };
    let range = common.validated()?;

    let format = export::ExportFormat::from_slug(&params.format).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "Invalid format: '{}'. Use 'csv' or 'json'.",
            params.format
        ))
    })?;
    let kind = export::ExportKind::from_slug(&params.kind).ok_or_else(|| {
        ApiError::BadRequest(format!(
            "Invalid kind: '{}'. Use 'daily' or 'raw'.",
            params.kind
        ))
    })?;

    // Exports are not cached — they are large and read once — so no cache key.
    let filters = parse_filters(params.filters.as_deref().unwrap_or_default())?;
    let scope = state
        .scope(&params.site_id, &range.start, &range.end)
        .with_filters(filters);

    let (body, filename) = match kind {
        export::ExportKind::Daily => {
            let rows =
                run_query(&state, move |conn| export::query_daily_export(conn, &scope)).await?;
            let body = match format {
                export::ExportFormat::Csv => export::daily_to_csv(&rows),
                export::ExportFormat::Json => {
                    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
                }
            };
            (body, "mallard-daily")
        }
        export::ExportKind::Raw => {
            let limit = common.limit_or(DEFAULT_EXPORT_LIMIT, export::MAX_RAW_ROWS)?;
            let rows = run_query(&state, move |conn| {
                export::query_raw_export(conn, &scope, limit)
            })
            .await?;
            let body = match format {
                export::ExportFormat::Csv => export::to_csv(export::RAW_COLUMNS, &rows),
                export::ExportFormat::Json => export::raw_to_json(&rows),
            };
            (body, "mallard-events")
        }
    };

    let disposition = format!(
        "attachment; filename=\"{filename}-{}.{}\"",
        range.start,
        format.extension()
    );
    let disposition = HeaderValue::from_str(&disposition)
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"));

    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(format.content_type()),
            ),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        body,
    ))
}

// ── GDPR erasure ─────────────────────────────────────────────────────────

/// Parameters for the erasure endpoint.
#[derive(Debug, Deserialize)]
pub struct GdprEraseParams {
    pub site_id: String,
    /// Inclusive start date, `YYYY-MM-DD`.
    pub start_date: String,
    /// Inclusive end date, `YYYY-MM-DD`.
    pub end_date: String,
}

/// DELETE /api/gdpr/erase — permanently delete data for a site and date range.
///
/// Removes rows from the hot table and the matching Parquet partitions, then
/// refreshes `events_all` and drops the site's cached query results — otherwise
/// the dashboard would keep serving deleted data until the cache TTL expired.
///
/// **Requires admin authentication.**
///
/// # Limitations
///
/// Visitor IDs are pseudonymous hashes, not identities, so a specific person's
/// rows cannot be singled out. Erasure therefore operates on site + date range,
/// which is the granularity an operator can actually act on. Document this in
/// your privacy notice.
pub async fn gdpr_erase(
    State(state): State<Arc<AppState>>,
    Query(params): Query<GdprEraseParams>,
) -> Result<impl IntoResponse, ApiError> {
    validate_site_id(&params.site_id)?;

    let start_date = parse_date(&params.start_date, "start_date")?;
    let end_date = parse_date(&params.end_date, "end_date")?;
    if end_date < start_date {
        return Err(ApiError::BadRequest(
            "end_date must be on or after start_date".to_string(),
        ));
    }
    if (end_date - start_date).num_days() > MAX_RANGE_DAYS {
        return Err(ApiError::BadRequest(format!(
            "Date range must not exceed {MAX_RANGE_DAYS} days"
        )));
    }

    let site_id = params.site_id.clone();
    let start_str = params.start_date.clone();
    let end_str = params.end_date.clone();
    let events_dir = state.events_dir.clone();
    let storage = state.buffer.storage().clone();
    let writer = state.buffer.conn().clone();

    let (db_records_deleted, parquet_partitions_deleted) =
        tokio::task::spawn_blocking(move || -> Result<(i64, u64), duckdb::Error> {
            // Delete from the hot table first, holding the writer lock only for
            // the two statements.
            let db_count: i64 = {
                let guard = writer.lock();
                let count: i64 = guard
                    .query_row(
                        "SELECT COUNT(*) FROM events WHERE site_id = ? \
                         AND STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d') BETWEEN ? AND ?",
                        duckdb::params![site_id, start_str, end_str],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                guard.execute(
                    "DELETE FROM events WHERE site_id = ? \
                     AND STRFTIME(CAST(timestamp AS DATE), '%Y-%m-%d') BETWEEN ? AND ?",
                    duckdb::params![site_id, start_str, end_str],
                )?;
                count
            };

            let parquet_removed = storage
                .erase_partitions(&site_id, start_date, end_date)
                .unwrap_or(0);

            // Rebuild the union view so the deleted partitions leave the read
            // glob. Connections opened against one database share its catalog,
            // so refreshing on the writer covers the whole reader pool — see
            // `storage::tests::test_readers_see_the_view_refreshed_after_a_flush`,
            // which is what the post-flush refresh in `parquet.rs` relies on too.
            crate::storage::schema::setup_query_view(&writer.lock(), &events_dir)?;

            Ok((db_count, parquet_removed))
        })
        .await
        .map_err(|e| ApiError::Internal(format!("Erasure task panicked: {e}")))?
        .map_err(ApiError::DatabaseError)?;

    // Without this the dashboard would keep serving erased data from cache.
    state.query_cache.invalidate_site(&params.site_id);

    tracing::warn!(
        site_id = %params.site_id,
        start_date = %params.start_date,
        end_date = %params.end_date,
        db_records_deleted,
        parquet_partitions_deleted,
        "GDPR erasure completed"
    );

    Ok(Json(serde_json::json!({
        "status": "erased",
        "site_id": params.site_id,
        "start_date": params.start_date,
        "end_date": params.end_date,
        "db_records_deleted": db_records_deleted,
        "parquet_partitions_deleted": parquet_partitions_deleted,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Segment filters ──────────────────────────────────────────────────

    #[test]
    fn test_parse_filters_accepts_equality_and_inequality() {
        let filters = parse_filters("browsers==Chrome;countries!=US").unwrap();
        assert_eq!(filters.len(), 2);
        assert_eq!(filters[0].dimension, breakdowns::Dimension::Browser);
        assert!(!filters[0].negated);
        assert_eq!(filters[0].value, "Chrome");
        assert_eq!(filters[1].dimension, breakdowns::Dimension::CountryCode);
        assert!(filters[1].negated);
        assert_eq!(filters[1].value, "US");
    }

    #[test]
    fn test_parse_filters_treats_commas_as_part_of_the_value() {
        // `;` separates filters precisely so a campaign name can contain a comma.
        let filters = parse_filters("utm-campaigns==spring,sale-2024").unwrap();
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0].value, "spring,sale-2024");
    }

    #[test]
    fn test_parse_filters_does_not_split_a_negation_on_the_equals() {
        // `!=` must be found before `==`, or `a!=b` parses as dimension `a!`.
        let filters = parse_filters("pages!=/admin").unwrap();
        assert_eq!(filters[0].dimension, breakdowns::Dimension::Page);
        assert!(filters[0].negated);
        assert_eq!(filters[0].value, "/admin");
    }

    #[test]
    fn test_parse_filters_is_empty_for_absent_or_blank_input() {
        assert!(parse_filters("").unwrap().is_empty());
        assert!(parse_filters("   ").unwrap().is_empty());
        assert!(parse_filters(";;").unwrap().is_empty());
    }

    #[test]
    fn test_parse_filters_trims_whitespace_around_both_sides() {
        let filters = parse_filters(" browsers == Chrome ; os != Linux ").unwrap();
        assert_eq!(filters[0].value, "Chrome");
        assert_eq!(filters[1].value, "Linux");
    }

    #[test]
    fn test_parse_filters_accepts_the_unknown_sentinel() {
        let filters = parse_filters(&format!("browsers=={UNKNOWN_VALUE}")).unwrap();
        assert_eq!(filters[0].value, UNKNOWN_VALUE);
    }

    #[test]
    fn test_parse_filters_rejects_malformed_input() {
        for bad in [
            "browsers",        // no operator
            "browsers=Chrome", // single '='
            "browsers==",      // empty value
            "==Chrome",        // no dimension
            "nonexistent==x",  // unknown dimension
            "entry-pages==/",  // session-derived, not a column
            "exit-pages!=/",
        ] {
            assert!(
                parse_filters(bad).is_err(),
                "{bad:?} should have been rejected"
            );
        }
    }

    #[test]
    fn test_parse_filters_error_messages_name_the_problem() {
        let err = parse_filters("nonexistent==x").unwrap_err().to_string();
        assert!(err.contains("nonexistent"), "{err}");
        assert!(
            err.contains("browsers"),
            "the error should list what is valid: {err}"
        );

        let err = parse_filters("entry-pages==/").unwrap_err().to_string();
        assert!(err.contains("session"), "{err}");
    }

    #[test]
    fn test_parse_filters_bounds_count_and_value_length() {
        let many = (0..=MAX_FILTERS)
            .map(|i| format!("pages==/p{i}"))
            .collect::<Vec<_>>()
            .join(";");
        assert!(parse_filters(&many).is_err());

        let long = format!("pages=={}", "x".repeat(MAX_FILTER_VALUE_LEN + 1));
        assert!(parse_filters(&long).is_err());
        let at_limit = format!("pages=={}", "x".repeat(MAX_FILTER_VALUE_LEN));
        assert!(parse_filters(&at_limit).is_ok());
    }

    #[test]
    fn test_filters_cache_key_is_order_independent() {
        let a = parse_filters("browsers==Chrome;countries==DE").unwrap();
        let b = parse_filters("countries==DE;browsers==Chrome").unwrap();
        assert_eq!(filters_cache_key(&a), filters_cache_key(&b));
    }

    #[test]
    fn test_filters_cache_key_separates_different_segments() {
        let unfiltered = filters_cache_key(&[]);
        let chrome = filters_cache_key(&parse_filters("browsers==Chrome").unwrap());
        let not_chrome = filters_cache_key(&parse_filters("browsers!=Chrome").unwrap());
        let firefox = filters_cache_key(&parse_filters("browsers==Firefox").unwrap());
        let keys = [&unfiltered, &chrome, &not_chrome, &firefox];
        for (i, x) in keys.iter().enumerate() {
            for y in keys.iter().skip(i + 1) {
                assert_ne!(x, y, "two different segments share a cache key");
            }
        }
    }

    fn params(period: &str) -> StatsParams {
        StatsParams {
            site_id: "test.com".to_string(),
            period: period.to_string(),
            start_date: None,
            end_date: None,
            limit: None,
            filters: None,
        }
    }

    // ── Date ranges ──────────────────────────────────────────────────────

    #[test]
    fn test_all_named_periods_resolve() {
        for period in ["day", "today", "7d", "30d", "90d", "12mo"] {
            assert!(params(period).date_range().is_ok(), "period {period}");
        }
    }

    #[test]
    fn test_invalid_period_rejected() {
        let err = params("last-tuesday").date_range().unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn test_explicit_range_end_is_made_exclusive() {
        // Regression: an inclusive end_date was passed straight through as the
        // exclusive bound, so the final day of every explicit range was omitted.
        let p = StatsParams {
            start_date: Some("2024-01-01".to_string()),
            end_date: Some("2024-01-31".to_string()),
            ..params("custom")
        };
        let range = p.date_range().unwrap();
        assert_eq!(range.start, "2024-01-01");
        assert_eq!(range.end, "2024-02-01", "end_date must be included");
        assert_eq!(range.days, 31);
    }

    #[test]
    fn test_single_day_explicit_range() {
        let p = StatsParams {
            start_date: Some("2024-01-15".to_string()),
            end_date: Some("2024-01-15".to_string()),
            ..params("custom")
        };
        let range = p.date_range().unwrap();
        assert_eq!(range.start, "2024-01-15");
        assert_eq!(range.end, "2024-01-16");
        assert_eq!(range.days, 1);
    }

    #[test]
    fn test_inverted_range_rejected() {
        let p = StatsParams {
            start_date: Some("2024-06-30".to_string()),
            end_date: Some("2024-01-01".to_string()),
            ..params("custom")
        };
        assert!(p.date_range().is_err());
    }

    #[test]
    fn test_over_long_range_rejected() {
        let p = StatsParams {
            start_date: Some("2000-01-01".to_string()),
            end_date: Some("2030-01-01".to_string()),
            ..params("custom")
        };
        assert!(p.date_range().unwrap_err().to_string().contains("366"));
    }

    #[test]
    fn test_malformed_date_rejected() {
        let p = StatsParams {
            start_date: Some("not-a-date".to_string()),
            end_date: Some("2024-01-01".to_string()),
            ..params("custom")
        };
        assert!(p.date_range().is_err());
    }

    #[test]
    fn test_period_span_drives_granularity() {
        assert_eq!(
            timeseries::Granularity::for_span_days(params("day").date_range().unwrap().days),
            timeseries::Granularity::Hour
        );
        assert_eq!(
            timeseries::Granularity::for_span_days(params("30d").date_range().unwrap().days),
            timeseries::Granularity::Day
        );
    }

    // ── Limits ───────────────────────────────────────────────────────────

    #[test]
    fn test_limit_defaults_and_caps() {
        assert_eq!(params("30d").limit_or(10, 1000).unwrap(), 10);

        let p = StatsParams {
            limit: Some(50),
            ..params("30d")
        };
        assert_eq!(p.limit_or(10, 1000).unwrap(), 50);
    }

    #[test]
    fn test_excessive_limit_is_an_error_not_a_silent_clamp() {
        let p = StatsParams {
            limit: Some(999_999),
            ..params("30d")
        };
        assert!(
            p.limit_or(10, 1000)
                .unwrap_err()
                .to_string()
                .contains("1000")
        );
    }

    #[test]
    fn test_zero_limit_rejected() {
        let p = StatsParams {
            limit: Some(0),
            ..params("30d")
        };
        assert!(p.limit_or(10, 1000).is_err());
    }

    // ── site_id validation ───────────────────────────────────────────────

    #[test]
    fn test_validate_site_id_accepts_realistic_ids() {
        for id in ["example.com", "my-site.co.uk", "localhost:8080", "my_site"] {
            assert!(validate_site_id(id).is_ok(), "{id}");
        }
    }

    #[test]
    fn test_validate_site_id_rejects_dangerous_input() {
        for id in ["", "example.com/path", "site id", "site\0null", "../etc"] {
            assert!(validate_site_id(id).is_err(), "{id} must be rejected");
        }
        assert!(validate_site_id(&"a".repeat(257)).is_err());
    }

    // ── Funnel step parsing ──────────────────────────────────────────────

    #[test]
    fn test_parse_funnel_step() {
        assert_eq!(
            parse_funnel_step("page:/pricing").unwrap(),
            "pathname = '/pricing'"
        );
        assert_eq!(
            parse_funnel_step("event:signup").unwrap(),
            "event_name = 'signup'"
        );
    }

    #[test]
    fn test_parse_funnel_step_escapes_quotes() {
        assert_eq!(
            parse_funnel_step("page:/it's").unwrap(),
            "pathname = '/it''s'"
        );
    }

    #[test]
    fn test_parse_funnel_step_rejects_other_shapes() {
        assert!(parse_funnel_step("invalid").is_err());
        assert!(parse_funnel_step("sql:DROP TABLE").is_err());
        assert!(parse_funnel_step("page:").is_err());
        assert!(parse_funnel_step(&format!("page:{}", "a".repeat(257))).is_err());
    }

    #[test]
    fn test_parse_steps_enforces_bounds() {
        assert!(parse_steps("page:/a,page:/b", 2, 32).is_ok());
        assert!(parse_steps("page:/a", 2, 32).is_err());
        assert!(parse_steps("", 2, 32).is_err());
        // Trailing separators must not count as steps.
        assert_eq!(parse_steps("page:/a,page:/b,", 2, 32).unwrap().len(), 2);
    }

    // ── Interval validation ──────────────────────────────────────────────

    #[test]
    fn test_is_safe_interval() {
        for good in ["1 day", "2 hours", "30 minutes", "7 days", "52 weeks"] {
            assert!(is_safe_interval(good), "{good}");
        }
        for bad in [
            "1",
            "day",
            "0 days",
            "1 day; DROP TABLE",
            "999 days",
            "365 weeks",
            "1 fortnight",
        ] {
            assert!(!is_safe_interval(bad), "{bad} must be rejected");
        }
    }
}
