//! Lavalink 互換のエラーレスポンス（チケット: REST 共通）。

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use std::time::{SystemTime, UNIX_EPOCH};

/// Lavalink のエラー JSON を返すエラー型。
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    pub path: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>, path: impl Into<String>) -> Self {
        Self { status, message: message.into(), path: path.into() }
    }
    pub fn not_found(message: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message, path)
    }
    pub fn bad_request(message: impl Into<String>, path: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message, path)
    }
    pub fn unauthorized(path: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Unauthorized", path)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let ts =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        let body = serde_json::json!({
            "timestamp": ts,
            "status": self.status.as_u16(),
            "error": self.status.canonical_reason().unwrap_or("Error"),
            "message": self.message,
            "path": self.path,
        });
        (self.status, Json(body)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
