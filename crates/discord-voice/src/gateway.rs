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
    pub const CLIENT_DISCONNECT: u64 = 13;
}

/// op 0 Identify。`max_dave_protocol_version=0` で DAVE 非対応を明示（ADR-0001）。
pub fn identify(
    guild_id: u64,
    user_id: u64,
    session_id: &str,
    token: &str,
    max_dave: u8,
) -> Value {
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
