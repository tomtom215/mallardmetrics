use crate::api::errors::ApiError;
use crate::ingest::handler::AppState;
use crate::query::{breakdowns, flow, funnel, metrics, retention, sequences, sessions, timeseries};
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::sync::Arc;

/// Query parameters for stats endpoints.
#[derive(Debug, Deserialize)]
pub struct StatsParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
}

fn default_period() -> String {
    "30d".to_string()
}

/// Validate that a `site_id` parameter is safe for use in queries and storage.
///
/// - Must be non-empty and at most 256 bytes.
/// - Must contain only alphanumeric ASCII characters or `.`, `-`, `_`, `:`.
///
/// Used by both the stats API handlers and the ingest handler to ensure a
/// domain accepted at ingestion is also queryable through the stats API.
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
    let valid = site_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'));
    if !valid {
        return Err(ApiError::BadRequest(
            "site_id may only contain alphanumeric characters, '.', '-', '_', ':'".to_string(),
        ));
    }
    Ok(())
}

impl StatsParams {
    /// Resolve the start and end dates from the period or explicit params.
    ///
    /// Also validates `site_id` format.
    pub fn validate_and_date_range(&self) -> Result<(String, String), ApiError> {
        validate_site_id(&self.site_id)?;
        self.date_range()
    }

    /// Resolve the start and end dates from the period or explicit params.
    pub fn date_range(&self) -> Result<(String, String), ApiError> {
        if let (Some(start), Some(end)) = (&self.start_date, &self.end_date) {
            return Ok((start.clone(), end.clone()));
        }

        let now = chrono::Utc::now().date_naive();
        let (start, end) = match self.period.as_str() {
            "day" | "today" => (now, now + chrono::Days::new(1)),
            "7d" => (now - chrono::Days::new(7), now + chrono::Days::new(1)),
            "30d" => (now - chrono::Days::new(30), now + chrono::Days::new(1)),
            "90d" => (now - chrono::Days::new(90), now + chrono::Days::new(1)),
            _ => {
                return Err(ApiError::BadRequest(format!(
                    "Invalid period: {}. Use 'day', '7d', '30d', '90d', or provide start_date and end_date.",
                    self.period
                )));
            }
        };

        Ok((start.to_string(), end.to_string()))
    }
}

/// GET /api/stats/main — Core metrics (visitors, pageviews, bounce rate, etc.)
pub async fn get_main_stats(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatsParams>,
) -> Result<Json<metrics::CoreMetrics>, ApiError> {
    let (start, end) = params.validate_and_date_range()?;
    let cache_key = format!("main:{}:{}:{}", params.site_id, start, end);

    if let Some(cached) = state.query_cache.get(&cache_key) {
        if let Ok(val) = serde_json::from_str(&cached) {
            return Ok(Json(val));
        }
    }

    let site_id = params.site_id.clone();
    let state2 = Arc::clone(&state);
    let result = tokio::task::spawn_blocking(move || {
        let conn = state2.buffer.conn().lock();
        metrics::query_core_metrics(&conn, &site_id, &start, &end)
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))??;

    if let Ok(serialized) = serde_json::to_string(&result) {
        state.query_cache.insert(cache_key, serialized);
    }
    Ok(Json(result))
}

/// GET /api/stats/timeseries — Time-bucketed visitor/pageview counts.
pub async fn get_timeseries(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatsParams>,
) -> Result<Json<Vec<timeseries::TimeBucket>>, ApiError> {
    let (start, end) = params.validate_and_date_range()?;

    // Choose granularity based on range
    let granularity = if params.period == "day" || params.period == "today" {
        timeseries::Granularity::Hour
    } else {
        timeseries::Granularity::Day
    };

    let cache_key = format!("ts:{}:{}:{}:{granularity:?}", params.site_id, start, end);
    if let Some(cached) = state.query_cache.get(&cache_key) {
        if let Ok(val) = serde_json::from_str(&cached) {
            return Ok(Json(val));
        }
    }

    let site_id = params.site_id.clone();
    let state2 = Arc::clone(&state);
    let result = tokio::task::spawn_blocking(move || {
        let conn = state2.buffer.conn().lock();
        timeseries::query_timeseries(&conn, &site_id, &start, &end, granularity)
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))??;

    if let Ok(serialized) = serde_json::to_string(&result) {
        state.query_cache.insert(cache_key, serialized);
    }
    Ok(Json(result))
}

