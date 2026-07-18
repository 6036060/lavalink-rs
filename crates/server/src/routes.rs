//! REST ハンドラ（チケット 2-3, 2-4, 2-5 + info/stats/version/session）。

use std::sync::atomic::Ordering;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use lavalink_player::MockPlayer;
use lavalink_protocol::{
    Cpu, Event, Exception, Filters, GitInfo, Info, LoadResult, Memory, Player, ServerMessage,
    SessionInfo, SessionUpdate, Severity, Stats, Track, TrackEndReason, TrackInfo,
    UpdatePlayerRequest, VersionInfo, VoiceState,
};
use lavalink_source_youtube::extract_video_id;
use lavalink_track_codec as codec;

use crate::error::{ApiError, ApiResult};
use crate::state::{uptime_ms, AppState, SharedState};

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ============================== version ==============================

pub async fn version() -> &'static str {
    VERSION
}

// ============================== info / stats ==============================

pub async fn info(State(state): State<SharedState>) -> Json<Info> {
    let src = &state.config.lavalink.server.sources;
    let mut source_managers = Vec::new();
    for (name, on) in [
        ("youtube", src.youtube),
        ("soundcloud", src.soundcloud),
        ("bandcamp", src.bandcamp),
        ("twitch", src.twitch),
        ("vimeo", src.vimeo),
        ("nico", src.nico),
        ("http", src.http),
        ("local", src.local),
    ] {
        if on {
            source_managers.push(name.to_string());
        }
    }

    let f = &state.config.lavalink.server.filters;
    let mut filters = Vec::new();
    for (name, on) in [
        ("volume", f.volume),
        ("equalizer", f.equalizer),
        ("karaoke", f.karaoke),
        ("timescale", f.timescale),
        ("tremolo", f.tremolo),
        ("vibrato", f.vibrato),
        ("distortion", f.distortion),
        ("rotation", f.rotation),
        ("channelMix", f.channel_mix),
        ("lowPass", f.low_pass),
    ] {
        if on {
            filters.push(name.to_string());
        }
    }

    Json(Info {
        version: VersionInfo {
            semver: VERSION.to_string(),
            major: 0,
            minor: 1,
            patch: 0,
            pre_release: None,
            build: None,
        },
        build_time: 0,
        git: GitInfo { branch: "main".into(), commit: "unknown".into(), commit_time: 0 },
        jvm: "rust".into(),
        lavaplayer: "rust-native".into(),
        source_managers,
        filters,
        plugins: Vec::new(),
    })
}

/// `/v4/stats`（frameStats は常に null）。
pub async fn build_stats(state: &AppState) -> Stats {
    let (players, playing_players) = state.player_counts().await;
    let cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    Stats {
        players,
        playing_players,
        uptime: uptime_ms(state),
        memory: Memory { free: 0, used: 0, allocated: 0, reservable: 0 },
        cpu: Cpu { cores, system_load: 0.0, lavalink_load: 0.0 },
        frame_stats: None,
    }
}

pub async fn stats(State(state): State<SharedState>) -> Json<Stats> {
    Json(build_stats(&state).await)
}

// ============================== loadtracks ==============================

#[derive(Debug, Deserialize)]
pub struct LoadQuery {
    pub identifier: String,
}

/// TrackInfo を encoded 付き Track に変換する。
fn track_from_info(info: TrackInfo) -> Track {
    let encoded = codec::encode(&info);
    Track::new(encoded, info)
}

/// identifier だけから最小限の Track を作る（identifier 指定の play 用。
/// 実メタ/ストリームは再生時に解決される）。
fn track_from_identifier(identifier: &str) -> Track {
    let is_url = identifier.starts_with("http://") || identifier.starts_with("https://");
    let (uri, source) = if is_url {
        (identifier.to_string(), "http")
    } else {
        (format!("https://www.youtube.com/watch?v={identifier}"), "youtube")
    };
    track_from_info(TrackInfo {
        identifier: identifier.to_string(),
        is_seekable: true,
        author: String::new(),
        length: 0,
        is_stream: false,
        position: 0,
        title: identifier.to_string(),
        uri: Some(uri),
        artwork_url: None,
        isrc: None,
        source_name: source.into(),
    })
}

