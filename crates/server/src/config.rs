//! `application.yml` 互換の設定読み込み（チケット 1-3）。
//!
//! 既存 Lavalink と同じ構造の YAML を読み込む。値は環境変数でも上書き可能
//! （best-effort: `SERVER_PORT`, `LAVALINK_SERVER_PASSWORD` のように `_` 区切り。
//! camelCase キーの env 上書きは Spring の relaxed binding とは完全一致しない点に注意）。

use serde::Deserialize;

/// トップレベル設定。未知キー（metrics/logging 等）は serde が無視する。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub lavalink: LavalinkConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub port: u16,
    pub address: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { port: 2333, address: "0.0.0.0".to_string() }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LavalinkConfig {
    pub server: LavalinkServer,
}

/// 注意: config クレートはファイル/環境変数のキーをマージ時にすべて小文字化する
/// （`Expression::set` が `to_lowercase()` を行う）。そのため camelCase キーは
/// `#[serde(rename_all = "camelCase")]` だけでは一致せず、既定値へ静かにフォールバック
/// してしまう（playerUpdateInterval が常に 5 になるバグの原因）。
/// 全小文字の alias を併記して両方のキー形を受け付ける。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LavalinkServer {
    pub password: String,
    pub sources: Sources,
    pub filters: Filters,
    #[serde(alias = "bufferdurationms")]
    pub buffer_duration_ms: u32,
    #[serde(alias = "framebufferdurationms")]
    pub frame_buffer_duration_ms: u32,
    #[serde(alias = "opusencodingquality")]
    pub opus_encoding_quality: u8,
    #[serde(alias = "resamplingquality")]
    pub resampling_quality: ResamplingQuality,
    #[serde(alias = "trackstuckthresholdms")]
    pub track_stuck_threshold_ms: u64,
    #[serde(alias = "useseekghosting")]
    pub use_seek_ghosting: bool,
    #[serde(alias = "youtubeplaylistloadlimit")]
    pub youtube_playlist_load_limit: u32,
    /// playerUpdate op の送信間隔（秒）。
    #[serde(alias = "playerupdateinterval")]
    pub player_update_interval: u64,
    #[serde(alias = "youtubesearchenabled")]
    pub youtube_search_enabled: bool,
    #[serde(alias = "soundcloudsearchenabled")]
    pub soundcloud_search_enabled: bool,
}

