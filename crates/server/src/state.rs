//! サーバー共有状態とセッション管理（チケット 2-2）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use tokio::sync::{mpsc, Mutex, RwLock};

use lavalink_player::MockPlayer;
use lavalink_protocol::ServerMessage;

use crate::config::AppConfig;

pub type Tx = mpsc::UnboundedSender<ServerMessage>;

/// 1 つの WebSocket セッションに対応する状態。
pub struct Session {
    pub id: String,
    pub user_id: String,
    pub resuming: AtomicBool,
    pub timeout_secs: AtomicU64,
    /// guildId -> プレイヤー。
    pub players: Mutex<HashMap<String, MockPlayer>>,
    pub playbacks: Mutex<HashMap<String, crate::playback::Playback>>,
    tx: StdMutex<Option<Tx>>,
}

impl Session {
    pub fn new(id: String, user_id: String) -> Self {
        Self {
            id,
            user_id,
            resuming: AtomicBool::new(false),
            timeout_secs: AtomicU64::new(60),
            players: Mutex::new(HashMap::new()),
            playbacks: Mutex::new(HashMap::new()),
            tx: StdMutex::new(None),
        }
    }

    /// WebSocket 送信チャネルを取り付ける。
    pub fn attach(&self, tx: Tx) {
        *self.tx.lock().unwrap() = Some(tx);
    }
    pub fn detach(&self) {
        *self.tx.lock().unwrap() = None;
    }
    pub fn is_attached(&self) -> bool {
        self.tx.lock().unwrap().is_some()
    }
    /// 接続中なら best-effort で送信する。
    pub fn send(&self, msg: ServerMessage) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(msg);
        }
    }
}

pub struct AppState {
    pub config: Arc<AppConfig>,
    pub started: Instant,
    pub sessions: RwLock<HashMap<String, Arc<Session>>>,
    pub youtube: Arc<lavalink_source_youtube::YoutubeClient>,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    pub fn new(config: AppConfig) -> SharedState {
        Arc::new(Self {
            config: Arc::new(config),
            started: Instant::now(),
            sessions: RwLock::new(HashMap::new()),
            youtube: Arc::new(lavalink_source_youtube::YoutubeClient::new()),
        })
    }

    pub async fn get_session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.read().await.get(id).cloned()
    }

    pub async fn create_session(&self, user_id: String) -> Arc<Session> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let session = Arc::new(Session::new(id.clone(), user_id));
        self.sessions.write().await.insert(id, session.clone());
        session
    }

    pub async fn remove_session(&self, id: &str) {
        self.sessions.write().await.remove(id);
    }

    /// (players 総数, 再生中プレイヤー数)。
    pub async fn player_counts(&self) -> (u32, u32) {
        let sessions = self.sessions.read().await;
        let mut total = 0u32;
        let mut playing = 0u32;
        for s in sessions.values() {
            let players = s.players.lock().await;
            for p in players.values() {
                total += 1;
                if p.has_track() && !p.is_paused() {
                    playing += 1;
                }
            }
        }
        (total, playing)
    }
}

/// uptime をミリ秒で返す。
pub fn uptime_ms(state: &AppState) -> u64 {
    state.started.elapsed().as_millis() as u64
}