/// HTTP 直リンクの Track。
fn http_track(url: &str) -> Track {
    let title = url.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(url).to_string();
    track_from_info(TrackInfo {
        identifier: url.to_string(),
        is_seekable: false,
        author: "http".into(),
        length: 0,
        is_stream: false,
        position: 0,
        title,
        uri: Some(url.to_string()),
        artwork_url: None,
        isrc: None,
        source_name: "http".into(),
    })
}

fn yt_exception(e: lavalink_source_youtube::YtError) -> Exception {
    Exception {
        message: Some(e.to_string()),
        severity: Severity::Common,
        cause: "youtube".into(),
        cause_stack_trace: String::new(),
    }
}

pub async fn load_tracks(
    State(state): State<SharedState>,
    Query(q): Query<LoadQuery>,
) -> Json<LoadResult> {
    let id = q.identifier.trim();
    if id.is_empty() {
        return Json(LoadResult::empty());
    }

    // YouTube 検索（ytsearch: / ytmsearch:）
    for prefix in ["ytsearch:", "ytmsearch:"] {
        if let Some(query) = id.strip_prefix(prefix) {
            return Json(match state.youtube.search(query).await {
                Ok(infos) if !infos.is_empty() => {
                    LoadResult::search(infos.into_iter().map(track_from_info).collect())
                }
                Ok(_) => LoadResult::empty(),
                Err(e) => LoadResult::error(yt_exception(e)),
            });
        }
    }

    // YouTube 動画（watch URL / youtu.be / 生 11 文字 ID）
    let is_youtube = id.contains("youtube.com") || id.contains("youtu.be");
    let bare_id =
        id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if is_youtube || bare_id {
        if let Some(vid) = extract_video_id(id) {
            return Json(match state.youtube.resolve_meta(&vid).await {
                Ok(info) => LoadResult::track(track_from_info(info)),
                Err(e) => LoadResult::error(yt_exception(e)),
            });
        }
    }

    // HTTP 直リンク
    if id.starts_with("http://") || id.starts_with("https://") {
        return Json(LoadResult::track(http_track(id)));
    }

    // scsearch 等・その他は未対応。
    Json(LoadResult::empty())
}

// ============================== decode ==============================

#[derive(Debug, Deserialize)]
pub struct DecodeQuery {
    #[serde(rename = "encodedTrack")]
    pub encoded_track: String,
}

fn decode_one(encoded: &str, path: &str) -> ApiResult<Track> {
    let info = codec::decode(encoded)
        .map_err(|e| ApiError::bad_request(format!("Failed to decode track: {e}"), path))?;
    Ok(Track::new(encoded.to_string(), info))
}

pub async fn decode_track(Query(q): Query<DecodeQuery>) -> ApiResult<Json<Track>> {
    Ok(Json(decode_one(&q.encoded_track, "/v4/decodetrack")?))
}

pub async fn decode_tracks(Json(list): Json<Vec<String>>) -> ApiResult<Json<Vec<Track>>> {
    let mut out = Vec::with_capacity(list.len());
    for enc in &list {
        out.push(decode_one(enc, "/v4/decodetracks")?);
    }
    Ok(Json(out))
}

// ============================== session ==============================

pub async fn update_session(
    Path(session_id): Path<String>,
    State(state): State<SharedState>,
    Json(body): Json<SessionUpdate>,
) -> ApiResult<Json<SessionInfo>> {
    let path = format!("/v4/sessions/{session_id}");
    let session = state
        .get_session(&session_id)
        .await
        .ok_or_else(|| ApiError::not_found("Session not found", path))?;

    if let Some(resuming) = body.resuming {
        session.resuming.store(resuming, Ordering::SeqCst);
    }
    if let Some(timeout) = body.timeout {
        session.timeout_secs.store(timeout, Ordering::SeqCst);
    }

    Ok(Json(SessionInfo {
        resuming: session.resuming.load(Ordering::SeqCst),
        timeout: session.timeout_secs.load(Ordering::SeqCst),
    }))
}

