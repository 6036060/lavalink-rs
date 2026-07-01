//! WebSocket ハンドラと dispatch ループ（チケット 2-6）。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    http::HeaderMap,
    response::Response,
};
use futures_util::{sink::SinkExt, stream::StreamExt};
use tokio::sync::mpsc;

use lavalink_protocol::{Event, ServerMessage, TrackEndReason};

use crate::routes::build_stats;
use crate::state::{SharedState, Session, Tx};

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// `GET /v4/websocket`。認証はミドルウェアで済んでいる前提。
pub async fn websocket(
    State(state): State<SharedState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let user_id = header(&headers, "user-id").unwrap_or("").to_string();
    let resume_id = header(&headers, "session-id").map(|s| s.to_string());
    tracing::info!(%user_id, resume = ?resume_id, "websocket upgrade requested");
    ws.on_upgrade(move |socket| handle_socket(socket, state, user_id, resume_id))
}

async fn get_or_create(
    state: &SharedState,
    resume_id: Option<String>,
    user_id: String,
    tx: Tx,
) -> (Arc<Session>, bool) {
    if let Some(rid) = resume_id {
        if let Some(sess) = state.get_session(&rid).await {
            if sess.resuming.load(Ordering::SeqCst) {
                sess.attach(tx);
                return (sess, true);
            }
        }
    }
    let sess = state.create_session(user_id).await;
    sess.attach(tx);
    (sess, false)
}

async fn handle_socket(socket: WebSocket, state: SharedState, user_id: String, resume_id: Option<String>) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

    let (session, resumed) = get_or_create(&state, resume_id, user_id, tx).await;

    // 送信タスク（このソケットへの唯一の writer）。
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let txt = match serde_json::to_string(&msg) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if sink.send(Message::Text(txt.into())).await.is_err() {
                break;
            }
        }
    });

    // Ready op を送信。
    session.send(ServerMessage::Ready {
        resumed,
        session_id: session.id.clone(),
    });
    tracing::info!(session_id = %session.id, user_id = %session.user_id, resumed, "websocket connected");

    // 受信ループ（v4 ではクライアントからの制御はないため close 待ち）。
    while let Some(Ok(msg)) = stream.next().await {
        if matches!(msg, Message::Close(_)) {
            break;
        }
    }

    // 切断処理。
    writer.abort();
    session.detach();
    tracing::info!(session_id = %session.id, "websocket disconnected");

    if session.resuming.load(Ordering::SeqCst) {
        let timeout = session.timeout_secs.load(Ordering::SeqCst);
        let state2 = state.clone();
        let id = session.id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(timeout)).await;
            if let Some(s) = state2.get_session(&id).await {
                if !s.is_attached() {
                    state2.remove_session(&id).await;
                    tracing::info!(session_id = %id, "resumable session timed out");
                }
            }
        });
    } else {
        state.remove_session(&session.id).await;
    }
}

/// 1 秒間隔のグローバル dispatch ループ:
/// - トラック終了検知 → TrackEnd(finished)
/// - playerUpdate（設定間隔ごと）
/// - stats（60 秒ごと）
pub async fn dispatcher(state: SharedState) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let mut secs: u64 = 0;
    loop {
        ticker.tick().await;
        secs += 1;
        let interval = state.config.lavalink.server.player_update_interval.max(1);

        let stats = if secs % 60 == 0 {
            Some(build_stats(&state).await)
        } else {
            None
        };

        let sessions: Vec<Arc<Session>> = state.sessions.read().await.values().cloned().collect();
        for sess in sessions {
            if !sess.is_attached() {
                continue;
            }
            {
                let mut players = sess.players.lock().await;
                let mut finished: Vec<(String, lavalink_protocol::Track)> = Vec::new();
                for (g, p) in players.iter_mut() {
                    if let Some(track) = p.poll_finished() {
                        finished.push((g.clone(), track));
                    }
                }
                for (g, track) in finished {
                    sess.send(ServerMessage::Event(Event::TrackEnd {
                        guild_id: g,
                        track,
                        reason: TrackEndReason::Finished,
                    }));
                }
                if secs % interval == 0 {
                    for (g, p) in players.iter() {
                        sess.send(ServerMessage::PlayerUpdate {
                            guild_id: g.clone(),
                            state: p.state(),
                        });
                    }
                }
            }
            if let Some(s) = &stats {
                sess.send(ServerMessage::Stats(s.clone()));
            }
        }
    }
}