/// Query parameters for breakdown endpoints.
#[derive(Debug, Deserialize)]
pub struct BreakdownParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

const fn default_limit() -> usize {
    10
}

impl BreakdownParams {
    fn date_range(&self) -> Result<(String, String), ApiError> {
        validate_site_id(&self.site_id)?;
        let stats_params = StatsParams {
            site_id: self.site_id.clone(),
            period: self.period.clone(),
            start_date: self.start_date.clone(),
            end_date: self.end_date.clone(),
        };
        stats_params.date_range()
    }
}

/// GET /api/stats/breakdown/pages — Top pages breakdown.
pub async fn get_pages_breakdown(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BreakdownParams>,
) -> Result<Json<Vec<breakdowns::BreakdownRow>>, ApiError> {
    let (start, end) = params.date_range()?;
    let site_id = params.site_id.clone();
    let limit = params.limit;
    let result = tokio::task::spawn_blocking(move || {
        let conn = state.buffer.conn().lock();
        breakdowns::query_breakdown(
            &conn,
            &site_id,
            &start,
            &end,
            breakdowns::Dimension::Page,
            limit,
        )
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))??;
    Ok(Json(result))
}

/// GET /api/stats/breakdown/sources — Top referrer sources breakdown.
pub async fn get_sources_breakdown(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BreakdownParams>,
) -> Result<Json<Vec<breakdowns::BreakdownRow>>, ApiError> {
    let (start, end) = params.date_range()?;
    let site_id = params.site_id.clone();
    let limit = params.limit;
    let result = tokio::task::spawn_blocking(move || {
        let conn = state.buffer.conn().lock();
        breakdowns::query_breakdown(
            &conn,
            &site_id,
            &start,
            &end,
            breakdowns::Dimension::ReferrerSource,
            limit,
        )
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))??;
    Ok(Json(result))
}

/// GET /api/stats/breakdown/browsers — Browser breakdown.
pub async fn get_browsers_breakdown(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BreakdownParams>,
) -> Result<Json<Vec<breakdowns::BreakdownRow>>, ApiError> {
    let (start, end) = params.date_range()?;
    let site_id = params.site_id.clone();
    let limit = params.limit;
    let result = tokio::task::spawn_blocking(move || {
        let conn = state.buffer.conn().lock();
        breakdowns::query_breakdown(
            &conn,
            &site_id,
            &start,
            &end,
            breakdowns::Dimension::Browser,
            limit,
        )
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))??;
    Ok(Json(result))
}

/// GET /api/stats/breakdown/os — OS breakdown.
pub async fn get_os_breakdown(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BreakdownParams>,
) -> Result<Json<Vec<breakdowns::BreakdownRow>>, ApiError> {
    let (start, end) = params.date_range()?;
    let site_id = params.site_id.clone();
    let limit = params.limit;
    let result = tokio::task::spawn_blocking(move || {
        let conn = state.buffer.conn().lock();
        breakdowns::query_breakdown(
            &conn,
            &site_id,
            &start,
            &end,
            breakdowns::Dimension::Os,
            limit,
        )
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))??;
    Ok(Json(result))
}

/// GET /api/stats/breakdown/devices — Device type breakdown.
pub async fn get_devices_breakdown(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BreakdownParams>,
) -> Result<Json<Vec<breakdowns::BreakdownRow>>, ApiError> {
    let (start, end) = params.date_range()?;
    let site_id = params.site_id.clone();
    let limit = params.limit;
    let result = tokio::task::spawn_blocking(move || {
        let conn = state.buffer.conn().lock();
        breakdowns::query_breakdown(
            &conn,
            &site_id,
            &start,
            &end,
            breakdowns::Dimension::DeviceType,
            limit,
        )
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))??;
    Ok(Json(result))
}

/// GET /api/stats/breakdown/countries — Country breakdown.
pub async fn get_countries_breakdown(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BreakdownParams>,
) -> Result<Json<Vec<breakdowns::BreakdownRow>>, ApiError> {
    let (start, end) = params.date_range()?;
    let site_id = params.site_id.clone();
    let limit = params.limit;
    let result = tokio::task::spawn_blocking(move || {
        let conn = state.buffer.conn().lock();
        breakdowns::query_breakdown(
            &conn,
            &site_id,
            &start,
            &end,
            breakdowns::Dimension::CountryCode,
            limit,
        )
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))??;
    Ok(Json(result))
}