// ============================== players ==============================

pub async fn get_players(
    Path(session_id): Path<String>,
    State(state): State<SharedState>,
) -> ApiResult<Json<Vec<Player>>> {
    let path = format!("/v4/sessions/{session_id}/players");
    let session = state
        .get_session(&session_id)
        .await
        .ok_or_else(|| ApiError::not_found("Session not found", path))?;
    let players = session.players.lock().await;
    Ok(Json(players.values().map(|p| p.to_player()).collect()))
}

pub async fn get_player(
    Path((session_id, guild_id)): Path<(String, String)>,
    State(state): State<SharedState>,
) -> ApiResult<Json<Player>> {
    let path = format!("/v4/sessions/{session_id}/players/{guild_id}");
    let session = state
        .get_session(&session_id)
        .await
        .ok_or_else(|| ApiError::not_found("Session not found", path.clone()))?;
    let players = session.players.lock().await;
    let player =
        players.get(&guild_id).ok_or_else(|| ApiError::not_found("Player not found", path))?;
    Ok(Json(player.to_player()))
}

#[derive(Debug, Deserialize)]
pub struct NoReplaceQuery {
    #[serde(default, rename = "noReplace")]
    pub no_replace: bool,
}

/// トラック指定の解決結果。
#[allow(clippy::large_enum_variant)]
enum TrackAction {
    Keep,
    Stop,
    Play(Track),
}

/// PATCH リクエストからトラック操作を決定する（encoded/identifier の解決）。
fn resolve_track_action(req: &UpdatePlayerRequest) -> TrackAction {
    // `track` オブジェクトが最優先。
    if let Some(t) = &req.track {
        if let Some(enc) = &t.encoded {
            return match enc {
                Some(encoded) => decode_to_track(encoded, t.user_data.clone()),
                None => TrackAction::Stop,
            };
        }
        if let Some(identifier) = &t.identifier {
            return TrackAction::Play(track_from_identifier(identifier));
        }
        return TrackAction::Keep;
    }
    // 非推奨トップレベルフィールド。
    if let Some(enc) = &req.encoded_track {
        return match enc {
            Some(encoded) => decode_to_track(encoded, None),
            None => TrackAction::Stop,
        };
    }
    if let Some(identifier) = &req.identifier {
        return TrackAction::Play(track_from_identifier(identifier));
    }
    TrackAction::Keep
}

fn decode_to_track(encoded: &str, user_data: Option<serde_json::Value>) -> TrackAction {
    match codec::decode(encoded) {
        Ok(info) => {
            let mut track = Track::new(encoded.to_string(), info);
            if let Some(ud) = user_data {
                track.user_data = ud;
            }
            TrackAction::Play(track)
        }
        // デコード失敗時はトラック変更を行わない（不正な encoded を無視）。
        Err(_) => TrackAction::Keep,
    }
}

