use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// API error type with HTTP status-code mapping.
#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized(String),
    NotFound(String),
    Internal(String),
    DatabaseError(duckdb::Error),
    /// 429 — the concurrent-query semaphore is exhausted.
    TooManyRequests(String),
    /// 503 — the request needs the `behavioral` extension, which is not loaded.
    ///
    /// Distinct from a 500: the deployment is healthy, the feature simply is
    /// not available. Previously these requests returned `200` with an empty
    /// body, so the dashboard showed "no data" for a site full of data.
    BehavioralUnavailable(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            Self::Unauthorized(msg) => write!(f, "Unauthorized: {msg}"),
            Self::NotFound(msg) => write!(f, "Not found: {msg}"),
            Self::Internal(msg) => write!(f, "Internal error: {msg}"),
            Self::DatabaseError(e) => write!(f, "Database error: {e}"),
            Self::TooManyRequests(msg) => write!(f, "Too many requests: {msg}"),
            Self::BehavioralUnavailable(msg) => {
                write!(f, "Behavioral extension unavailable: {msg}")
            }
        }
    }
}

impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DatabaseError(e) => Some(e),
            _ => None,
        }
    }
}

impl ApiError {
    /// The status code this error maps to.
    pub const fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Internal(_) | Self::DatabaseError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::BehavioralUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// Standard error for a feature that needs the behavioral extension.
    pub fn behavioral_required(feature: &str) -> Self {
        Self::BehavioralUnavailable(format!(
            "{feature} requires the DuckDB `behavioral` community extension, which is not \
             loaded. Check /health/detailed and the server logs; the extension is installed \
             from the community repository at startup and needs outbound network access."
        ))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        // A database error's detail can carry SQL and column names, so it is
        // logged server-side and replaced with a generic message in the body.
        let message = match &self {
            Self::DatabaseError(e) => {
                tracing::error!(error = %e, "Database error");
                "Internal server error".to_string()
            }
            Self::Internal(msg) => {
                tracing::error!(error = %msg, "Internal error");
                "Internal server error".to_string()
            }
            Self::BadRequest(msg)
            | Self::Unauthorized(msg)
            | Self::NotFound(msg)
            | Self::TooManyRequests(msg)
            | Self::BehavioralUnavailable(msg) => msg.clone(),
        };

        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

impl From<duckdb::Error> for ApiError {
    fn from(e: duckdb::Error) -> Self {
        Self::DatabaseError(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn test_status_mapping() {
        assert_eq!(
            ApiError::BadRequest(String::new()).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::Unauthorized(String::new()).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ApiError::NotFound(String::new()).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::Internal(String::new()).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            ApiError::TooManyRequests(String::new()).status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            ApiError::behavioral_required("funnels").status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn test_bad_request_body_keeps_the_message() {
        let response = ApiError::BadRequest("bad period".to_string()).into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["error"], "bad period");
    }

    #[tokio::test]
    async fn test_internal_errors_do_not_leak_detail() {
        let response =
            ApiError::Internal("connection string with a secret".to_string()).into_response();
        let body = body_json(response).await;
        assert_eq!(body["error"], "Internal server error");
        assert!(!body["error"].as_str().unwrap().contains("secret"));
    }

    #[tokio::test]
    async fn test_behavioral_error_explains_the_cause() {
        let response = ApiError::behavioral_required("Funnel analysis").into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(response).await;
        let message = body["error"].as_str().unwrap();
        assert!(message.contains("Funnel analysis"));
        assert!(message.contains("behavioral"));
    }

    #[test]
    fn test_database_error_exposes_source() {
        let err = ApiError::from(duckdb::Error::QueryReturnedNoRows);
        assert!(std::error::Error::source(&err).is_some());
    }
}
