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

use lavalink_audio_pipeline::{AudioPipeline, SharedBuffer, SharedFilters, TsToAdts};
use lavalink_discord_voice::{VoiceConfig, VoiceConnection};
use lavalink_protocol::{
    Event, Exception, Filters, ServerMessage, Severity, Track, TrackEndReason, VoiceState,
};
use lavalink_source_youtube::{ResolvedUrl, YoutubeClient};

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
    /// 現在のトラックとハンドル（seek での再生し直しに使う）。
    current: Option<CurrentTrack>,
    /// identifier ごとに使い回す解決 URL / ダウンロード状態。
    /// seek やトラックループの再再生時に再解決・再ダウンロードを避ける
    /// （seek 後の長い無音 = 先頭からの再ダウンロード待ち の解消）。
    cache: Option<TrackCache>,
}

/// 1 トラックぶんの解決/ダウンロードの共有キャッシュ。
struct TrackCache {
    identifier: String,
    resolved: Arc<Mutex<Option<ResolvedUrl>>>,
    dl: Arc<Mutex<Option<SharedDl>>>,
}

/// 進行中 (または完了済み) のダウンロード共有状態。デコードをまたいで使い回す。
#[derive(Clone)]
struct SharedDl {
    buf: Arc<Mutex<SharedBuffer>>,
    ext: Option<String>,
    /// ダウンロード専用の停止フラグ (デコードの停止とは独立)。
    stop: Arc<AtomicBool>,
}

/// seek でストリームを開き直すために保持する再生コンテキスト。
#[derive(Clone)]
struct CurrentTrack {
    track: Track,
    session: Arc<Session>,
    youtube: Arc<YoutubeClient>,
    /// 解決済みストリーム URL (TrackCache と共有)。
    resolved: Arc<Mutex<Option<ResolvedUrl>>>,
    /// ダウンロード共有スロット (TrackCache と共有)。
    dl: Arc<Mutex<Option<SharedDl>>>,
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
            current: None,
            cache: None,
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
            // 映像 (実験的): LAVALINK_VIDEO=1 でオプトイン (docs/video-streaming-plan.md)
            video: std::env::var("LAVALINK_VIDEO").is_ok_and(|v| v == "1"),
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
    /// `start_ms` 指定でその位置から再生する（position 付き PATCH 対応）。
    pub fn play(
        &mut self,
        track: Track,
        filters: Filters,
        session: Arc<Session>,
        youtube: Arc<YoutubeClient>,
        start_ms: u64,
    ) {
        // このトラックの初期フィルタを共有設定へ反映（以降の変更はライブ適用される）。
        self.filters.lock().unwrap().update(filters);

        // 同一トラックの再再生 (トラックループ等) なら解決 URL / ダウンロードを使い回す。
        let reuse = matches!(&self.cache, Some(c) if c.identifier == track.info.identifier);
        if !reuse {
            self.stop_download();
            self.cache = Some(TrackCache {
                identifier: track.info.identifier.clone(),
                resolved: Arc::new(Mutex::new(None)),
                dl: Arc::new(Mutex::new(None)),
            });
        }
        let cache = self.cache.as_ref().unwrap();
        let cur = CurrentTrack {
            track,
            session,
            youtube,
            resolved: cache.resolved.clone(),
            dl: cache.dl.clone(),
        };
        self.current = Some(cur.clone());
        self.start_stream(cur, start_ms);
    }

    /// 進行中のダウンロードに停止を通知する (別トラックへ切り替えるとき)。
    fn stop_download(&self) {
        if let Some(c) = &self.cache {
            if let Some(d) = c.dl.lock().unwrap().as_ref() {
                d.stop.store(true, Ordering::Relaxed);
            }
        }
    }

    /// 再生中トラックの位置を変更する。ストリームを開き直して指定位置まで
    /// 高速スキップする（デコード直後の読み捨てなので実時間よりずっと速い）。
    pub fn seek(&mut self, position_ms: u64) {
        let Some(cur) = self.current.clone() else {
            return;
        };
        if cur.track.info.is_stream {
            return; // ライブ配信はシーク不可。
        }
        self.start_stream(cur, position_ms);
    }