/// Filters の各パラメータを公式仕様 (https://lavalink.dev/api/rest#filters) の
/// 範囲で検証する。範囲外は 400 を返す（本家 Lavalink 互換）。
fn validate_filters(f: &Filters) -> Result<(), String> {
    if let Some(v) = f.volume {
        if !(0.0..=5.0).contains(&v) {
            return Err(format!("volume must be within 0.0 to 5.0: {v}"));
        }
    }
    if let Some(eq) = &f.equalizer {
        for b in eq {
            if b.band > 14 {
                return Err(format!("equalizer band must be within 0 to 14: {}", b.band));
            }
            if !(-0.25..=1.0).contains(&b.gain) {
                return Err(format!("equalizer gain must be within -0.25 to 1.0: {}", b.gain));
            }
        }
    }
    if let Some(k) = &f.karaoke {
        for (name, v) in [("level", k.level), ("monoLevel", k.mono_level)] {
            if let Some(v) = v {
                if !(0.0..=1.0).contains(&v) {
                    return Err(format!("karaoke {name} must be within 0.0 to 1.0: {v}"));
                }
            }
        }
    }
    if let Some(ts) = &f.timescale {
        for (name, v) in [("speed", ts.speed), ("pitch", ts.pitch), ("rate", ts.rate)] {
            if let Some(v) = v {
                if v < 0.0 {
                    return Err(format!(
                        "timescale {name} must be greater than or equal to 0.0: {v}"
                    ));
                }
            }
        }
    }
    if let Some(t) = &f.tremolo {
        if let Some(freq) = t.frequency {
            if freq <= 0.0 {
                return Err(format!("tremolo frequency must be greater than 0.0: {freq}"));
            }
        }
        if let Some(d) = t.depth {
            if d <= 0.0 || d > 1.0 {
                return Err(format!("tremolo depth must be within 0.0 (exclusive) to 1.0: {d}"));
            }
        }
    }
    if let Some(v) = &f.vibrato {
        if let Some(freq) = v.frequency {
            if freq <= 0.0 || freq > 14.0 {
                return Err(format!(
                    "vibrato frequency must be within 0.0 (exclusive) to 14.0: {freq}"
                ));
            }
        }
        if let Some(d) = v.depth {
            if d <= 0.0 || d > 1.0 {
                return Err(format!("vibrato depth must be within 0.0 (exclusive) to 1.0: {d}"));
            }
        }
    }
    if let Some(cm) = &f.channel_mix {
        for (name, v) in [
            ("leftToLeft", cm.left_to_left),
            ("leftToRight", cm.left_to_right),
            ("rightToLeft", cm.right_to_left),
            ("rightToRight", cm.right_to_right),
        ] {
            if let Some(v) = v {
                if !(0.0..=1.0).contains(&v) {
                    return Err(format!("channelMix {name} must be within 0.0 to 1.0: {v}"));
                }
            }
        }
    }
    // lowPass: smoothing ≤ 1.0 は「無効化」の意味なので不正値ではない（仕様どおり）。
    // rotation / distortion: 仕様上の範囲制限なし。
    Ok(())
}

/// 設定 (`lavalink.server.filters`) で無効化されているのに使われたフィルタ名を返す。
fn disabled_filters_used(f: &Filters, cfg: &crate::config::Filters) -> Vec<&'static str> {
    let mut used = Vec::new();
    if f.volume.is_some() && !cfg.volume {
        used.push("volume");
    }
    if f.equalizer.is_some() && !cfg.equalizer {
        used.push("equalizer");
    }
    if f.karaoke.is_some() && !cfg.karaoke {
        used.push("karaoke");
    }
    if f.timescale.is_some() && !cfg.timescale {
        used.push("timescale");
    }
    if f.tremolo.is_some() && !cfg.tremolo {
        used.push("tremolo");
    }
    if f.vibrato.is_some() && !cfg.vibrato {
        used.push("vibrato");
    }
    if f.distortion.is_some() && !cfg.distortion {
        used.push("distortion");
    }
    if f.rotation.is_some() && !cfg.rotation {
        used.push("rotation");
    }
    if f.channel_mix.is_some() && !cfg.channel_mix {
        used.push("channelMix");
    }
    if f.low_pass.is_some() && !cfg.low_pass {
        used.push("lowPass");
    }
    used
}

/// プレイヤー音量（0-1000, 100=等倍）をフィルタ音量へ合成した「実効フィルタ」を返す。
/// 音声パイプラインへ渡す用（REST が返す filters には player volume を混ぜない）。
fn effective_filters(filters: &Filters, volume: u16) -> Filters {
    let mut f = filters.clone();
    let gain = f32::from(volume) / 100.0;
    if (gain - 1.0).abs() > f32::EPSILON {
        f.volume = Some(f.volume.unwrap_or(1.0) * gain);
    }
    f
}

