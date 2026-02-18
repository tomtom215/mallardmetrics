use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

/// API error type with HTTP status code mapping.
#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    #[allow(dead_code)]
    Unauthorized(String),
    #[allow(dead_code)]
    NotFound(String),
    #[allow(dead_code)]
    Internal(String),
    DatabaseError(duckdb::Error),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            Self::Unauthorized(msg) => write!(f, "Unauthorized: {msg}"),
            Self::NotFound(msg) => write!(f, "Not found: {msg}"),
            Self::Internal(msg) => write!(f, "Internal error: {msg}"),
            Self::DatabaseError(e) => write!(f, "Database error: {e}"),
        }
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            Self::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            Self::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
            Self::DatabaseError(e) => {
                tracing::error!(error = %e, "Database error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            }
        };

        let body = serde_json::json!({ "error": message });
        (status, Json(body)).into_response()
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
    use axum::response::IntoResponse;

    #[test]
    fn test_bad_request_status() {
        let err = ApiError::BadRequest("invalid input".to_string());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_not_found_status() {
        let err = ApiError::NotFound("site not found".to_string());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_internal_error_status() {
        let err = ApiError::Internal("something broke".to_string());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_display() {
        let err = ApiError::BadRequest("test".to_string());
        assert_eq!(format!("{err}"), "Bad request: test");
    }
}
