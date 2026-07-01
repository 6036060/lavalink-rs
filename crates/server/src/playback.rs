//! 実音声再生エンジン（server 結線, ストリーミング再生）。
//!
//! REST の player 操作に応じて Discord 音声へ接続し、トラックの音声を
//! 解決(http 直リンク / YouTube は再生時に stream URL 再解決) → ダウンロードしながら
//! 逐次デコード → Opus → VoiceConnection へ送出する。全曲を溜めないので再生開始が速い。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::task::JoinHandle;

use lavalink_audio_pipeline::{AudioPipeline, SharedBuffer, SharedFilters};
use lavalink_discord_voice::{VoiceConfig, VoiceConnection};
use lavalink_protocol::{Event, Filters, ServerMessage, Track, TrackEndReason, VoiceState};
use lavalink_source_youtube::YoutubeClient;

use crate::state::Session;

/// 1 ギルドの実再生（voice 接続 + 再生タスク）。
pub struct Playback {
    guild_id: u64,
    user_id: u64,
    guild_key: String,
    conn: Option<VoiceConnection>,
    task: Option<JoinHandle<()>>,
    /// 現在の再生を停止させるフラグ（spawn_blocking のデコードは abort できないため）。
    stop_flag: Arc<AtomicBool>,
    /// 再生中にライブ更新できるフィルタ（音量/EQ/timescale 等）。デコードタスクと共有。
    filters: Arc<Mutex<SharedFilters>>,
    /// 望ましい一時停止状態。connect で新しい接続にも再適用する。
    paused: bool,
}

impl Playback {
    pub fn new(guild_id: u64, user_id: u64, guild_key: String) -> Self {
        Self {
            guild_id,
            user_id,
            guild_key,
            conn: None,
            task: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            filters: Arc::new(Mutex::new(SharedFilters::default())),
            paused: false,
        }
    }