pub async fn update_player(
    Path((session_id, guild_id)): Path<(String, String)>,
    Query(nr): Query<NoReplaceQuery>,
    State(state): State<SharedState>,
    Json(req): Json<UpdatePlayerRequest>,
) -> ApiResult<Json<Player>> {
    let path = format!("/v4/sessions/{session_id}/players/{guild_id}");

    // フィルタのパラメータ範囲を公式仕様どおり検証し、
    // 設定で無効化されたフィルタの使用も 400（本家 Lavalink 互換）。
    if let Some(filters) = &req.filters {
        if let Err(msg) = validate_filters(filters) {
            return Err(ApiError::bad_request(msg, path));
        }
        let disabled = disabled_filters_used(filters, &state.config.lavalink.server.filters);
        if !disabled.is_empty() {
            return Err(ApiError::bad_request(
                format!(
                    "The following filters are disabled in the config: {}",
                    disabled.join(", ")
                ),
                path,
            ));
        }
    }

    let session = state
        .get_session(&session_id)
        .await
        .ok_or_else(|| ApiError::not_found("Session not found", path))?;

    // 実再生制御に渡す情報。players ロック解放後に実行する。
    let mut connect_voice: Option<VoiceState> = None;
    let mut start_play: Option<(Track, Filters)> = None;
    let mut do_stop_playback = false;
    let mut set_paused_to: Option<bool> = None;
    let mut filters_changed = false;
    let mut volume_changed = false;
    let mut seek_to: Option<u64> = None;

    let player_json = {
        let mut players = session.players.lock().await;
        let entry =
            players.entry(guild_id.clone()).or_insert_with(|| MockPlayer::new(guild_id.clone()));

        // --- トラック操作 ---
        match resolve_track_action(&req) {
            TrackAction::Play(track) => {
                let already_playing = entry.has_track();
                if already_playing && nr.no_replace {
                    // noReplace: 再生中なら新トラックを無視。
                } else {
                    tracing::info!(
                        %guild_id,
                        title = %track.info.title,
                        "player PATCH: play"
                    );
                    if let Some(old) = entry.stop() {
                        session.send(ServerMessage::Event(Event::TrackEnd {
                            guild_id: guild_id.clone(),
                            track: old,
                            reason: TrackEndReason::Replaced,
                        }));
                    }
                    entry.play(track.clone());
                    entry.set_real_playback(true);
                    session.send(ServerMessage::Event(Event::TrackStart {
                        guild_id: guild_id.clone(),
                        track: track.clone(),
                    }));
                    start_play = Some((track, Filters::default()));
                }
            }
            TrackAction::Stop => {
                tracing::info!(%guild_id, "player PATCH: stop");
                if let Some(old) = entry.stop() {
                    session.send(ServerMessage::Event(Event::TrackEnd {
                        guild_id: guild_id.clone(),
                        track: old,
                        reason: TrackEndReason::Stopped,
                    }));
                }
                entry.set_real_playback(false);
                do_stop_playback = true;
            }
            TrackAction::Keep => {}
        }

        // --- その他フィールド（部分更新）---
        if let Some(voice) = req.voice {
            if voice.is_complete() {
                // dev: playfile が自動で読む voice.env を書き出す。voice token を平文で
                // ディスクに残すため、明示オプトイン（LAVALINK_WRITE_VOICE_ENV=1）時のみ。
                if std::env::var("LAVALINK_WRITE_VOICE_ENV").is_ok_and(|v| v == "1") {
                    let content = format!(
                        "GUILD_ID={}\nUSER_ID={}\nSESSION_ID={}\nVOICE_TOKEN={}\nVOICE_ENDPOINT={}\n",
                        guild_id, session.user_id, voice.session_id, voice.token, voice.endpoint
                    );
                    if std::fs::write("voice.env", content).is_ok() {
                        tracing::info!(
                            "wrote voice.env -> run: cargo run -p lavalink-playfile -- <FILE>"
                        );
                    }
                }
                // playfile 等の外部ツールでこのセッションを使う場合、サーバー自身は
                // 接続しない（同一セッションの二重接続を防ぎ 4006 を回避）。
                if std::env::var("LAVALINK_SKIP_VOICE_CONNECT").is_ok_and(|v| v == "1") {
                    tracing::warn!(
                        "LAVALINK_SKIP_VOICE_CONNECT=1: サーバーは voice 接続をスキップします \
                         (voice.env を playfile 等で使うテスト用)。音声再生はされません。"
                    );
                } else {
                    connect_voice = Some(voice.clone());
                }
            }
            entry.set_voice(voice);
        }
        if let Some(volume) = req.volume {
            entry.set_volume(volume);
            volume_changed = true;
        }
        if let Some(paused) = req.paused {
            entry.set_paused(paused);
            set_paused_to = Some(paused);
        }
        if let Some(filters) = req.filters {
            entry.set_filters(filters);
            filters_changed = true;
        }
        if let Some(end_time) = req.end_time {
            entry.set_end_time(end_time.map(|e| e as u64));
        }
        if let Some(position) = req.position {
            let pos = position.max(0) as u64;
            entry.seek(pos);
            // トラックが無いのに実再生を開き直さない (終了後の position PATCH 対策)
            if entry.has_track() {
                seek_to = Some(pos);
            }
        }

        let pj = entry.to_player();
        if let Some((_, f)) = start_play.as_mut() {
            // プレイヤー音量も合成して音声パイプラインへ渡す。
            *f = effective_filters(&pj.filters, pj.volume);
        }
        pj
    };

    // 再生中フィルタ/音量のライブ適用は、トラックを新規開始しない場合のみ
    // （開始時は play 側が反映）。volume はフィルタ音量へ合成して適用する。
    let apply_filters = if (filters_changed || volume_changed) && start_play.is_none() {
        Some(effective_filters(&player_json.filters, player_json.volume))
    } else {
        None
    };

    // --- 実再生制御（voice 接続 / 再生 / 停止 / 一時停止 / シーク / フィルタ）---
    if connect_voice.is_some()
        || start_play.is_some()
        || do_stop_playback
        || set_paused_to.is_some()
        || apply_filters.is_some()
        || seek_to.is_some()
    {
        match (guild_id.parse::<u64>(), session.user_id.parse::<u64>()) {
            (Ok(guild_u64), Ok(user_u64)) => {
                let mut pbs = session.playbacks.lock().await;
                // 接続/再生開始時のみ Playback を生成。pause/seek/filters だけなら既存を操作する。
                let needs_create = connect_voice.is_some() || start_play.is_some();
                let pb_opt = if needs_create {
                    Some(pbs.entry(guild_id.clone()).or_insert_with(|| {
                        crate::playback::Playback::new(guild_u64, user_u64, guild_id.clone())
                    }))
                } else {
                    pbs.get_mut(&guild_id)
                };
                if let Some(pb) = pb_opt {
                    if let Some(voice) = &connect_voice {
                        pb.connect(voice).await;
                    }
                    if let Some((track, filters)) = start_play {
                        // 新トラックに position が同時指定されていればその位置から開始。
                        let start_ms = seek_to.take().unwrap_or(0);
                        pb.play(track, filters, session.clone(), state.youtube.clone(), start_ms);
                    } else if do_stop_playback {
                        pb.stop();
                    }
                    if let Some(paused) = set_paused_to {
                        pb.set_paused(paused);
                    }
                    if let Some(filters) = apply_filters {
                        pb.set_filters(filters);
                    }
                    if let Some(pos) = seek_to {
                        pb.seek(pos);
                    }
                }
            }
            _ => tracing::warn!(%guild_id, "non-numeric guild/user id; skipping real playback"),
        }
    }

    Ok(Json(player_json))
}

