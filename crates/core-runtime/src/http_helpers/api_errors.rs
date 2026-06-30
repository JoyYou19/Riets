use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use core_core::errors::{self, CorelamoError, ErrorCategory};
use serde_json::json;

/// Plain RFC 7807 fields. Knows nothing about request_id or wire format —
/// those get stamped on by other middleware, not this layer.
pub struct Problem {
    pub status: StatusCode,
    pub type_: String,
    pub title: String,
    pub detail: String,
}

impl Problem {
    pub fn new(status: StatusCode, code: &str, detail: impl Into<String>) -> Self {
        Problem {
            status,
            type_: format!("https://corelamo.dev/errors/{code}"),
            title: title_for(code),
            detail: detail.into(),
        }
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = self.status;
        let body = Json(json!({
            "type": self.type_,
            "title": self.title,
            "status": status.as_u16(),
            "detail": self.detail,
        }));
        (status, body).into_response()
    }
}

fn title_for(code: &str) -> String {
    match code {
        c if c == errors::ERR_VALIDATION => "Validation Error",
        c if c == errors::ERR_NOT_FOUND => "Not Found",
        c if c == errors::ERR_CONFLICT => "Conflict",
        c if c == errors::ERR_TIMEOUT => "Timeout",
        c if c == errors::ERR_UNAVAILABLE => "Service Unavailable",
        _ => "Internal Server Error",
    }
    .to_string()
}

pub struct ApiError(pub CorelamoError);

impl From<CorelamoError> for ApiError {
    fn from(err: CorelamoError) -> Self {
        ApiError(err)
    }
}
impl From<std::io::Error> for ApiError {
    fn from(err: std::io::Error) -> Self {
        ApiError(CorelamoError::from(err))
    }
}
impl From<serde_json::Error> for ApiError {
    fn from(err: serde_json::Error) -> Self {
        ApiError(CorelamoError::from(err))
    }
}

impl ApiError {
    pub fn to_problem(&self) -> Problem {
        let err = &self.0;
        let status = match err.category() {
            ErrorCategory::Validation => StatusCode::BAD_REQUEST,
            ErrorCategory::NotFound => StatusCode::NOT_FOUND,
            ErrorCategory::Conflict => StatusCode::CONFLICT,
            ErrorCategory::Timeout => StatusCode::GATEWAY_TIMEOUT,
            ErrorCategory::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            ErrorCategory::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Problem::new(status, err.code(), err.public_message())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.0.is_server_error() {
            tracing::error!(error = %self.0, code = self.0.code(), "internal error");
        }
        self.to_problem().into_response()
    }
}
