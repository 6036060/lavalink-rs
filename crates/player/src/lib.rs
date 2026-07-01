//! プレイヤー状態機械。
//!
//! フェーズ2 ではモックプレイヤー（実音声なし・状態と時間進行のみ）を実装し、
//! 実クライアントとの疎通を先に確認する（推奨進め方 #2）。
//! フェーズ4 で audio-pipeline / discord-voice と接続して実再生に置き換える。

#![forbid(unsafe_code)]

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use lavalink_protocol::{Filters, Player, PlayerState, Track, VoiceState};

/// 実音声を伴わないプレイヤー状態機械。時間経過で position が進み、
/// length / endTime 到達でトラック終了を検知できる。
#[derive(Debug)]
pub struct MockPlayer {
    pub guild_id: String,
    track: Option<Track>,
    volume: u16,
    paused: bool,
    voice: VoiceState,
    filters: Filters,
    end_time_ms: Option<u64>,
    /// 直近の seek / resume 時点での位置。
    position_base_ms: u64,
    /// 再生中（track あり・未 pause）のとき計測開始時刻。
    playing_since: Option<Instant>,
    /// 実音声再生がサーバー側で駆動されている場合 true（終了検知は再生タスクが行う）。
    real_playback: bool,
}

impl MockPlayer {
    pub fn new(guild_id: impl Into<String>) -> Self {
        Self {
            guild_id: guild_id.into(),
            track: None,
            volume: 100,
            paused: false,
            voice: VoiceState::default(),
            filters: Filters::default(),
            end_time_ms: None,
            position_base_ms: 0,
            playing_since: None,
            real_playback: false,
        }
    }

    pub fn track(&self) -> Option<&Track> {
        self.track.as_ref()
    }
    pub fn is_paused(&self) -> bool {
        self.paused
    }
    pub fn has_track(&self) -> bool {
        self.track.is_some()
    }
    /// 実音声再生モードの設定（true の間は poll_finished が発火しない）。
    pub fn set_real_playback(&mut self, on: bool) {
        self.real_playback = on;
    }
    fn is_active(&self) -> bool {
        self.track.is_some() && !self.paused
    }

    /// トラック長（ms）。
    fn length_ms(&self) -> u64 {
        self.track.as_ref().map(|t| t.info.length).unwrap_or(0)
    }

    /// 終了とみなす位置（endTime 指定があればそれ、無ければ length）。
    fn effective_end_ms(&self) -> u64 {
        match self.end_time_ms {
            Some(e) => e.min(self.length_ms()).max(0),
            None => self.length_ms(),
        }
    }

    /// 現在位置（ms）。length を超えない。
    pub fn position_ms(&self) -> u64 {
        let raw = match self.playing_since {
            Some(since) => self.position_base_ms + since.elapsed().as_millis() as u64,
            None => self.position_base_ms,
        };
        if self.track.is_some() {
            raw.min(self.length_ms())
        } else {
            raw
        }
    }

    pub fn connected(&self) -> bool {
        self.voice.is_complete()
    }

    // ----- 制御 -----

    /// 新しいトラックを再生開始。position は 0 から。
    pub fn play(&mut self, track: Track) {
        self.track = Some(track);
        self.position_base_ms = 0;
        self.playing_since = if self.paused { None } else { Some(Instant::now()) };
    }

    pub fn stop(&mut self) -> Option<Track> {
        self.playing_since = None;
        self.position_base_ms = 0;
        self.end_time_ms = None;
        self.track.take()
    }

    pub fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        if paused {
            // 現在位置で凍結。
            self.position_base_ms = self.position_ms();
            self.playing_since = None;
        } else if self.track.is_some() {
            self.playing_since = Some(Instant::now());
        }
        self.paused = paused;
    }

    pub fn seek(&mut self, position_ms: u64) {
        self.position_base_ms = position_ms.min(self.length_ms().max(position_ms));
        self.playing_since = if self.is_active() { Some(Instant::now()) } else { None };
    }

    pub fn set_volume(&mut self, volume: u16) {
        self.volume = volume.min(1000);
    }

    pub fn set_voice(&mut self, voice: VoiceState) {
        self.voice = voice;
    }

    pub fn set_filters(&mut self, filters: Filters) {
        self.filters = filters;
    }

    /// endTime を設定（`None` でリセット）。
    pub fn set_end_time(&mut self, end_time_ms: Option<u64>) {
        self.end_time_ms = end_time_ms;
    }

    /// 終了到達なら現在トラックを取り出して返す（TrackEnd 発火用）。
    pub fn poll_finished(&mut self) -> Option<Track> {
        if self.real_playback {
            return None;
        }
        if self.is_active() && self.position_ms() >= self.effective_end_ms() {
            self.playing_since = None;
            self.position_base_ms = 0;
            self.end_time_ms = None;
            self.track.take()
        } else {
            None
        }
    }

    // ----- スナップショット -----

    pub fn state(&self) -> PlayerState {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let connected = self.connected();
        PlayerState {
            time: now,
            position: self.position_ms() as i64,
            connected,
            ping: if connected { 0 } else { -1 },
        }
    }

    pub fn to_player(&self) -> Player {
        Player {
            guild_id: self.guild_id.clone(),
            track: self.track.clone(),
            volume: self.volume,
            paused: self.paused,
            state: self.state(),
            voice: self.voice.clone(),
            filters: self.filters.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lavalink_protocol::{Track, TrackInfo};

    fn sample_track(length: u64) -> Track {
        Track::new(
            "enc".into(),
            TrackInfo {
                identifier: "id".into(),
                is_seekable: true,
                author: "a".into(),
                length,
                is_stream: false,
                position: 0,
                title: "t".into(),
                uri: None,
                artwork_url: None,
                isrc: None,
                source_name: "youtube".into(),
            },
        )
    }

    #[test]
    fn pause_freezes_position() {
        let mut p = MockPlayer::new("g");
        p.play(sample_track(100_000));
        p.seek(5_000);
        p.set_paused(true);
        let pos = p.position_ms();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(p.position_ms(), pos, "paused position must not advance");
        assert!(pos >= 5_000);
    }

    #[test]
    fn finishes_at_end() {
        let mut p = MockPlayer::new("g");
        p.play(sample_track(0)); // 長さ0 → 即終了扱い
        let ended = p.poll_finished();
        assert!(ended.is_some());
        assert!(!p.has_track());
    }

    #[test]
    fn end_time_caps_finish() {
        let mut p = MockPlayer::new("g");
        p.play(sample_track(100_000));
        p.set_end_time(Some(0));
        assert!(p.poll_finished().is_some());
    }
}