pub async fn destroy_player(
    Path((session_id, guild_id)): Path<(String, String)>,
    State(state): State<SharedState>,
) -> ApiResult<StatusCode> {
    let path = format!("/v4/sessions/{session_id}/players/{guild_id}");
    let session = state
        .get_session(&session_id)
        .await
        .ok_or_else(|| ApiError::not_found("Session not found", path))?;
    session.players.lock().await.remove(&guild_id);
    // 再生も停止（Playback の Drop で voice 接続/タスクが落ちる）。
    session.playbacks.lock().await.remove(&guild_id);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lavalink_protocol::{ChannelMix, Equalizer, Tremolo, Vibrato};

    #[test]
    fn validate_accepts_spec_examples() {
        // 公式ドキュメントの Example Payload 相当。
        let f = Filters {
            volume: Some(1.0),
            equalizer: Some(vec![Equalizer { band: 0, gain: 0.2 }]),
            tremolo: Some(Tremolo { frequency: Some(2.0), depth: Some(0.5) }),
            vibrato: Some(Vibrato { frequency: Some(2.0), depth: Some(0.5) }),
            channel_mix: Some(ChannelMix {
                left_to_left: Some(1.0),
                left_to_right: Some(0.0),
                right_to_left: Some(0.0),
                right_to_right: Some(1.0),
            }),
            ..Default::default()
        };
        assert!(validate_filters(&f).is_ok());
        assert!(validate_filters(&Filters::default()).is_ok());
    }

    #[test]
    fn validate_rejects_out_of_range() {
        // volume > 5.0
        let f = Filters { volume: Some(5.5), ..Default::default() };
        assert!(validate_filters(&f).is_err());
        // equalizer band > 14 / gain < -0.25
        let f = Filters {
            equalizer: Some(vec![Equalizer { band: 15, gain: 0.0 }]),
            ..Default::default()
        };
        assert!(validate_filters(&f).is_err());
        let f = Filters {
            equalizer: Some(vec![Equalizer { band: 0, gain: -0.5 }]),
            ..Default::default()
        };
        assert!(validate_filters(&f).is_err());
        // tremolo frequency <= 0 / depth > 1
        let f = Filters {
            tremolo: Some(Tremolo { frequency: Some(0.0), depth: Some(0.5) }),
            ..Default::default()
        };
        assert!(validate_filters(&f).is_err());
        let f = Filters {
            tremolo: Some(Tremolo { frequency: Some(2.0), depth: Some(1.5) }),
            ..Default::default()
        };
        assert!(validate_filters(&f).is_err());
        // vibrato frequency > 14
        let f = Filters {
            vibrato: Some(Vibrato { frequency: Some(15.0), depth: Some(0.5) }),
            ..Default::default()
        };
        assert!(validate_filters(&f).is_err());
        // channelMix > 1.0
        let f = Filters {
            channel_mix: Some(ChannelMix {
                left_to_left: Some(1.5),
                left_to_right: None,
                right_to_left: None,
                right_to_right: None,
            }),
            ..Default::default()
        };
        assert!(validate_filters(&f).is_err());
    }

    #[test]
    fn effective_filters_merges_player_volume() {
        // player volume 200% × filter volume 0.5 → 実効 1.0
        let f = Filters { volume: Some(0.5), ..Default::default() };
        let eff = effective_filters(&f, 200);
        assert!((eff.volume.unwrap() - 1.0).abs() < 1e-6);
        // volume 100% はフィルタをそのまま透過
        let eff = effective_filters(&f, 100);
        assert!((eff.volume.unwrap() - 0.5).abs() < 1e-6);
        // フィルタ volume 未指定でも player volume は反映
        let eff = effective_filters(&Filters::default(), 50);
        assert!((eff.volume.unwrap() - 0.5).abs() < 1e-6);
    }
}