    pub async fn connect(&mut self, voice: &VoiceState) {
        self.stop();
        self.conn = None;
        let cfg = VoiceConfig {
            guild_id: self.guild_id,
            user_id: self.user_id,
            session_id: voice.session_id.clone(),
            token: voice.token.clone(),
            endpoint: voice.endpoint.clone(),
        };
        match VoiceConnection::connect(cfg).await {
            Ok(c) => {
                tracing::info!(guild = self.guild_id, "voice connection established");
                // 既存の一時停止状態を新しい接続にも適用。
                c.set_paused(self.paused);
                self.conn = Some(c);
            }
            Err(e) => tracing::warn!(guild = self.guild_id, error = %e, "voice connect failed"),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    /// トラックを再生。YouTube は再生時に stream URL を再解決する。完了/失敗で TrackEnd 発火。
    pub fn play(
        &mut self,
        track: Track,
        filters: Filters,
        session: Arc<Session>,
        youtube: Arc<YoutubeClient>,
    ) {
        self.stop();
        let Some(conn) = self.conn.as_ref() else {
            tracing::warn!(guild = self.guild_id, "play without voice connection");
            return;
        };
        let tx = conn.audio_sender();
        let guild_key = self.guild_key.clone();

        // 新しい再生用の停止フラグ。
        let stop_flag = Arc::new(AtomicBool::new(false));
        self.stop_flag = stop_flag.clone();

        // このトラックの初期フィルタを共有設定へ反映（以降の変更はライブ適用される）。
        self.filters.lock().unwrap().update(filters);
        let filters_arc = self.filters.clone();

        let task = tokio::spawn(async move {
            let reason = stream_track(&tx, &track, &youtube, filters_arc, stop_flag).await;
            {
                let mut players = session.players.lock().await;
                if let Some(p) = players.get_mut(&guild_key) {
                    p.set_real_playback(false);
                    let _ = p.stop();
                }
            }
            session.send(ServerMessage::Event(Event::TrackEnd {
                guild_id: guild_key,
                track,
                reason,
            }));
        });
        self.task = Some(task);
    }

    pub fn stop(&mut self) {
        // 進行中のデコード/ダウンロードに停止を通知。
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }

    /// 一時停止/再開。送出側を止めるだけでデコード位置は失わない（再開で続きから鳴る）。
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if let Some(c) = self.conn.as_ref() {
            c.set_paused(paused);
        }
    }

    /// 再生中のフィルタ更新（音量/EQ/timescale 等）。次のフレームから反映される。
    pub fn set_filters(&self, filters: Filters) {
        self.filters.lock().unwrap().update(filters);
    }
}

/// URL 解決 → ストリーミング DL ＋ 逐次デコード → Opus 送出。完了/失敗の理由を返す。
async fn stream_track(
    tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    track: &Track,
    youtube: &YoutubeClient,
    filters: Arc<Mutex<SharedFilters>>,
    stop_flag: Arc<AtomicBool>,
) -> TrackEndReason {
    let t0 = std::time::Instant::now();
    // ソースに応じて再生 URL を解決。
    let url = if track.info.source_name == "youtube" {
        match youtube.stream_url(&track.info.identifier).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, id = %track.info.identifier, "youtube resolve failed");
                return TrackEndReason::LoadFailed;
            }
        }
    } else {
        match track.info.uri.clone() {
            Some(u) => u,
            None => return TrackEndReason::LoadFailed,
        }
    };
    tracing::info!(resolve_ms = t0.elapsed().as_millis() as u64, "playback: url resolved");

    let ext = url
        .split('?')
        .next()
        .unwrap_or(&url)
        .rsplit('.')
        .next()
        .filter(|e| !e.is_empty() && e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(|e| e.to_string());

    // ストリーミング GET。
    let resp = match reqwest::get(&url).await.and_then(|r| r.error_for_status()) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "track fetch failed");
            return TrackEndReason::LoadFailed;
        }
    };
    let total = resp.content_length();
    let buf = Arc::new(Mutex::new(SharedBuffer::new(total)));

    // ダウンロードタスク: チャンクをバッファへ追記。
    let buf_dl = buf.clone();
    let stop_dl = stop_flag.clone();
    let dl = tokio::spawn(async move {
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            if stop_dl.load(Ordering::Relaxed) {
                break;
            }
            match chunk {
                Ok(c) => buf_dl.lock().unwrap().push(&c),
                Err(_) => break,
            }
        }
        buf_dl.lock().unwrap().finish();
    });

    // デコード（spawn_blocking）: バッファを読みながら逐次 Opus 化し送出。
    let buf_dec = buf.clone();
    let filters2 = filters.clone();
    let ext2 = ext.clone();
    let tx2 = tx.clone();
    let stop_dec = stop_flag.clone();
    let t_dec = std::time::Instant::now();
    let dec_res = tokio::task::spawn_blocking(move || {
        let init = filters2.lock().unwrap().filters.clone();
        let mut pipeline = AudioPipeline::new(&init).map_err(|e| e.to_string())?;
        let mut first = true;
        pipeline
            .decode_stream_to_opus(buf_dec, ext2.as_deref(), filters2, |opus| {
                if first {
                    first = false;
                    tracing::info!(
                        first_frame_ms = t_dec.elapsed().as_millis() as u64,
                        "playback: first opus frame (decode started)"
                    );
                }
                !stop_dec.load(Ordering::Relaxed) && tx2.blocking_send(opus).is_ok()
            })
            .map_err(|e| e.to_string())
    })
    .await;

    dl.abort();
    tracing::info!(
        total_ms = t0.elapsed().as_millis() as u64,
        bytes = buf.lock().unwrap().downloaded(),
        "playback: stream finished"
    );

    match dec_res {
        Ok(Ok(())) => {
            // 末尾の無音送出が届くよう少し待つ。
            tokio::time::sleep(Duration::from_secs(2)).await;
            TrackEndReason::Finished
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "decode failed");
            TrackEndReason::LoadFailed
        }
        Err(_) => TrackEndReason::Stopped, // join エラー（abort/panic）
    }
}