/// GET /api/stats/sessions — Session metrics (requires behavioral extension).
pub async fn get_sessions(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatsParams>,
) -> Result<Json<sessions::SessionMetrics>, ApiError> {
    let (start, end) = params.validate_and_date_range()?;
    let site_id = params.site_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = state.buffer.conn().lock();
        sessions::query_session_metrics(&conn, &site_id, &start, &end).unwrap_or(
            sessions::SessionMetrics {
                total_sessions: 0,
                avg_session_duration_secs: 0.0,
                avg_pages_per_session: 0.0,
            },
        )
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))?;
    Ok(Json(result))
}

/// Query parameters for the funnel endpoint.
#[derive(Debug, Deserialize)]
pub struct FunnelParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    #[serde(default = "default_window")]
    pub window: String,
    /// Comma-separated list of step types. Each step is `page:<path>` or `event:<name>`.
    pub steps: String,
}

fn default_window() -> String {
    "1 day".to_string()
}

impl FunnelParams {
    fn date_range(&self) -> Result<(String, String), ApiError> {
        validate_site_id(&self.site_id)?;
        let stats_params = StatsParams {
            site_id: self.site_id.clone(),
            period: self.period.clone(),
            start_date: self.start_date.clone(),
            end_date: self.end_date.clone(),
        };
        stats_params.date_range()
    }
}

/// Parse a safe funnel step condition from a structured step definition.
///
/// Accepts `page:/path` or `event:name` formats only. Returns a SQL boolean
/// expression using only safe, known column comparisons.
fn parse_funnel_step(step: &str) -> Result<String, ApiError> {
    let step = step.trim();
    if let Some(path) = step.strip_prefix("page:") {
        if path.is_empty() || path.len() > 256 {
            return Err(ApiError::BadRequest("Invalid page path".to_string()));
        }
        // Escape single quotes to prevent injection
        let escaped = path.replace('\'', "''");
        Ok(format!("pathname = '{escaped}'"))
    } else if let Some(name) = step.strip_prefix("event:") {
        if name.is_empty() || name.len() > 256 {
            return Err(ApiError::BadRequest("Invalid event name".to_string()));
        }
        let escaped = name.replace('\'', "''");
        Ok(format!("event_name = '{escaped}'"))
    } else {
        Err(ApiError::BadRequest(format!(
            "Invalid step format: '{step}'. Use 'page:/path' or 'event:name'."
        )))
    }
}

/// GET /api/stats/funnel — Funnel analysis (requires behavioral extension).
pub async fn get_funnel(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FunnelParams>,
) -> Result<Json<Vec<funnel::FunnelStep>>, ApiError> {
    let (start, end) = params.date_range()?;

    // Validate window interval format (only allow simple intervals)
    let window = params.window.trim().to_string();
    if !is_safe_interval(&window) {
        return Err(ApiError::BadRequest(
            "Invalid window interval. Use e.g. '1 day', '2 hours', '30 minutes'.".to_string(),
        ));
    }

    // Parse step definitions into safe SQL conditions
    let step_strs: Vec<String> = params
        .steps
        .split(',')
        .map(parse_funnel_step)
        .collect::<Result<Vec<_>, _>>()?;

    if step_strs.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let site_id = params.site_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = state.buffer.conn().lock();
        let step_refs: Vec<&str> = step_strs.iter().map(String::as_str).collect();
        funnel::query_funnel(&conn, &site_id, &start, &end, &window, &step_refs).unwrap_or_default()
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))?;
    Ok(Json(result))
}

/// Validate that an interval string is a safe, simple DuckDB interval.
fn is_safe_interval(s: &str) -> bool {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 2 {
        return false;
    }
    let Ok(n) = parts[0].parse::<u32>() else {
        return false;
    };
    if n == 0 || n > 365 {
        return false;
    }
    matches!(
        parts[1],
        "second"
            | "seconds"
            | "minute"
            | "minutes"
            | "hour"
            | "hours"
            | "day"
            | "days"
            | "week"
            | "weeks"
    )
}

/// Query parameters for the retention endpoint.
#[derive(Debug, Deserialize)]
pub struct RetentionParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    #[serde(default = "default_num_weeks")]
    pub weeks: u32,
}

const fn default_num_weeks() -> u32 {
    4
}

