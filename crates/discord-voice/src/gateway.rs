//! Voice Gateway（WSS, v8）の opcode とペイロードビルダー。

use serde_json::{json, Value};

#[allow(dead_code)] // 完全な opcode 参照表として全定義を保持
pub mod op {
    pub const IDENTIFY: u64 = 0;
    pub const SELECT_PROTOCOL: u64 = 1;
    pub const READY: u64 = 2;
    pub const HEARTBEAT: u64 = 3;
    pub const SESSION_DESCRIPTION: u64 = 4;
    pub const SPEAKING: u64 = 5;
    pub const HEARTBEAT_ACK: u64 = 6;
    pub const RESUME: u64 = 7;
    pub const HELLO: u64 = 8;
    pub const RESUMED: u64 = 9;
    pub const VIDEO: u64 = 12;
    pub const CLIENT_DISCONNECT: u64 = 13;
}

/// op 0 Identify。`max_dave_protocol_version=0` で DAVE 非対応を明示（ADR-0001）。
pub fn identify(guild_id: u64, user_id: u64, session_id: &str, token: &str, max_dave: u8) -> Value {
    json!({
        "op": op::IDENTIFY,
        "d": {
            "server_id": guild_id.to_string(),
            "user_id": user_id.to_string(),
            "session_id": session_id,
            "token": token,
            "max_dave_protocol_version": max_dave
        }
    })
}

/// op 1 Select Protocol。IP Discovery で得た外部 IP/port と選択した暗号モードを通知。
pub fn select_protocol(address: &str, port: u16, mode: &str) -> Value {
    json!({
        "op": op::SELECT_PROTOCOL,
        "d": { "protocol": "udp", "data": { "address": address, "port": port, "mode": mode } }
    })
}

/// op 3 Heartbeat（v8 は `seq_ack` 必須。未受信なら省略）。
pub fn heartbeat(nonce: u64, seq_ack: i64) -> Value {
    let d = if seq_ack >= 0 {
        json!({ "t": nonce, "seq_ack": seq_ack })
    } else {
        json!({ "t": nonce })
    };
    json!({ "op": op::HEARTBEAT, "d": d })
}

/// op 5 Speaking（音声送出前に最低 1 回。microphone ビット）。
pub fn speaking(ssrc: u32) -> Value {
    json!({ "op": op::SPEAKING, "d": { "speaking": 1, "delay": 0, "ssrc": ssrc } })
}

// ----------------------------- 映像 (実験的) -----------------------------
//
// ⚠️ ボットの映像送信は Discord 公式には文書化されていない。以下のペイロード形状は
// コミュニティのリバースエンジニアリング (Discord-video-stream 等) と公式クライアントの
// キャプチャに基づくもので、結線前に実キャプチャでの検証が必要 (docs/video-streaming-plan.md)。

/// op 1 Select Protocol の映像対応版。`codecs` で音声 (opus) + 映像 (H264) を宣言する。
/// payload_type は Discord クライアントの慣例値 (opus=120, H264=101, rtx=102)。
pub fn select_protocol_with_codecs(address: &str, port: u16, mode: &str) -> Value {
    json!({
        "op": op::SELECT_PROTOCOL,
        "d": {
            "protocol": "udp",
            "data": { "address": address, "port": port, "mode": mode },
            "codecs": [
                { "name": "opus", "type": "audio", "priority": 1000, "payload_type": 120 },
                {
                    "name": "H264",
                    "type": "video",
                    "priority": 1000,
                    "payload_type": 101,
                    "rtx_payload_type": 102,
                    "encode": true,
                    "decode": true
                }
            ]
        }
    })
}

/// op 12 Video。映像ストリームの SSRC を宣言する (送出開始前に 1 回、停止時は
/// `video_ssrc=0` で送る)。`audio_ssrc` は READY で得た自身の SSRC。
pub fn video(
    audio_ssrc: u32,
    video_ssrc: u32,
    rtx_ssrc: u32,
    width: u32,
    height: u32,
    framerate: u32,
) -> Value {
    json!({
        "op": op::VIDEO,
        "d": {
            "audio_ssrc": audio_ssrc,
            "video_ssrc": video_ssrc,
            "rtx_ssrc": rtx_ssrc,
            "streams": [{
                "type": "video",
                "rid": "100",
                "ssrc": video_ssrc,
                "active": video_ssrc != 0,
                "quality": 100,
                "rtx_ssrc": rtx_ssrc,
                "max_bitrate": 2_500_000,
                "max_framerate": framerate,
                "max_resolution": { "type": "fixed", "width": width, "height": height }
            }]
        }
    })
}
