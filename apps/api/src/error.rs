//! HTTP representation of core errors.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use omnion_core::CoreError;
use omnion_identity::IdentityError;
use serde::Serialize;

/// Error response shape used across `/api/v1`:
/// `{"error":{"code":"internal_error","message":"…"}}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    /// Build an error with an explicit status, code and message.
    #[must_use]
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    /// `400` — the request body or its parameters are unusable.
    #[must_use]
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    /// `401` — the caller is not signed in, or the session is gone.
    #[must_use]
    pub fn unauthorized(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code, message)
    }

    /// `403` — the caller is signed in but the action is not allowed.
    #[must_use]
    pub fn forbidden(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, message)
    }

    /// Map a core error onto the API surface.
    ///
    /// A dependency that did not answer becomes `503` (retryable); everything else is an
    /// internal `500` — the client can do nothing about it, but the operator can.
    #[must_use]
    pub fn from_core(error: CoreError) -> Self {
        match error {
            CoreError::Unavailable {
                dependency,
                message,
            } => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "dependency_unavailable",
                message: format!("{dependency}: {message}"),
            },
            other => Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "internal_error",
                message: other.to_string(),
            },
        }
    }

    /// HTTP status this error maps to.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Stable, machine-readable error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl From<CoreError> for ApiError {
    fn from(error: CoreError) -> Self {
        Self::from_core(error)
    }
}

impl From<IdentityError> for ApiError {
    fn from(error: IdentityError) -> Self {
        match error {
            // A pool that cannot hand out a connection is a retryable infrastructure
            // failure; everything else is an internal error the operator has to look at.
            IdentityError::Database(
                sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_),
            ) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "dependency_unavailable",
                "database is unavailable",
            ),
            other => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                other.to_string(),
            ),
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}
#[derive(Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code,
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_dependency_maps_to_503() {
        let error = ApiError::from(CoreError::Unavailable {
            dependency: "redis",
            message: "PING timed out".to_owned(),
        });
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.code(), "dependency_unavailable");
    }

    #[test]
    fn other_core_errors_map_to_500() {
        let error = ApiError::from(CoreError::Telemetry("boom".to_owned()));
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.code(), "internal_error");
    }

    #[test]
    fn helpers_pick_the_right_status() {
        assert_eq!(
            ApiError::bad_request("invalid_request", "no").status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::unauthorized("unauthenticated", "no").status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ApiError::forbidden("account_disabled", "no").status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn exhausted_pool_maps_to_503_but_other_identity_errors_to_500() {
        let unavailable = ApiError::from(IdentityError::Database(sqlx::Error::PoolTimedOut));
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(unavailable.code(), "dependency_unavailable");

        let internal = ApiError::from(IdentityError::EmailTaken);
        assert_eq!(internal.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(internal.code(), "internal_error");
        assert_eq!(internal.message, "email address is already registered");
    }
}