impl RetentionParams {
    fn date_range(&self) -> Result<(String, String), ApiError> {
        validate_site_id(&self.site_id)?;
        let stats_params = StatsParams {
            site_id: self.site_id.clone(),
            period: self.period.clone(),
            start_date: self.start_date.clone(),
            end_date: self.end_date.clone(),
        };
        stats_params.date_range()
    }
}

/// GET /api/stats/retention — Retention cohort analysis (requires behavioral extension).
pub async fn get_retention(
    State(state): State<Arc<AppState>>,
    Query(params): Query<RetentionParams>,
) -> Result<Json<Vec<retention::RetentionCohort>>, ApiError> {
    let (start, end) = params.date_range()?;

    if params.weeks == 0 || params.weeks > 52 {
        return Err(ApiError::BadRequest(
            "weeks must be between 1 and 52".to_string(),
        ));
    }

    let site_id = params.site_id.clone();
    let weeks = params.weeks;
    let result = tokio::task::spawn_blocking(move || {
        let conn = state.buffer.conn().lock();
        retention::query_retention(&conn, &site_id, &start, &end, weeks).unwrap_or_default()
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))?;
    Ok(Json(result))
}

/// Query parameters for the sequence endpoint.
#[derive(Debug, Deserialize)]
pub struct SequenceParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    /// Comma-separated steps in `page:/path` or `event:name` format.
    pub steps: String,
}

impl SequenceParams {
    fn date_range(&self) -> Result<(String, String), ApiError> {
        validate_site_id(&self.site_id)?;
        let stats_params = StatsParams {
            site_id: self.site_id.clone(),
            period: self.period.clone(),
            start_date: self.start_date.clone(),
            end_date: self.end_date.clone(),
        };
        stats_params.date_range()
    }
}

/// Sequence match result for API response.
#[derive(Debug, Serialize)]
pub struct SequenceMatchResponse {
    pub converting_visitors: u64,
    pub total_visitors: u64,
    pub conversion_rate: f64,
}

/// GET /api/stats/sequences — Sequence match analysis (requires behavioral extension).
pub async fn get_sequences(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SequenceParams>,
) -> Result<Json<SequenceMatchResponse>, ApiError> {
    let (start, end) = params.date_range()?;

    // Parse step definitions into safe SQL conditions
    let step_strs: Vec<String> = params
        .steps
        .split(',')
        .map(parse_funnel_step)
        .collect::<Result<Vec<_>, _>>()?;

    if step_strs.len() < 2 {
        return Err(ApiError::BadRequest(
            "At least 2 steps required for sequence analysis".to_string(),
        ));
    }

    let site_id = params.site_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = state.buffer.conn().lock();
        let step_refs: Vec<&str> = step_strs.iter().map(String::as_str).collect();
        sequences::execute_sequence_match(&conn, &site_id, &start, &end, &step_refs).unwrap_or(
            sequences::SequenceMatchResult {
                converting_visitors: 0,
                total_visitors: 0,
                conversion_rate: 0.0,
            },
        )
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))?;
    Ok(Json(SequenceMatchResponse {
        converting_visitors: result.converting_visitors,
        total_visitors: result.total_visitors,
        conversion_rate: result.conversion_rate,
    }))
}

/// Query parameters for the flow endpoint.
#[derive(Debug, Deserialize)]
pub struct FlowParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    /// The page to analyze flow from.
    pub page: String,
}

impl FlowParams {
    fn date_range(&self) -> Result<(String, String), ApiError> {
        validate_site_id(&self.site_id)?;
        let stats_params = StatsParams {
            site_id: self.site_id.clone(),
            period: self.period.clone(),
            start_date: self.start_date.clone(),
            end_date: self.end_date.clone(),
        };
        stats_params.date_range()
    }
}

/// GET /api/stats/flow — Flow analysis showing next pages (requires behavioral extension).
pub async fn get_flow(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FlowParams>,
) -> Result<Json<Vec<flow::FlowNode>>, ApiError> {
    let (start, end) = params.date_range()?;

    if params.page.is_empty() || params.page.len() > 256 {
        return Err(ApiError::BadRequest("Invalid page path".to_string()));
    }

    let site_id = params.site_id.clone();
    let page = params.page.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = state.buffer.conn().lock();
        flow::query_flow(&conn, &site_id, &start, &end, &page).unwrap_or_default()
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Query task panicked: {e}")))?;
    Ok(Json(result))
}

