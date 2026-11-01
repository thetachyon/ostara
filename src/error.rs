//! API error type: upstream failure is normal, so every fallible handler
//! returns a JSON `{"error", "detail"}` body with a non-2xx status instead of
//! panicking. Nothing here ever blocks another feed.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// A structured API error: `{ "error": <short>, "detail": <upstream detail> }`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    error: String,
    detail: String,
}

impl ApiError {
    pub fn bad_gateway(error: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            error: error.into(),
            detail: detail.into(),
        }
    }

    pub fn bad_request(error: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: error.into(),
            detail: detail.into(),
        }
    }

    pub fn internal(error: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: error.into(),
            detail: detail.into(),
        }
    }

    /// 503 by design: a keyed feed or gated capability whose credentials /
    /// enable-switch are unset. Mirrors OSIRIS's disabled RECON backend.
    pub fn service_unavailable(error: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            error: error.into(),
            detail: detail.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({
            "error": self.error,
            "detail": self.detail,
        }));
        (self.status, body).into_response()
    }
}

/// Feed-level failure, converted to a 502 at the route boundary.
#[derive(Debug)]
pub struct FeedError {
    pub feed: &'static str,
    pub detail: String,
}

impl FeedError {
    pub fn new(feed: &'static str, detail: impl Into<String>) -> Self {
        Self {
            feed,
            detail: detail.into(),
        }
    }
}

impl From<FeedError> for ApiError {
    fn from(e: FeedError) -> Self {
        ApiError::bad_gateway(format!("upstream fetch failed: {}", e.feed), e.detail)
    }
}

impl From<reqwest::Error> for FeedError {
    fn from(e: reqwest::Error) -> Self {
        FeedError::new("http", e.to_string())
    }
}
