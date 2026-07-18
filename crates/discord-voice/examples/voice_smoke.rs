//! 実 Discord 音声経路の単体疎通テスト（v0 トランスポート）。
//!
//! Bot が VC に参加したときに得られる値を環境変数で渡して実行する:
//!   GUILD_ID, USER_ID, SESSION_ID, VOICE_TOKEN, VOICE_ENDPOINT(host:port)
//! 5 秒間 無音 Opus を送る。成功すれば Discord 上で Bot が「発話中」表示になる。
//!
//! 実行: cargo run -p lavalink-discord-voice --example voice_smoke

use std::time::Duration;

use lavalink_discord_voice::{VoiceConfig, VoiceConnection, SILENCE_FRAME};

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("environment variable {k} is required"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = VoiceConfig {
        guild_id: env("GUILD_ID").parse()?,
        user_id: env("USER_ID").parse()?,
        session_id: env("SESSION_ID"),
        token: env("VOICE_TOKEN"),
        endpoint: env("VOICE_ENDPOINT"),
        video: false,
    };
    println!("connecting to voice endpoint {} ...", cfg.endpoint);
    let conn = VoiceConnection::connect(cfg).await?;
    println!("connected. sending 5s of silence (check the 'speaking' indicator in Discord)...");

    // 20ms ごとに無音フレームを投入（250 フレーム = 5 秒）。
    for _ in 0..250 {
        conn.send_opus_frame(SILENCE_FRAME.to_vec()).await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    conn.disconnect();
    println!("done.");
    Ok(())
}