impl Default for LavalinkServer {
    fn default() -> Self {
        Self {
            password: "youshallnotpass".to_string(),
            sources: Sources::default(),
            filters: Filters::default(),
            buffer_duration_ms: 400,
            frame_buffer_duration_ms: 5000,
            opus_encoding_quality: 10,
            resampling_quality: ResamplingQuality::Low,
            track_stuck_threshold_ms: 10_000,
            use_seek_ghosting: true,
            youtube_playlist_load_limit: 6,
            player_update_interval: 5,
            youtube_search_enabled: true,
            soundcloud_search_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Sources {
    pub youtube: bool,
    pub bandcamp: bool,
    pub soundcloud: bool,
    pub twitch: bool,
    pub vimeo: bool,
    pub nico: bool,
    pub http: bool,
    pub local: bool,
}

impl Default for Sources {
    fn default() -> Self {
        // 公式 Lavalink の既定に合わせる（local のみ false）。
        Self {
            youtube: true,
            bandcamp: true,
            soundcloud: true,
            twitch: true,
            vimeo: true,
            nico: true,
            http: true,
            local: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Filters {
    pub volume: bool,
    pub equalizer: bool,
    pub karaoke: bool,
    pub timescale: bool,
    pub tremolo: bool,
    pub vibrato: bool,
    pub distortion: bool,
    pub rotation: bool,
    #[serde(alias = "channelmix")]
    pub channel_mix: bool,
    #[serde(alias = "lowpass")]
    pub low_pass: bool,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            volume: true,
            equalizer: true,
            karaoke: true,
            timescale: true,
            tremolo: true,
            vibrato: true,
            distortion: true,
            rotation: true,
            channel_mix: true,
            low_pass: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ResamplingQuality {
    #[default]
    Low,
    Medium,
    High,
}

/// 作業ディレクトリの設定ファイル ＋ 環境変数から設定を構築する。
/// ファイルが無くても既定値で起動できる。
///
/// 読み込み優先度（後のソースほど優先）:
///   1. `application.example.yml`（同梱サンプル。編集すればそのまま反映される）
///   2. `application.yml` / `application.yaml`（公式 Lavalink 互換。あればサンプルを上書き）
///   3. 環境変数
pub fn load() -> Result<AppConfig, config::ConfigError> {
    config::Config::builder()
        .add_source(config::File::with_name("application.example").required(false))
        .add_source(config::File::with_name("application").required(false))
        .add_source(config::Environment::default().separator("_").ignore_empty(true))
        .build()?
        .try_deserialize()
}

/// どの設定ファイルが作業ディレクトリに存在するかを返す（起動ログ用）。
pub fn config_files_present() -> Vec<String> {
    ["application.yml", "application.yaml", "application.example.yml"]
        .iter()
        .filter(|f| std::path::Path::new(f).exists())
        .map(|f| f.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::{Config, File, FileFormat};

    // ワークスペース直下の application.example.yml を取り込んで検証する。
    const SAMPLE: &str = include_str!("../../../application.example.yml");

    #[test]
    fn parses_example_yaml() {
        let cfg: AppConfig = Config::builder()
            .add_source(File::from_str(SAMPLE, FileFormat::Yaml))
            .build()
            .expect("build")
            .try_deserialize()
            .expect("deserialize");

        assert_eq!(cfg.server.port, 2333);
        assert_eq!(cfg.server.address, "0.0.0.0");
        assert_eq!(cfg.lavalink.server.password, "youshallnotpass");
        assert!(cfg.lavalink.server.sources.youtube);
        assert!(!cfg.lavalink.server.sources.soundcloud);
        assert_eq!(cfg.lavalink.server.player_update_interval, 5);
        assert_eq!(cfg.lavalink.server.resampling_quality, ResamplingQuality::Low);
        assert_eq!(cfg.lavalink.server.track_stuck_threshold_ms, 10_000);
    }

    // config クレートはマージ時にキーを小文字化するため、camelCase キーが
    // 既定値へフォールバックしていないことを「非デフォルト値」で検証する回帰テスト。
    #[test]
    fn camelcase_keys_override_defaults() {
        let yaml = r#"
lavalink:
  server:
    playerUpdateInterval: 1
    bufferDurationMs: 800
    frameBufferDurationMs: 1000
    opusEncodingQuality: 5
    resamplingQuality: HIGH
    trackStuckThresholdMs: 5000
    useSeekGhosting: false
    youtubePlaylistLoadLimit: 3
    youtubeSearchEnabled: false
    soundcloudSearchEnabled: false
    filters:
      channelMix: false
      lowPass: false
"#;
        let cfg: AppConfig = Config::builder()
            .add_source(File::from_str(yaml, FileFormat::Yaml))
            .build()
            .expect("build")
            .try_deserialize()
            .expect("deserialize");

        let s = &cfg.lavalink.server;
        assert_eq!(s.player_update_interval, 1);
        assert_eq!(s.buffer_duration_ms, 800);
        assert_eq!(s.frame_buffer_duration_ms, 1000);
        assert_eq!(s.opus_encoding_quality, 5);
        assert_eq!(s.resampling_quality, ResamplingQuality::High);
        assert_eq!(s.track_stuck_threshold_ms, 5000);
        assert!(!s.use_seek_ghosting);
        assert_eq!(s.youtube_playlist_load_limit, 3);
        assert!(!s.youtube_search_enabled);
        assert!(!s.soundcloud_search_enabled);
        assert!(!s.filters.channel_mix);
        assert!(!s.filters.low_pass);
    }

    #[test]
    fn defaults_are_sane() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.server.port, 2333);
        assert_eq!(cfg.lavalink.server.opus_encoding_quality, 10);
    }
}
