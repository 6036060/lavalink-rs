//! REST DTO 群（Track / LoadResult / Player / Filters / Stats / Info / Session 等）。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::{double_option, empty_object};

// ============================== Track ==============================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub encoded: String,
    pub info: TrackInfo,
    #[serde(default = "empty_object")]
    pub plugin_info: Value,
    #[serde(default = "empty_object")]
    pub user_data: Value,
}

impl Track {
    pub fn new(encoded: String, info: TrackInfo) -> Self {
        Self { encoded, info, plugin_info: empty_object(), user_data: empty_object() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub identifier: String,
    pub is_seekable: bool,
    pub author: String,
    /// 長さ（ミリ秒）。
    pub length: u64,
    pub is_stream: bool,
    /// 位置（ミリ秒）。
    pub position: u64,
    pub title: String,
    pub uri: Option<String>,
    pub artwork_url: Option<String>,
    pub isrc: Option<String>,
    pub source_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistInfo {
    pub name: String,
    /// 選択トラックの index（無ければ -1）。
    pub selected_track: i32,
}

// ============================== LoadResult ==============================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoadType {
    Track,
    Playlist,
    Search,
    Empty,
    Error,
}

/// `/v4/loadtracks` のレスポンス。`data` の形は loadType により異なるため
/// `Value` で正確な wire 形状を保証する（empty は `null`）。
#[derive(Debug, Clone, Serialize)]
pub struct LoadResult {
    #[serde(rename = "loadType")]
    pub load_type: LoadType,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistData {
    pub info: PlaylistInfo,
    #[serde(default = "empty_object")]
    pub plugin_info: Value,
    pub tracks: Vec<Track>,
}

impl LoadResult {
    pub fn track(t: Track) -> Self {
        Self { load_type: LoadType::Track, data: serde_json::to_value(t).unwrap_or(Value::Null) }
    }
    pub fn playlist(p: PlaylistData) -> Self {
        Self { load_type: LoadType::Playlist, data: serde_json::to_value(p).unwrap_or(Value::Null) }
    }
    pub fn search(tracks: Vec<Track>) -> Self {
        Self { load_type: LoadType::Search, data: serde_json::to_value(tracks).unwrap_or(Value::Null) }
    }
    pub fn empty() -> Self {
        Self { load_type: LoadType::Empty, data: Value::Null }
    }
    pub fn error(e: Exception) -> Self {
        Self { load_type: LoadType::Error, data: serde_json::to_value(e).unwrap_or(Value::Null) }
    }
}

// ============================== Exception ==============================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Common,
    Suspicious,
    Fault,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Exception {
    pub message: Option<String>,
    pub severity: Severity,
    pub cause: String,
    #[serde(default)]
    pub cause_stack_trace: String,
}

// ============================== Filters ==============================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Filters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub equalizer: Option<Vec<Equalizer>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub karaoke: Option<Karaoke>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timescale: Option<Timescale>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tremolo: Option<Tremolo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vibrato: Option<Vibrato>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotation: Option<Rotation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distortion: Option<Distortion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_mix: Option<ChannelMix>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_pass: Option<LowPass>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_filters: Option<HashMap<String, Value>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Equalizer {
    pub band: u8,
    pub gain: f32,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Karaoke {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mono_level: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter_band: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter_width: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Timescale {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pitch: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Tremolo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Vibrato {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rotation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotation_hz: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Distortion {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sin_offset: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sin_scale: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cos_offset: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cos_scale: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tan_offset: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tan_scale: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelMix {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_to_left: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_to_right: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_to_left: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right_to_right: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct LowPass {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoothing: Option<f32>,
}

// ============================== Player / Voice ==============================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceState {
    pub token: String,
    pub endpoint: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
}

impl VoiceState {
    /// 接続に必要な 3 値が揃っているか。
    pub fn is_complete(&self) -> bool {
        !self.token.is_empty() && !self.endpoint.is_empty() && !self.session_id.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerState {
    /// Unix ミリ秒。
    pub time: i64,
    /// 位置（ミリ秒）。
    pub position: i64,
    pub connected: bool,
    /// Discord 音声サーバーへの ping（未接続は -1）。
    pub ping: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Player {
    pub guild_id: String,
    pub track: Option<Track>,
    /// 音量（パーセント, 0-1000）。
    pub volume: u16,
    pub paused: bool,
    pub state: PlayerState,
    pub voice: VoiceState,
    pub filters: Filters,
}

// ----- PATCH /v4/sessions/{s}/players/{guild} リクエスト -----

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePlayerTrack {
    /// `null` で現在のトラックを停止。
    #[serde(default, deserialize_with = "double_option")]
    pub encoded: Option<Option<String>>,
    #[serde(default)]
    pub identifier: Option<String>,
    #[serde(default)]
    pub user_data: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePlayerRequest {
    #[serde(default)]
    pub track: Option<UpdatePlayerTrack>,
    /// 非推奨。`track.encoded` を使うこと。`null` で停止。
    #[serde(default, deserialize_with = "double_option")]
    pub encoded_track: Option<Option<String>>,
    /// 非推奨。`track.identifier` を使うこと。
    #[serde(default)]
    pub identifier: Option<String>,
    #[serde(default)]
    pub position: Option<i64>,
    /// `null` で endTime をリセット。
    #[serde(default, deserialize_with = "double_option")]
    pub end_time: Option<Option<i64>>,
    #[serde(default)]
    pub volume: Option<u16>,
    #[serde(default)]
    pub paused: Option<bool>,
    #[serde(default)]
    pub filters: Option<Filters>,
    #[serde(default)]
    pub voice: Option<VoiceState>,
}

// ============================== Session ==============================

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdate {
    #[serde(default)]
    pub resuming: Option<bool>,
    #[serde(default)]
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub resuming: bool,
    pub timeout: u64,
}

// ============================== Stats ==============================

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub players: u32,
    pub playing_players: u32,
    pub uptime: u64,
    pub memory: Memory,
    pub cpu: Cpu,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_stats: Option<FrameStats>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Memory {
    pub free: u64,
    pub used: u64,
    pub allocated: u64,
    pub reservable: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cpu {
    pub cores: u32,
    pub system_load: f64,
    pub lavalink_load: f64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct FrameStats {
    pub sent: i64,
    pub nulled: i64,
    pub deficit: i64,
}

// ============================== Info ==============================

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub version: VersionInfo,
    pub build_time: u64,
    pub git: GitInfo,
    pub jvm: String,
    pub lavaplayer: String,
    pub source_managers: Vec<String>,
    pub filters: Vec<String>,
    pub plugins: Vec<PluginInfo>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    pub semver: String,
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub pre_release: Option<String>,
    pub build: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitInfo {
    pub branch: String,
    pub commit: String,
    pub commit_time: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
}
