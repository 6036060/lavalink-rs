//! WebSocket サーバー→クライアント メッセージ（op: ready / playerUpdate / stats / event）。
//! 本サーバーは送信のみ行う（v4 では全制御が REST 経由のため受信メッセージは無い）。

use serde::Serialize;

use crate::types::{Exception, PlayerState, Stats, Track};

/// `op` で内部タグ付けされたサーバー→クライアント メッセージ。
// バリアント間のサイズ差は許容（送信専用でヒープ化のコストに見合わない）。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum ServerMessage {
    /// 接続確立直後。
    Ready {
        resumed: bool,
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// x 秒毎のプレイヤー状態。
    PlayerUpdate {
        #[serde(rename = "guildId")]
        guild_id: String,
        state: PlayerState,
    },
    /// 1 分毎の統計（フィールドはトップレベルに展開される）。
    Stats(Stats),
    /// プレイヤー/音声イベント（`type` で更にタグ付け）。
    Event(Event),
}

/// `type` で内部タグ付けされたイベント。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "TrackStartEvent", rename_all = "camelCase")]
    TrackStart { guild_id: String, track: Track },

    #[serde(rename = "TrackEndEvent", rename_all = "camelCase")]
    TrackEnd { guild_id: String, track: Track, reason: TrackEndReason },

    #[serde(rename = "TrackExceptionEvent", rename_all = "camelCase")]
    TrackException { guild_id: String, track: Track, exception: Exception },

    #[serde(rename = "TrackStuckEvent", rename_all = "camelCase")]
    TrackStuck { guild_id: String, track: Track, threshold_ms: u64 },

    #[serde(rename = "WebSocketClosedEvent", rename_all = "camelCase")]
    WebSocketClosed { guild_id: String, code: u16, reason: String, by_remote: bool },
}

/// トラック終了理由。`may_start_next` はクライアントの自動次曲再生判断に使われる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TrackEndReason {
    Finished,
    LoadFailed,
    Stopped,
    Replaced,
    Cleanup,
}

impl TrackEndReason {
    pub fn may_start_next(&self) -> bool {
        matches!(self, TrackEndReason::Finished | TrackEndReason::LoadFailed)
    }
}
