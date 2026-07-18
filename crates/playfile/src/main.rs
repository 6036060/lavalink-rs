//! ローカル音声/映像ファイルの再生/書き出しツール。
//!
//! 3 つのモード:
//!   (1) WAV 書き出し（Discord 不要・確実に耳で確認）:
//!         cargo run -p lavalink-playfile -- input.m4a output.wav
//!   (2) Discord VC へ送出（v0, voice.env の音声情報を使用）:
//!         cargo run -p lavalink-playfile -- input.m4a
//!   (3) 映像テスト送出（実験的, .h264 = Annex-B 生ストリーム）:
//!         ffmpeg -i input.mp4 -an -c:v libx264 -profile:v baseline -pix_fmt yuv420p \
//!                -g 30 -bsf:v h264_mp4toannexb out.h264
//!         cargo run -p lavalink-playfile -- out.h264      (VIDEO_FPS=30 が既定)
//!
//! 音声情報(モード2/3)は voice.env を自動で読む (サーバーを LAVALINK_WRITE_VOICE_ENV=1 で
//! 起動して /play すると出力される)。環境変数が優先。

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use lavalink_audio_pipeline::{decoder, AudioPipeline};
use lavalink_discord_voice::{split_annex_b, VideoFrame, VoiceConfig, VoiceConnection};
use lavalink_protocol::Filters;

fn pick(file: &HashMap<String, String>, key: &str) -> anyhow::Result<String> {
    std::env::var(key)
        .ok()
        .or_else(|| file.get(key).cloned())
        .with_context(|| format!("missing {key} (set env var, or /play first to write voice.env)"))
}

/// 48kHz ステレオ f32 を 16bit PCM WAV として書き出す。
fn write_wav(path: &str, samples: &[f32]) -> anyhow::Result<()> {
    let data_bytes = (samples.len() * 2) as u32; // i16
    let byte_rate: u32 = 48_000 * 2 * 2;
    let f = std::fs::File::create(path)?;
    let mut w = BufWriter::new(f);
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_bytes).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // fmt chunk size
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&2u16.to_le_bytes())?; // channels
    w.write_all(&48_000u32.to_le_bytes())?; // sample rate
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&4u16.to_le_bytes())?; // block align (2ch * 2byte)
    w.write_all(&16u16.to_le_bytes())?; // bits per sample
    w.write_all(b"data")?;
    w.write_all(&data_bytes.to_le_bytes())?;
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        w.write_all(&v.to_le_bytes())?;
    }
    w.flush()?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: playfile <input> [output.wav]"))?;
    let ext = Path::new(&path).extension().and_then(|e| e.to_str()).map(str::to_owned);
    let data = std::fs::read(&path)?;

    // ---- モード(1): WAV 書き出し（第2引数があれば。Discord 不要）----
    if let Some(out) = std::env::args().nth(2) {
        println!("decoding {path} -> WAV ...");
        let pcm = decoder::decode(data, ext.as_deref())?;
        let seconds = pcm.len() as f64 / 2.0 / 48_000.0;
        write_wav(&out, &pcm)?;
        println!(
            "wrote {out} ({seconds:.1}s, 48kHz stereo) — 任意のプレイヤーで再生して確認できます"
        );
        return Ok(());
    }

    // ---- モード(2): Discord VC へ送出（v0）----
    let mut file_vals: HashMap<String, String> = HashMap::new();
    if let Ok(text) = std::fs::read_to_string("voice.env") {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                file_vals.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        println!("loaded voice.env ({} keys)", file_vals.len());
    }
    // 映像テストモードかどうか (Annex-B 生 H264)
    let video_mode = matches!(ext.as_deref(), Some("h264") | Some("264"));
    let cfg = VoiceConfig {
        guild_id: pick(&file_vals, "GUILD_ID")?.parse()?,
        user_id: pick(&file_vals, "USER_ID")?.parse()?,
        session_id: pick(&file_vals, "SESSION_ID")?,
        token: pick(&file_vals, "VOICE_TOKEN")?,
        endpoint: pick(&file_vals, "VOICE_ENDPOINT")?,
        video: video_mode,
    };

    // ---- モード(3): 映像テスト送出（実験的）----
    if video_mode {
        let fps: u32 = std::env::var("VIDEO_FPS").ok().and_then(|v| v.parse().ok()).unwrap_or(30);

        // Annex-B を NAL 列に分割し、アクセスユニット (フレーム) にグループ化する。
        // ヒューリスティック: VCL NAL (type 1/5) でフレーム終端とみなす
        // (x264 の既定 = 1 スライス/フレーム前提。SPS/PPS/SEI は次のフレームに前置)。
        let nals = split_annex_b(&data);
        let mut frames: Vec<Vec<Vec<u8>>> = Vec::new();
        let mut current: Vec<Vec<u8>> = Vec::new();
        for nal in nals {
            let ty = nal.first().map(|b| b & 0x1F).unwrap_or(0);
            current.push(nal.to_vec());
            if ty == 1 || ty == 5 {
                frames.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            frames.push(current);
        }
        if frames.is_empty() {
            anyhow::bail!("no H264 frames found (Annex-B 形式の .h264 を渡してください)");
        }

        println!("connecting to voice endpoint {} (video mode) ...", cfg.endpoint);
        let conn = VoiceConnection::connect(cfg).await?;
        let video_tx =
            conn.video_sender().ok_or_else(|| anyhow::anyhow!("video sender unavailable"))?;

        let total = frames.len();
        println!(
            "sending {total} video frames at {fps}fps (~{:.1}s) ...",
            total as f64 / fps as f64
        );
        let step = 90_000 / fps.max(1);
        let mut ts: u32 = 0;
        for f in frames {
            video_tx
                .send(VideoFrame { nals: f, timestamp_90k: ts })
                .await
                .map_err(|_| anyhow::anyhow!("video task closed"))?;
            ts = ts.wrapping_add(step);
        }
        // 送出タスクがキュー (最大 120 フレーム ≈ 4 秒) を掃くのを待つ
        tokio::time::sleep(Duration::from_secs(6)).await;
        conn.disconnect();
        println!("done. (Discord 上で Bot のタイルに映像が出ていれば成功)");
        return Ok(());
    }

    println!("decoding {path} ...");
    let mut pipeline = AudioPipeline::new(&Filters::default())?;
    let frames = pipeline.decode_to_opus(data, ext.as_deref())?;
    println!("decoded {} opus frames (~{:.1}s)", frames.len(), frames.len() as f64 * 0.02);
    if frames.is_empty() {
        anyhow::bail!("no audio decoded (unsupported codec?)");
    }

    println!("connecting to voice endpoint {} ...", cfg.endpoint);
    let conn = VoiceConnection::connect(cfg).await?;
    println!("streaming... (plays in real time)");
    for frame in frames {
        conn.send_opus_frame(frame).await?;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    conn.disconnect();
    println!("done.");
    Ok(())
}
