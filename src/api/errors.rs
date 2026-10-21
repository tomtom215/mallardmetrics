use axum::Json;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;

/// API error type with HTTP status-code mapping.
#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized(String),
    NotFound(String),
    /// 409 — the request is valid but conflicts with existing state.
    Conflict(String),
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
    /// 503 — the report needs exact event timestamps and this instance stores
    /// them rounded to the hour (`round_timestamps`, enabled by `gdpr_mode`).
    ///
    /// A different cause from [`Self::BehavioralUnavailable`] with a different
    /// remedy (a privacy setting, not an install), so a client must be able to
    /// tell them apart without parsing the message.
    SessionDataUnavailable(String),
    /// 504 — the query passed its deadline and was interrupted.
    ///
    /// The caller's request was valid and the server is healthy; the work was
    /// too large for the time allowed, so a narrower range or fewer filters will
    /// succeed. A 400 would blame the caller, a 500 would page someone.
    QueryTimeout(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            Self::Unauthorized(msg) => write!(f, "Unauthorized: {msg}"),
            Self::NotFound(msg) => write!(f, "Not found: {msg}"),
            Self::Conflict(msg) => write!(f, "Conflict: {msg}"),
            Self::Internal(msg) => write!(f, "Internal error: {msg}"),
            Self::DatabaseError(e) => write!(f, "Database error: {e}"),
            Self::TooManyRequests(msg) => write!(f, "Too many requests: {msg}"),
            Self::BehavioralUnavailable(msg) => {
                write!(f, "Behavioral extension unavailable: {msg}")
            }
            Self::SessionDataUnavailable(msg) => {
                write!(f, "Session data unavailable: {msg}")
            }
            Self::QueryTimeout(msg) => write!(f, "Query timed out: {msg}"),
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
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Internal(_) | Self::DatabaseError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::BehavioralUnavailable(_) | Self::SessionDataUnavailable(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::QueryTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
        }
    }

    /// A stable, machine-readable identifier for the failure.
    ///
    /// Clients branch on this rather than on the message text, which is written
    /// for people and free to change.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::Unauthorized(_) => "unauthorized",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Internal(_) | Self::DatabaseError(_) => "internal_error",
            Self::TooManyRequests(_) => "too_many_requests",
            Self::BehavioralUnavailable(_) => "behavioral_unavailable",
            Self::SessionDataUnavailable(_) => "session_data_unavailable",
            Self::QueryTimeout(_) => "query_timeout",
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
            | Self::Conflict(msg)
            | Self::TooManyRequests(msg)
            | Self::BehavioralUnavailable(msg)
            | Self::SessionDataUnavailable(msg)
            | Self::QueryTimeout(msg) => msg.clone(),
        };

        let body = Json(serde_json::json!({ "error": message, "code": self.code() }));
        if matches!(self, Self::TooManyRequests(_)) {
            // Capacity frees as in-flight queries finish, which is seconds.
            return (status, [(axum::http::header::RETRY_AFTER, "1")], body).into_response();
        }
        (status, body).into_response()
    }
}

/// A query string extractor whose failures are [`ApiError`]s.
///
/// axum's own `Query` answers a missing or malformed parameter with a plain-text
/// 400, which is the one error response a client could not parse like the rest.
/// This reports the same message as a JSON body with `code: bad_request`.
pub struct ApiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Query(value)| Self(value))
            .map_err(|rejection| ApiError::BadRequest(rejection.body_text()))
    }
}

/// A JSON body extractor whose failures are [`ApiError`]s: the same reasoning as
/// [`ApiQuery`] for a missing, malformed or wrongly-typed request body.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(|rejection| ApiError::BadRequest(rejection.body_text()))
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
        assert_eq!(
            ApiError::QueryTimeout(String::new()).status(),
            StatusCode::GATEWAY_TIMEOUT
        );
    }

    #[tokio::test]
    async fn test_every_error_carries_a_stable_code() {
        let cases = [
            (ApiError::BadRequest("x".into()), "bad_request"),
            (ApiError::Unauthorized("x".into()), "unauthorized"),
            (ApiError::NotFound("x".into()), "not_found"),
            (ApiError::Conflict("x".into()), "conflict"),
            (ApiError::Internal("x".into()), "internal_error"),
            (
                ApiError::from(duckdb::Error::QueryReturnedNoRows),
                "internal_error",
            ),
            (ApiError::TooManyRequests("x".into()), "too_many_requests"),
            (
                ApiError::behavioral_required("Funnel analysis"),
                "behavioral_unavailable",
            ),
            (
                ApiError::SessionDataUnavailable("x".into()),
                "session_data_unavailable",
            ),
            (ApiError::QueryTimeout("x".into()), "query_timeout"),
        ];
        for (error, code) in cases {
            let response = error.into_response();
            assert_eq!(body_json(response).await["code"], code);
        }
    }

    #[test]
    fn test_only_a_busy_server_asks_the_client_to_retry() {
        let busy = ApiError::TooManyRequests("busy".into()).into_response();
        assert_eq!(
            busy.headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );
        // A missing extension needs an operator, so retrying would only repeat the failure.
        let missing = ApiError::behavioral_required("Funnel analysis").into_response();
        assert!(
            missing
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .is_none()
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
