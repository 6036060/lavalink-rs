//! Voice Gateway の DAVE バイナリメッセージ フレーミングと opcode 定義。
//!
//! バイナリメッセージ形式:
//!   [Sequence Number u16 BE (server->client のみ)] [Opcode u8] [Payload ...]

/// DAVE プロトコル opcode（21-31）。
pub mod op {
    pub const PREPARE_TRANSITION: u8 = 21;
    pub const EXECUTE_TRANSITION: u8 = 22;
    pub const TRANSITION_READY: u8 = 23;
    pub const PREPARE_EPOCH: u8 = 24;
    pub const MLS_EXTERNAL_SENDER: u8 = 25;
    pub const MLS_KEY_PACKAGE: u8 = 26;
    pub const MLS_PROPOSALS: u8 = 27;
    pub const MLS_COMMIT_WELCOME: u8 = 28;
    pub const MLS_ANNOUNCE_COMMIT_TRANSITION: u8 = 29;
    pub const MLS_WELCOME: u8 = 30;
    pub const MLS_INVALID_COMMIT_WELCOME: u8 = 31;
}

/// サーバー→クライアントのバイナリメッセージを解析する。
/// 戻り値: (sequence, opcode, payload)。
pub fn parse_server_binary(data: &[u8]) -> Option<(u16, u8, &[u8])> {
    if data.len() < 3 {
        return None;
    }
    let seq = u16::from_be_bytes([data[0], data[1]]);
    let opcode = data[2];
    Some((seq, opcode, &data[3..]))
}

/// クライアント→サーバーのバイナリメッセージを組み立てる（シーケンス番号なし）。
pub fn build_client_binary(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + payload.len());
    out.push(opcode);
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_build() {
        let msg = [0x00, 0x0A, op::MLS_WELCOME, 1, 2, 3];
        let (seq, opcode, payload) = parse_server_binary(&msg).unwrap();
        assert_eq!(seq, 10);
        assert_eq!(opcode, 30);
        assert_eq!(payload, &[1, 2, 3]);

        let built = build_client_binary(op::MLS_KEY_PACKAGE, &[9, 9]);
        assert_eq!(built, vec![26, 9, 9]);
    }
}