/// Maximum number of days allowed for an explicit-date export request.
///
/// Prevents unbounded in-memory accumulation of daily rows when the caller
/// supplies `start_date` + `end_date` directly.  Period-based requests
/// (`day` / `7d` / `30d` / `90d`) are already bounded; only explicit ranges
/// need this guard.
const MAX_EXPORT_DAYS: i64 = 366;

/// Query parameters for the export endpoint.
#[derive(Debug, Deserialize)]
pub struct ExportParams {
    pub site_id: String,
    #[serde(default = "default_period")]
    pub period: String,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    /// Export format: "csv" (default) or "json"
    #[serde(default = "default_export_format")]
    pub format: String,
}

fn default_export_format() -> String {
    "csv".to_string()
}

impl ExportParams {
    fn date_range(&self) -> Result<(String, String), ApiError> {
        validate_site_id(&self.site_id)?;

        // When explicit dates are provided, validate their format and enforce the
        // maximum range to prevent building an arbitrarily large in-memory result.
        if let (Some(start_str), Some(end_str)) = (&self.start_date, &self.end_date) {
            let start_date =
                chrono::NaiveDate::parse_from_str(start_str, "%Y-%m-%d").map_err(|_| {
                    ApiError::BadRequest("Invalid start_date format. Use YYYY-MM-DD.".to_string())
                })?;
            let end_date =
                chrono::NaiveDate::parse_from_str(end_str, "%Y-%m-%d").map_err(|_| {
                    ApiError::BadRequest("Invalid end_date format. Use YYYY-MM-DD.".to_string())
                })?;
            let days = (end_date - start_date).num_days();
            if days < 0 {
                return Err(ApiError::BadRequest(
                    "end_date must be on or after start_date".to_string(),
                ));
            }
            if days > MAX_EXPORT_DAYS {
                return Err(ApiError::BadRequest(format!(
                    "Export date range must not exceed {MAX_EXPORT_DAYS} days. \
                     Use the period parameter or a shorter explicit range."
                )));
            }
        }

        let stats_params = StatsParams {
            site_id: self.site_id.clone(),
            period: self.period.clone(),
            start_date: self.start_date.clone(),
            end_date: self.end_date.clone(),
        };
        stats_params.date_range()
    }
}

/// A single row for the export response.
#[derive(Debug, Serialize)]
struct ExportRow {
    date: String,
    visitors: u64,
    pageviews: u64,
    top_page: String,
    top_source: String,
}

/// GET /api/stats/export — Export analytics data as CSV or JSON.
pub async fn get_export(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ExportParams>,
) -> Result<impl IntoResponse, ApiError> {
    let (start, end) = params.date_range()?;
    let site_id = params.site_id.clone();

    // Run all three queries together on a blocking thread so the DuckDB mutex
    // is acquired once and no Tokio worker is blocked.
    let (ts_data, top_pages, top_sources) = tokio::task::spawn_blocking(move || {
        let conn = state.buffer.conn().lock();
        let ts = timeseries::query_timeseries(
            &conn,
            &site_id,
            &start,
            &end,
            timeseries::Granularity::Day,
        )?;
        let pages = breakdowns::query_breakdown(
            &conn,
            &site_id,
            &start,
            &end,
            breakdowns::Dimension::Page,
            1,
        )?;
        let sources = breakdowns::query_breakdown(
            &conn,
            &site_id,
            &start,
            &end,
            breakdowns::Dimension::ReferrerSource,
            1,
        )?;
        drop(conn);
        Ok::<_, ApiError>((ts, pages, sources))
    })
    .await
    .map_err(|e| ApiError::Internal(format!("Export task panicked: {e}")))??;

    let top_page = top_pages
        .first()
        .map_or("(none)", |r| r.value.as_str())
        .to_string();
    let top_source = top_sources
        .first()
        .map_or("(direct)", |r| r.value.as_str())
        .to_string();

    let rows: Vec<ExportRow> = ts_data
        .iter()
        .map(|b| ExportRow {
            date: b.date.clone(),
            visitors: b.visitors,
            pageviews: b.pageviews,
            top_page: top_page.clone(),
            top_source: top_source.clone(),
        })
        .collect();

    match params.format.as_str() {
        "json" => {
            let body = serde_json::to_string(&rows)
                .map_err(|e| ApiError::Internal(format!("JSON serialization failed: {e}")))?;
            Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
        }
        "csv" => {
            let mut csv = String::from("date,visitors,pageviews,top_page,top_source\n");
            for row in &rows {
                let _ = writeln!(
                    csv,
                    "{},{},{},{},{}",
                    row.date,
                    row.visitors,
                    row.pageviews,
                    escape_csv_field(&row.top_page),
                    escape_csv_field(&row.top_source),
                );
            }
            Ok((
                [
                    (header::CONTENT_TYPE, "text/csv"),
                    (
                        header::CONTENT_DISPOSITION,
                        "attachment; filename=\"export.csv\"",
                    ),
                ],
                csv,
            )
                .into_response())
        }
        other => Err(ApiError::BadRequest(format!(
            "Invalid format: '{other}'. Use 'csv' or 'json'."
        ))),
    }
}

