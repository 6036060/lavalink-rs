//! Lavalink v4 互換サーバーのエントリポイント。

use std::net::SocketAddr;

use lavalink_server::{build_app, config, ws, AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Windows のタイマ解像度を 1ms に上げる。既定 ~15.6ms だと tokio の 20ms 周期が
    // ~31ms に丸められ、音声送出が 50fps→33fps に落ちて途切れる（受信側バッファ枯渇）。
    #[cfg(windows)]
    {
        #[link(name = "winmm")]
        extern "system" {
            fn timeBeginPeriod(uPeriod: u32) -> u32;
        }
        unsafe {
            timeBeginPeriod(1);
        }
    }

    // reqwest(https)/将来の音声で使う rustls 0.23 の既定 CryptoProvider を導入。
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cfg = config::load()?;
    let addr: SocketAddr = format!("{}:{}", cfg.server.address, cfg.server.port).parse()?;
    let ipv4_only = cfg.server.address == "0.0.0.0";

    // 設定ファイルの検出状況と、実際に適用された主要値をログに出す（設定が効いているか確認用）。
    let files = config::config_files_present();
    if files.is_empty() {
        tracing::warn!(
            "no config file found (application.yml / application.example.yml). using built-in \
             defaults. create application.yml in the working directory to override."
        );
    } else {
        tracing::info!(config_files = ?files, "loaded config file(s)");
    }
    tracing::info!(
        %addr,
        youtube = cfg.lavalink.server.sources.youtube,
        player_update_interval = cfg.lavalink.server.player_update_interval,
        "starting lavalink-rs"
    );

    let shared = AppState::new(cfg);
    tokio::spawn(ws::dispatcher(shared.clone()));

    let app = build_app(shared.clone());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "listening");
    if ipv4_only {
        tracing::info!(
            "note: bound to IPv4 only. A client using host \"localhost\" on Windows may resolve to \
             IPv6 (::1) and fail to connect. Point clients at 127.0.0.1."
        );
    }
    axum::serve(listener, app).await?;
    Ok(())
}
