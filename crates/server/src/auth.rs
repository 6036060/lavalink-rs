//! 認証ミドルウェア（チケット 2-7）。`Authorization` ヘッダを共有パスワードと照合する。

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::error::ApiError;
use crate::state::SharedState;

/// タイミング攻撃を避ける固定時間比較。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn require_auth(State(state): State<SharedState>, req: Request, next: Next) -> Response {
    let provided = req.headers().get("authorization").and_then(|v| v.to_str().ok());
    let expected = state.config.lavalink.server.password.as_str();

    if provided.is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes())) {
        next.run(req).await
    } else {
        let path = req.uri().path().to_string();
        tracing::warn!(%path, has_header = provided.is_some(), "rejected: bad or missing Authorization");
        ApiError::unauthorized(path).into_response()
    }
}