/// Escape a CSV field to prevent CSV injection attacks.
///
/// Wraps the field in double quotes and escapes internal double quotes.
/// Prefixes fields starting with formula-triggering characters (`=`, `+`, `-`, `@`)
/// with a single quote to neutralize them in spreadsheet applications.
fn escape_csv_field(field: &str) -> String {
    let escaped = field.replace('"', "\"\"");
    // Prefix formula-triggering characters to prevent CSV injection in spreadsheets
    if escaped.starts_with('=')
        || escaped.starts_with('+')
        || escaped.starts_with('-')
        || escaped.starts_with('@')
    {
        format!("\"'{escaped}\"")
    } else {
        format!("\"{escaped}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_date_range_7d() {
        let params = StatsParams {
            site_id: "test.com".to_string(),
            period: "7d".to_string(),
            start_date: None,
            end_date: None,
        };
        let (start, end) = params.date_range().unwrap();
        assert!(!start.is_empty());
        assert!(!end.is_empty());
    }

    #[test]
    fn test_date_range_custom() {
        let params = StatsParams {
            site_id: "test.com".to_string(),
            period: "custom".to_string(),
            start_date: Some("2024-01-01".to_string()),
            end_date: Some("2024-02-01".to_string()),
        };
        let (start, end) = params.date_range().unwrap();
        assert_eq!(start, "2024-01-01");
        assert_eq!(end, "2024-02-01");
    }

    #[test]
    fn test_date_range_invalid_period() {
        let params = StatsParams {
            site_id: "test.com".to_string(),
            period: "invalid".to_string(),
            start_date: None,
            end_date: None,
        };
        assert!(params.date_range().is_err());
    }

    #[test]
    fn test_date_range_all_periods() {
        for period in &["day", "today", "7d", "30d", "90d"] {
            let params = StatsParams {
                site_id: "test.com".to_string(),
                period: (*period).to_string(),
                start_date: None,
                end_date: None,
            };
            assert!(
                params.date_range().is_ok(),
                "Period '{period}' should be valid"
            );
        }
    }

    #[test]
    fn test_parse_funnel_step_page() {
        let result = parse_funnel_step("page:/pricing").unwrap();
        assert_eq!(result, "pathname = '/pricing'");
    }

    #[test]
    fn test_parse_funnel_step_event() {
        let result = parse_funnel_step("event:signup").unwrap();
        assert_eq!(result, "event_name = 'signup'");
    }

    #[test]
    fn test_parse_funnel_step_escapes_quotes() {
        let result = parse_funnel_step("page:/it's").unwrap();
        assert_eq!(result, "pathname = '/it''s'");
    }

    #[test]
    fn test_parse_funnel_step_invalid_format() {
        assert!(parse_funnel_step("invalid").is_err());
        assert!(parse_funnel_step("sql:DROP TABLE").is_err());
    }

    #[test]
    fn test_is_safe_interval_valid() {
        assert!(is_safe_interval("1 day"));
        assert!(is_safe_interval("2 hours"));
        assert!(is_safe_interval("30 minutes"));
        assert!(is_safe_interval("7 days"));
    }

    #[test]
    fn test_is_safe_interval_invalid() {
        assert!(!is_safe_interval("1"));
        assert!(!is_safe_interval("day"));
        assert!(!is_safe_interval("0 days"));
        assert!(!is_safe_interval("1 day; DROP TABLE"));
        assert!(!is_safe_interval("999 days"));
    }

    #[test]
    fn test_validate_site_id_valid() {
        assert!(validate_site_id("example.com").is_ok());
        assert!(validate_site_id("my-site.co.uk").is_ok());
        assert!(validate_site_id("localhost:8080").is_ok());
        assert!(validate_site_id("my_analytics_site").is_ok());
    }

    #[test]
    fn test_validate_site_id_empty() {
        assert!(validate_site_id("").is_err());
    }

    #[test]
    fn test_validate_site_id_too_long() {
        let long = "a".repeat(257);
        assert!(validate_site_id(&long).is_err());
    }

    #[test]
    fn test_validate_site_id_invalid_chars() {
        assert!(validate_site_id("example.com/path").is_err());
        assert!(validate_site_id("site id with spaces").is_err());
        assert!(validate_site_id("site\x00null").is_err());
    }

    #[test]
    fn test_escape_csv_field_plain() {
        assert_eq!(escape_csv_field("/about"), "\"/about\"");
    }

    #[test]
    fn test_escape_csv_field_with_quotes() {
        assert_eq!(escape_csv_field("it's \"great\""), "\"it's \"\"great\"\"\"");
    }

    #[test]
    fn test_escape_csv_field_formula_injection() {
        // Fields starting with formula characters are prefixed with a single quote
        assert_eq!(escape_csv_field("=CMD|'/c calc'"), "\"'=CMD|'/c calc'\"");
        assert_eq!(escape_csv_field("+1+2"), "\"'+1+2\"");
        assert_eq!(escape_csv_field("-1-2"), "\"'-1-2\"");
        assert_eq!(escape_csv_field("@SUM(A1)"), "\"'@SUM(A1)\"");
    }

    #[test]
    fn test_export_invalid_format() {
        let params = ExportParams {
            site_id: "test.com".to_string(),
            period: "30d".to_string(),
            start_date: None,
            end_date: None,
            format: "xml".to_string(),
        };
        let date_range = params.date_range();
        assert!(date_range.is_ok());
        // The format validation happens in the handler, so we test the validator indirectly
        assert_ne!(params.format, "csv");
        assert_ne!(params.format, "json");
    }

    // --- B4: Export date range limit tests ---

    #[test]
    fn test_export_date_range_too_long_rejected() {
        let params = ExportParams {
            site_id: "test.com".to_string(),
            period: "30d".to_string(),
            start_date: Some("2000-01-01".to_string()),
            end_date: Some("2030-01-01".to_string()),
            format: "csv".to_string(),
        };
        let err = params.date_range().unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(_)),
            "Exceeding {MAX_EXPORT_DAYS} days must return BadRequest"
        );
    }

    #[test]
    fn test_export_date_range_within_limit_allowed() {
        let params = ExportParams {
            site_id: "test.com".to_string(),
            period: "30d".to_string(),
            start_date: Some("2024-01-01".to_string()),
            end_date: Some("2024-06-30".to_string()),
            format: "csv".to_string(),
        };
        assert!(
            params.date_range().is_ok(),
            "Range under {MAX_EXPORT_DAYS} days must be accepted"
        );
    }

    #[test]
    fn test_export_date_range_end_before_start_rejected() {
        let params = ExportParams {
            site_id: "test.com".to_string(),
            period: "30d".to_string(),
            start_date: Some("2024-06-30".to_string()),
            end_date: Some("2024-01-01".to_string()),
            format: "csv".to_string(),
        };
        assert!(
            params.date_range().is_err(),
            "end_date before start_date must be rejected"
        );
    }

    #[test]
    fn test_export_date_range_invalid_format_rejected() {
        let params = ExportParams {
            site_id: "test.com".to_string(),
            period: "30d".to_string(),
            start_date: Some("not-a-date".to_string()),
            end_date: Some("2024-01-01".to_string()),
            format: "csv".to_string(),
        };
        assert!(
            params.date_range().is_err(),
            "Invalid date format must return an error"
        );
    }

    #[test]
    fn test_export_period_based_no_range_check() {
        // Period-based export (no explicit dates) must bypass the range check
        // and use the normal period resolution instead.
        let params = ExportParams {
            site_id: "test.com".to_string(),
            period: "90d".to_string(),
            start_date: None,
            end_date: None,
            format: "csv".to_string(),
        };
        assert!(
            params.date_range().is_ok(),
            "Period-based export must not be rejected by the date-range guard"
        );
    }
}