    /// 既存の再生タスクを止め、指定位置からストリーミング再生タスクを起動する。
    fn start_stream(&mut self, cur: CurrentTrack, start_ms: u64) {
        self.stop_task();
        let Some(conn) = self.conn.as_ref() else {
            tracing::warn!(guild = self.guild_id, "play without voice connection");
            return;
        };
        let tx = conn.audio_sender();
        let guild_key = self.guild_key.clone();

        // 新しい再生用の停止フラグ。
        let stop_flag = Arc::new(AtomicBool::new(false));
        self.stop_flag = stop_flag.clone();
        let filters_arc = self.filters.clone();

        let task = tokio::spawn(async move {
            let CurrentTrack { track, session, youtube, resolved, dl } = cur;
            let reason = stream_track(
                &tx,
                &track,
                &youtube,
                filters_arc,
                stop_flag,
                start_ms,
                &resolved,
                &dl,
            )
            .await;
            {
                let mut players = session.players.lock().await;
                if let Some(p) = players.get_mut(&guild_key) {
                    p.set_real_playback(false);
                    let _ = p.stop();
                }
            }
            // 失敗はクライアントにも見える形で通知する (bot がチャンネルに表示できる)。
            if reason == TrackEndReason::LoadFailed {
                session.send(ServerMessage::Event(Event::TrackException {
                    guild_id: guild_key.clone(),
                    track: track.clone(),
                    exception: Exception {
                        message: Some(
                            "ストリームの解決/取得/デコードに失敗しました (サーバーログ参照)"
                                .to_string(),
                        ),
                        severity: Severity::Common,
                        cause: "playback".into(),
                        cause_stack_trace: String::new(),
                    },
                }));
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
        self.current = None;
        self.stop_task();
    }

    /// 再生タスクのみ止める（seek での開き直し用。current は保持）。
    fn stop_task(&mut self) {
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
/// `start_ms` まではデコード直後に読み捨てて高速スキップする。
///
/// - `resolved_cache`: 解決済み URL。あれば再解決しない（seek/再再生の高速化）。
/// - `dl_slot`: ダウンロード共有スロット。あれば**再ダウンロードせず**同じバッファから
///   デコードし直す。seek 後の長い無音（先頭からの再ダウンロード待ち）はこれで解消する。
///   Direct のダウンロードはデコード停止と独立に走り続けるので、seek を挟んでも進捗が残る。
#[allow(clippy::too_many_arguments)]
async fn stream_track(
    tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    track: &Track,
    youtube: &YoutubeClient,
    filters: Arc<Mutex<SharedFilters>>,
    stop_flag: Arc<AtomicBool>,
    start_ms: u64,
    resolved_cache: &Mutex<Option<ResolvedUrl>>,
    dl_slot: &Mutex<Option<SharedDl>>,
) -> TrackEndReason {
    let t0 = std::time::Instant::now();

    // 既存ダウンロードの再利用 (seek / 同一トラックの再再生)。
    let existing = dl_slot.lock().unwrap().clone();
    let (ext, buf, hls_task) = if let Some(d) = existing {
        tracing::info!(
            bytes = d.buf.lock().unwrap().downloaded(),
            "playback: reusing existing download buffer"
        );
        (d.ext.clone(), d.buf.clone(), None)
    } else {
        // ソースに応じて再生ストリームを解決（キャッシュ優先）。
        let cached = resolved_cache.lock().unwrap().clone();
        let resolved = if let Some(r) = cached {
            r
        } else if track.info.source_name == "youtube" {
            match youtube.stream_url(&track.info.identifier).await {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!(error = %e, id = %track.info.identifier, "youtube resolve failed");
                    return TrackEndReason::LoadFailed;
                }
            }
        } else {
            match track.info.uri.clone() {
                Some(u) if u.split('?').next().unwrap_or("").ends_with(".m3u8") => {
                    ResolvedUrl::Hls { url: u, user_agent: None }
                }
                Some(u) => ResolvedUrl::Direct(u),
                None => return TrackEndReason::LoadFailed,
            }
        };
        *resolved_cache.lock().unwrap() = Some(resolved.clone());
        tracing::info!(resolve_ms = t0.elapsed().as_millis() as u64, "playback: url resolved");

        match resolved {
            ResolvedUrl::Direct(url) => {
                let ext = url
                    .split('?')
                    .next()
                    .unwrap_or(&url)
                    .rsplit('.')
                    .next()
                    .filter(|e| {
                        !e.is_empty()
                            && e.len() <= 5
                            && e.chars().all(|c| c.is_ascii_alphanumeric())
                    })
                    .map(|e| e.to_string());

                // ストリーミング GET。
                let resp = match reqwest::get(&url).await.and_then(|r| r.error_for_status()) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(error = %e, "track fetch failed");
                        // 失敗した URL は捨てて次回に再解決させる。
                        *resolved_cache.lock().unwrap() = None;
                        return TrackEndReason::LoadFailed;
                    }
                };
                let total = resp.content_length();
                let buf = Arc::new(Mutex::new(SharedBuffer::new(total)));

                // ダウンロードタスク: チャンクをバッファへ追記。
                // デコードの停止 (seek での開き直し) とは独立した専用フラグで止める。
                let dl_stop = Arc::new(AtomicBool::new(false));
                let buf_dl = buf.clone();
                let stop_dl = dl_stop.clone();
                tokio::spawn(async move {
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
                *dl_slot.lock().unwrap() =
                    Some(SharedDl { buf: buf.clone(), ext: ext.clone(), stop: dl_stop });
                (ext, buf, None)
            }
            ResolvedUrl::Hls { url: manifest_url, user_agent } => {
                // ライブ配信等の HLS。セグメント(MPEG-TS)から AAC(ADTS) を抽出して
                // 共有バッファへ流し、symphonia の AdtsReader（ヒント "aac"）でデコードする。
                // ライブは追記専用でシーク対象外のため dl_slot にはキャッシュしない。
                tracing::info!("playback: HLS stream (live)");
                let buf = Arc::new(Mutex::new(SharedBuffer::new(None)));
                let dl = tokio::spawn(hls_download(
                    manifest_url,
                    user_agent,
                    buf.clone(),
                    stop_flag.clone(),
                ));
                (Some("aac".to_string()), buf, Some(dl))
            }
        }
    };

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
            .decode_stream_to_opus(buf_dec, ext2.as_deref(), filters2, start_ms, |opus| {
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

    // HLS のダウンロードだけデコード終了で止める (Direct は完走させて再利用に備える)。
    if let Some(t) = hls_task {
        t.abort();
    }
    tracing::info!(
        total_ms = t0.elapsed().as_millis() as u64,
        bytes = buf.lock().unwrap().downloaded(),
        "playback: stream finished"
    );

    match dec_res {
        Ok(Ok(())) => {
            // 送出キューに残ったフレームが鳴り終わるまで待つ（最大 3 秒）。
            // 固定 2 秒待ちだと曲間に無駄なギャップができるため、キューが
            // 空になった時点 (+100ms) で TrackEnd を発火する。
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while tx.capacity() < tx.max_capacity() && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            TrackEndReason::Finished
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "decode failed");
            // 壊れた取得状態を捨て、次の再生/リトライで再解決・再ダウンロードさせる。
            if let Some(d) = dl_slot.lock().unwrap().take() {
                d.stop.store(true, Ordering::Relaxed);
            }
            *resolved_cache.lock().unwrap() = None;
            TrackEndReason::LoadFailed
        }
        Err(_) => TrackEndReason::Stopped, // join エラー（abort/panic）
    }
}

// ----------------------------- HLS（ライブ配信） -----------------------------

async fn http_text(client: &reqwest::Client, url: &str) -> Option<String> {
    client.get(url).send().await.ok()?.error_for_status().ok()?.text().await.ok()
}

/// HLS ダウンロードタスク（主に YouTube ライブ）。
///
/// マスタープレイリストなら最小 BANDWIDTH のバリアント（映像+音声の muxed TS。
/// 音声品質は共通なので帯域節約のため最小を選ぶ）を選択し、メディアプレイリストを
/// ポーリングしながら新しいセグメントを取得。MPEG-TS から AAC(ADTS) を抽出して
/// 共有バッファへ追記する。`#EXT-X-ENDLIST`（配信終了）で finish。
///
/// 注意: SharedBuffer は追記専用のため長時間のライブではメモリが増える
/// （ADTS 128kbps ≈ 60MB/時）。長期配信向けのリングバッファ化は今後の課題。
async fn hls_download(
    manifest_url: String,
    user_agent: Option<String>,
    buf: Arc<Mutex<SharedBuffer>>,
    stop: Arc<AtomicBool>,
) {
    // googlevideo は URL を発行したクライアントと UA が一致しないセグメント取得を
    // 403 にすることがあるため、解決時のクライアント UA をそのまま使う。
    let mut cb = reqwest::Client::builder();
    if let Some(ua) = &user_agent {
        cb = cb.user_agent(ua.clone());
    }
    let client = cb.build().unwrap_or_else(|_| reqwest::Client::new());

    // マスター → メディアプレイリスト解決（ネスト対策で最大 2 段）。
    let mut playlist_url = manifest_url;
    for _ in 0..2 {
        let Some(text) = http_text(&client, &playlist_url).await else {
            tracing::warn!(url = %playlist_url, "hls: playlist fetch failed");
            buf.lock().unwrap().finish();
            return;
        };
        if !text.contains("#EXT-X-STREAM-INF") {
            break; // 既にメディアプレイリスト
        }
        match select_hls_variant(&text, &playlist_url) {
            Some(v) => playlist_url = v,
            None => {
                tracing::warn!("hls: no variant found in master playlist");
                buf.lock().unwrap().finish();
                return;
            }
        }
    }

    let mut ts = TsToAdts::new();
    let mut last_seq: i64 = -1;
    // 連続失敗カウンタ。PoToken 不足等で全セグメントが 403 になる場合に
    // 無限ポーリングせず打ち切る（バッファ 0 のままなのでデコード側が LoadFailed になる）。
    let mut consecutive_failures: u32 = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if consecutive_failures >= 15 {
            tracing::warn!(
                "hls: giving up after repeated segment failures (403 の場合は PoToken 供給 \
                 (YT_POTOKEN_PROVIDER / YT_POTOKEN) の設定を確認してください)"
            );
            break;
        }
        let Some(text) = http_text(&client, &playlist_url).await else {
            tracing::warn!("hls: media playlist fetch failed");
            break;
        };
        let pl = parse_media_playlist(&text, &playlist_url);
        // 初回はライブエッジ付近（末尾 3 セグメント）から開始する。
        // playlist_type=DVR だと過去分（数時間分になり得る）が全部載っているため、
        // 先頭から取得すると大幅に遅延した再生とダウンロードの浪費になる。
        if last_seq < 0 {
            if let Some((max_seq, _)) = pl.segments.last() {
                last_seq = max_seq - 3;
            }
        }
        for (seq, seg_url) in &pl.segments {
            if *seq <= last_seq {
                continue;
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let bytes = match client.get(seg_url).send().await {
                Ok(r) => match r.error_for_status() {
                    Ok(r) => r.bytes().await.ok(),
                    Err(e) => {
                        tracing::warn!(error = %e, seq, "hls: segment fetch failed");
                        None
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, seq, "hls: segment request failed");
                    None
                }
            };
            if let Some(bytes) = bytes {
                let adts = ts.push(&bytes);
                if !adts.is_empty() {
                    buf.lock().unwrap().push(&adts);
                }
                consecutive_failures = 0;
            } else {
                consecutive_failures += 1;
            }
            // 取得失敗したセグメントは飛ばして先へ進む（ライブは追いつき優先）。
            last_seq = *seq;
        }
        if pl.ended {
            tracing::info!("hls: stream ended (ENDLIST)");
            break;
        }
        // ターゲット長の半分間隔でポーリング（1〜5 秒にクランプ）。
        let wait = (pl.target_duration / 2.0).clamp(1.0, 5.0);
        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
    }
    buf.lock().unwrap().finish();
}

struct MediaPlaylist {
    target_duration: f64,
    ended: bool,
    /// (メディアシーケンス番号, セグメント URL)
    segments: Vec<(i64, String)>,
}

/// メディアプレイリストを解析する。
fn parse_media_playlist(text: &str, base_url: &str) -> MediaPlaylist {
    let mut seq: i64 = 0;
    let mut target = 5.0;
    let mut ended = false;
    let mut segments = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target = v.trim().parse().unwrap_or(5.0);
        } else if line == "#EXT-X-ENDLIST" {
            ended = true;
        } else if !line.starts_with('#') {
            segments.push((seq, resolve_hls_url(base_url, line)));
            seq += 1;
        }
    }
    MediaPlaylist { target_duration: target, ended, segments }
}

/// マスタープレイリストから最小 BANDWIDTH のバリアント URL を選ぶ。
fn select_hls_variant(master: &str, base_url: &str) -> Option<String> {
    let mut best: Option<(u64, String)> = None;
    let mut lines = master.lines();
    while let Some(line) = lines.next() {
        let line = line.trim();
        if !line.starts_with("#EXT-X-STREAM-INF") {
            continue;
        }
        let bw = line
            .split("BANDWIDTH=")
            .nth(1)
            .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        // 属性行の次の非タグ行がバリアント URI。
        for next in lines.by_ref() {
            let next = next.trim();
            if next.is_empty() || next.starts_with('#') {
                continue;
            }
            if best.as_ref().map(|(b, _)| bw < *b).unwrap_or(true) {
                best = Some((bw, resolve_hls_url(base_url, next)));
            }
            break;
        }
    }
    best.map(|(_, u)| u)
}

/// プレイリスト内 URI を絶対 URL に解決する（絶対 URL はそのまま）。
fn resolve_hls_url(base: &str, uri: &str) -> String {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return uri.to_string();
    }
    if uri.starts_with('/') {
        // path-absolute: scheme://host まで + uri
        if let Some(scheme_end) = base.find("://") {
            let after = &base[scheme_end + 3..];
            if let Some(path_start) = after.find('/') {
                return format!("{}{}", &base[..scheme_end + 3 + path_start], uri);
            }
        }
        return format!("{base}{uri}");
    }
    // 相対: base のファイル名部分を差し替え。
    match base.rfind('/') {
        Some(i) => format!("{}{}", &base[..=i], uri),
        None => uri.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_lowest_bandwidth_variant() {
        let master = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,CODECS=\"avc1,mp4a\"\n\
            https://example.com/hi/index.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=300000,RESOLUTION=256x144,CODECS=\"avc1,mp4a\"\n\
            https://example.com/lo/index.m3u8\n";
        let v = select_hls_variant(master, "https://example.com/master.m3u8").unwrap();
        assert_eq!(v, "https://example.com/lo/index.m3u8");
    }

    #[test]
    fn parses_media_playlist_with_sequence() {
        let media = "#EXTM3U\n\
            #EXT-X-TARGETDURATION:4\n\
            #EXT-X-MEDIA-SEQUENCE:100\n\
            #EXTINF:4.0,\n\
            seg100.ts\n\
            #EXTINF:4.0,\n\
            seg101.ts\n";
        let pl = parse_media_playlist(media, "https://ex.com/live/index.m3u8");
        assert!(!pl.ended);
        assert_eq!(pl.target_duration, 4.0);
        assert_eq!(
            pl.segments,
            vec![
                (100, "https://ex.com/live/seg100.ts".to_string()),
                (101, "https://ex.com/live/seg101.ts".to_string()),
            ]
        );
    }

    #[test]
    fn detects_endlist() {
        let media = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:4.0,\na.ts\n#EXT-X-ENDLIST\n";
        assert!(parse_media_playlist(media, "https://ex.com/p.m3u8").ended);
    }

    #[test]
    fn resolves_relative_and_absolute_urls() {
        let base = "https://host.example/api/manifest/hls_playlist/x/index.m3u8";
        assert_eq!(
            resolve_hls_url(base, "seg1.ts"),
            "https://host.example/api/manifest/hls_playlist/x/seg1.ts"
        );
        assert_eq!(resolve_hls_url(base, "/abs/seg1.ts"), "https://host.example/abs/seg1.ts");
        assert_eq!(resolve_hls_url(base, "https://o.example/s.ts"), "https://o.example/s.ts");
    }
}
