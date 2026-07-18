//! Lavalink v4 互換サーバーのエントリポイント。

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use lavalink_server::{build_app, config, ws, AppState};

/// 設定のアドレス文字列からリスナー群を作る。
///
/// `0.0.0.0`（既定）は「全インターフェース」の意図なので、IPv4 と IPv6 の両方に
/// bind する。従来は IPv4 のみで、Windows では `localhost` が `::1` に解決されて
/// 接続に失敗する問題があった（localhost 接続問題の修正）。
/// - Linux 等では `[::]` が dual-stack のことがあり、その場合 IPv4 側の bind は
///   EADDRINUSE になるが、`[::]` 側が IPv4 も受けるので問題ない。
/// - `localhost` のようなホスト名も解決して bind できるようにする。
async fn bind_listeners(address: &str, port: u16) -> anyhow::Result<Vec<tokio::net::TcpListener>> {
    let address = address.trim();
    let mut listeners = Vec::new();

    if address == "0.0.0.0" || address == "*" || address.is_empty() {
        // IPv6 側を先に bind（dual-stack ならこれだけで IPv4 も受けられる）。
        let v6 = SocketAddr::from((Ipv6Addr::UNSPECIFIED, port));
        match tokio::net::TcpListener::bind(v6).await {
            Ok(l) => {
                tracing::info!(addr = %v6, "listening (IPv6, \"localhost\"=::1 もこちらで受ける)");
                listeners.push(l);
            }
            Err(e) => tracing::warn!(error = %e, "IPv6 [::] bind failed; IPv4 のみで継続"),
        }
        let v4 = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
        match tokio::net::TcpListener::bind(v4).await {
            Ok(l) => {
                tracing::info!(addr = %v4, "listening (IPv4)");
                listeners.push(l);
            }
            Err(e) if !listeners.is_empty() => {
                // [::] が dual-stack で IPv4 も掴んでいるケース。
                tracing::info!(error = %e, "IPv4 bind skipped (IPv6 socket が dual-stack で IPv4 も受信)");
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        // 明示アドレス。IP リテラルに加え "localhost" 等のホスト名も解決する。
        let addrs: Vec<SocketAddr> = match format!("{address}:{port}").parse::<SocketAddr>() {
            Ok(a) => vec![a],
            Err(_) => tokio::net::lookup_host((address, port)).await?.collect(),
        };
        let mut last_err: Option<std::io::Error> = None;
        for a in addrs {
            match tokio::net::TcpListener::bind(a).await {
                Ok(l) => {
                    tracing::info!(addr = %a, "listening");
                    listeners.push(l);
                }
                Err(e) => last_err = Some(e),
            }
        }
        if listeners.is_empty() {
            return Err(last_err
                .map(anyhow::Error::from)
                .unwrap_or_else(|| anyhow::anyhow!("no address to bind for {address}:{port}")));
        }
    }
    Ok(listeners)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Windows のタイマ精度対策（音声送出 50fps 維持）:
    // 1) timeBeginPeriod(1): タイマ解像度を 1ms に上げる。既定 ~15.6ms だと tokio の
    //    20ms 周期が ~31ms に丸められ、送出が 50fps→約32fps に落ちて途切れる。
    // 2) SetProcessInformation(ProcessPowerThrottling): Windows 11 はウィンドウ
    //    （コンソール含む）が最小化/非表示になるとプロセスのタイマ解像度要求を
    //    無視するため、IGNORE_TIMER_RESOLUTION のオプトアウトで「最小化中も 1ms を
    //    維持」を明示する。あわせて EXECUTION_SPEED(EcoQoS による速度抑制) も
    //    オプトアウトする。（コンソール最小化で frames_per_sec が 30 前後に
    //    落ちる問題の修正）
    #[cfg(windows)]
    {
        #[link(name = "winmm")]
        extern "system" {
            fn timeBeginPeriod(uPeriod: u32) -> u32;
        }

        // https://learn.microsoft.com/windows/win32/api/processthreadsapi/ns-processthreadsapi-process_power_throttling_state
        #[repr(C)]
        struct ProcessPowerThrottlingState {
            version: u32,
            control_mask: u32,
            state_mask: u32,
        }
        const PROCESS_POWER_THROTTLING_CURRENT_VERSION: u32 = 1;
        const PROCESS_POWER_THROTTLING_EXECUTION_SPEED: u32 = 0x1;
        const PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION: u32 = 0x4;
        // PROCESS_INFORMATION_CLASS::ProcessPowerThrottling
        const PROCESS_POWER_THROTTLING: i32 = 4;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentProcess() -> isize;
            fn SetProcessInformation(
                process: isize,
                class: i32,
                info: *mut core::ffi::c_void,
                size: u32,
            ) -> i32;
        }

        unsafe {
            timeBeginPeriod(1);
            let mut st = ProcessPowerThrottlingState {
                version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                control_mask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                    | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
                // StateMask=0: EcoQoS を適用しない ＆ タイマ解像度要求を常に尊重。
                state_mask: 0,
            };
            let ok = SetProcessInformation(
                GetCurrentProcess(),
                PROCESS_POWER_THROTTLING,
                &mut st as *mut ProcessPowerThrottlingState as *mut core::ffi::c_void,
                std::mem::size_of::<ProcessPowerThrottlingState>() as u32,
            );
            if ok == 0 {
                // Windows 10 等の未対応 OS では失敗するが、送出ループ側の追いつき
                // 送出（discord-voice::audio_task）で平均 50fps は維持される。
                tracing::warn!(
                    "SetProcessInformation(ProcessPowerThrottling) failed; relying on \
                     catch-up pacing to keep 50fps while minimized"
                );
            }
        }
    }

    // reqwest(https)/将来の音声で使う rustls 0.23 の既定 CryptoProvider を導入。
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cfg = config::load()?;
    let (bind_address, bind_port) = (cfg.server.address.clone(), cfg.server.port);

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
        address = %bind_address,
        port = bind_port,
        youtube = cfg.lavalink.server.sources.youtube,
        player_update_interval = cfg.lavalink.server.player_update_interval,
        "starting lavalink-rs"
    );

    let shared = AppState::new(cfg);
    tokio::spawn(ws::dispatcher(shared.clone()));

    let app = build_app(shared.clone());
    let listeners = bind_listeners(&bind_address, bind_port).await?;

    // 複数リスナー（IPv4/IPv6）で並行 serve。どれかが落ちたら終了。
    let mut serves = tokio::task::JoinSet::new();
    for listener in listeners {
        let app = app.clone();
        serves.spawn(async move { axum::serve(listener, app).await });
    }
    while let Some(res) = serves.join_next().await {
        res??;
    }
    Ok(())
}
