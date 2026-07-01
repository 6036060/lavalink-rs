//! Lavalink v4 互換サーバーのライブラリ部。
//! バイナリ(`main.rs`)と統合テスト(`tests/`)の両方から `build_app` を使う。

pub mod auth;
pub mod config;
pub mod error;
pub mod playback;
pub mod routes;
pub mod state;
pub mod ws;

pub use state::{AppState, SharedState};

use axum::{
    middleware,
    routing::{get, patch, post},
    Router,
};

/// 認証付き `/v4` ルートと `/version` を組み立て、状態を適用した Router を返す。
pub fn build_app(state: SharedState) -> Router {
    let v4 = Router::new()
        .route("/info", get(routes::info))
        .route("/stats", get(routes::stats))
        .route("/loadtracks", get(routes::load_tracks))
        .route("/decodetrack", get(routes::decode_track))
        .route("/decodetracks", post(routes::decode_tracks))
        .route("/sessions/{session_id}", patch(routes::update_session))
        .route("/sessions/{session_id}/players", get(routes::get_players))
        .route(
            "/sessions/{session_id}/players/{guild_id}",
            get(routes::get_player)
                .patch(routes::update_player)
                .delete(routes::destroy_player),
        )
        .route("/websocket", get(ws::websocket))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_auth));

    Router::new()
        .route("/version", get(routes::version))
        .nest("/v4", v4)
        .with_state(state)
}
