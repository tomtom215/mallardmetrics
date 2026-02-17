use crate::api::errors::ApiError;
use crate::ingest::handler::AppState;
use crate::query::{breakdowns, metrics, timeseries};
use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
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

impl StatsParams {
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
    let (start, end) = params.date_range()?;

    let conn = state.buffer.conn().lock();
    let result = metrics::query_core_metrics(&conn, &params.site_id, &start, &end)?;
    drop(conn);
    Ok(Json(result))
}

/// GET /api/stats/timeseries — Time-bucketed visitor/pageview counts.
pub async fn get_timeseries(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StatsParams>,
) -> Result<Json<Vec<timeseries::TimeBucket>>, ApiError> {
    let (start, end) = params.date_range()?;

    // Choose granularity based on range
    let granularity = if params.period == "day" || params.period == "today" {
        timeseries::Granularity::Hour
    } else {
        timeseries::Granularity::Day
    };

    let conn = state.buffer.conn().lock();
    let result = timeseries::query_timeseries(&conn, &params.site_id, &start, &end, granularity)?;
    drop(conn);
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
    let conn = state.buffer.conn().lock();
    let result = breakdowns::query_breakdown(
        &conn,
        &params.site_id,
        &start,
        &end,
        breakdowns::Dimension::Page,
        params.limit,
    )?;
    drop(conn);
    Ok(Json(result))
}

/// GET /api/stats/breakdown/sources — Top referrer sources breakdown.
pub async fn get_sources_breakdown(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BreakdownParams>,
) -> Result<Json<Vec<breakdowns::BreakdownRow>>, ApiError> {
    let (start, end) = params.date_range()?;
    let conn = state.buffer.conn().lock();
    let result = breakdowns::query_breakdown(
        &conn,
        &params.site_id,
        &start,
        &end,
        breakdowns::Dimension::ReferrerSource,
        params.limit,
    )?;
    drop(conn);
    Ok(Json(result))
}

/// GET /api/stats/breakdown/browsers — Browser breakdown.
pub async fn get_browsers_breakdown(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BreakdownParams>,
) -> Result<Json<Vec<breakdowns::BreakdownRow>>, ApiError> {
    let (start, end) = params.date_range()?;
    let conn = state.buffer.conn().lock();
    let result = breakdowns::query_breakdown(
        &conn,
        &params.site_id,
        &start,
        &end,
        breakdowns::Dimension::Browser,
        params.limit,
    )?;
    drop(conn);
    Ok(Json(result))
}

/// GET /api/stats/breakdown/os — OS breakdown.
pub async fn get_os_breakdown(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BreakdownParams>,
) -> Result<Json<Vec<breakdowns::BreakdownRow>>, ApiError> {
    let (start, end) = params.date_range()?;
    let conn = state.buffer.conn().lock();
    let result = breakdowns::query_breakdown(
        &conn,
        &params.site_id,
        &start,
        &end,
        breakdowns::Dimension::Os,
        params.limit,
    )?;
    drop(conn);
    Ok(Json(result))
}

/// GET /api/stats/breakdown/devices — Device type breakdown.
pub async fn get_devices_breakdown(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BreakdownParams>,
) -> Result<Json<Vec<breakdowns::BreakdownRow>>, ApiError> {
    let (start, end) = params.date_range()?;
    let conn = state.buffer.conn().lock();
    let result = breakdowns::query_breakdown(
        &conn,
        &params.site_id,
        &start,
        &end,
        breakdowns::Dimension::DeviceType,
        params.limit,
    )?;
    drop(conn);
    Ok(Json(result))
}

/// GET /api/stats/breakdown/countries — Country breakdown.
pub async fn get_countries_breakdown(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BreakdownParams>,
) -> Result<Json<Vec<breakdowns::BreakdownRow>>, ApiError> {
    let (start, end) = params.date_range()?;
    let conn = state.buffer.conn().lock();
    let result = breakdowns::query_breakdown(
        &conn,
        &params.site_id,
        &start,
        &end,
        breakdowns::Dimension::CountryCode,
        params.limit,
    )?;
    drop(conn);
    Ok(Json(result))
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
}
