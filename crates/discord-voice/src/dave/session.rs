//! DAVE プロトコル状態機械（フェーズ3-5）。
//!
//! opcode の表現（whitepaper 準拠, 重要）:
//! - JSON テキスト: 21 prepare_transition / 22 execute_transition / 24 prepare_epoch /
//!   （送信）23 ready_for_transition / 31 invalid_commit_welcome
//! - バイナリ([u16 seq][u8 op][payload] 受信 / [u8 op][payload] 送信):
//!   25 external_sender / 27 proposals / 29 announce_commit / 30 welcome /
//!   （送信）26 key_package / 28 commit_welcome
//! - 29/30 の payload 先頭は `uint16 transition_id`、27 の先頭は `operation_type(u8)`。
//!
//! MLS 操作は [`MlsBackend`] に委譲（フェーズB=NoopMls / フェーズA=OpenMlsBackend）。
//! ⚠️ 実 Discord とのバイト整合は実機検証が必須。本実装はログ付きで反復前提。

use serde_json::{json, Value};

/// MLS グループ操作のインターフェース。
pub trait MlsBackend {
    /// op25: external sender（ExternalSender 構造のバイト列）を受領。
    fn set_external_sender(&mut self, payload: &[u8]);
    /// op24 epoch=1: ローカルグループを作成し、自分の key package(MLSMessage バイト)を返す。
    fn create_group(&mut self) -> Option<Vec<u8>>;
    /// op27: proposals を処理し、op28 payload（commit MLSMessage [+ welcome]）を返す。
    fn handle_proposals(&mut self, payload: &[u8]) -> Option<Vec<u8>>;
    /// op29: 確定 commit(MLSMessage バイト)を適用して次 epoch へ。
    fn apply_commit(&mut self, commit: &[u8]) -> bool;
    /// op30: welcome(Welcome バイト)でグループ参加。
    fn join_welcome(&mut self, welcome: &[u8]) -> bool;
    /// FrameCryptor 用の送信者 base secret（exporter）。
    fn sender_base_secret(&self, sender_id: u64) -> Option<[u8; 16]>;
    fn epoch(&self) -> u64;
    /// op26 用: 自分の KeyPackage(MLSMessage バイト)。送らないバックエンドは None。
    fn key_package(&mut self) -> Option<Vec<u8>> {
        None
    }
}

/// 何もしないスタブ（DAVE 非対応ビルド/疎通用）。
pub struct NoopMls;
impl MlsBackend for NoopMls {
    fn set_external_sender(&mut self, _: &[u8]) {}
    fn create_group(&mut self) -> Option<Vec<u8>> {
        None
    }
    fn handle_proposals(&mut self, _: &[u8]) -> Option<Vec<u8>> {
        None
    }
    fn apply_commit(&mut self, _: &[u8]) -> bool {
        false
    }
    fn join_welcome(&mut self, _: &[u8]) -> bool {
        false
    }
    fn sender_base_secret(&self, _: u64) -> Option<[u8; 16]> {
        None
    }
    fn epoch(&self) -> u64 {
        0
    }
}

/// クライアント→サーバーへ送る DAVE メッセージ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaveOut {
    /// テキスト JSON メッセージ（op 23 / 31）。
    Json(String),
    /// バイナリメッセージ（先頭が opcode の生バイト, op 26 / 28）。
    Binary(Vec<u8>),
}

pub struct DaveSession<M: MlsBackend> {
    user_id: u64,
    protocol_version: u16,
    mls: M,
}

impl<M: MlsBackend> DaveSession<M> {
    pub fn new(user_id: u64, protocol_version: u16, mls: M) -> Self {
        Self { user_id, protocol_version, mls }
    }

    pub fn user_id(&self) -> u64 {
        self.user_id
    }
    pub fn protocol_version(&self) -> u16 {
        self.protocol_version
    }

    /// グループが確立し送信者鍵が導出できるなら返す（FrameCryptor 更新用）。
    pub fn sender_secret(&self) -> Option<[u8; 16]> {
        self.mls.sender_base_secret(self.user_id)
    }

    /// 現在の MLS epoch（鍵更新の検知用）。
    pub fn epoch(&self) -> u64 {
        self.mls.epoch()
    }

    /// op26 用の KeyPackage(MLSMessage バイト)を生成する。
    pub fn key_package(&mut self) -> Option<Vec<u8>> {
        self.mls.key_package()
    }

    fn ready(tid: i64) -> DaveOut {
        DaveOut::Json(json!({ "op": 23, "d": { "transition_id": tid } }).to_string())
    }
    fn invalid(tid: i64) -> DaveOut {
        DaveOut::Json(json!({ "op": 31, "d": { "transition_id": tid } }).to_string())
    }
    fn binary(opcode: u8, payload: Vec<u8>) -> DaveOut {
        let mut v = Vec::with_capacity(1 + payload.len());
        v.push(opcode);
        v.extend_from_slice(&payload);
        DaveOut::Binary(v)
    }

    /// JSON テキスト opcode（21 / 22 / 24）の処理。
    pub fn handle_json(&mut self, op: u64, d: &Value) -> Vec<DaveOut> {
        let tid = d.get("transition_id").and_then(Value::as_i64).unwrap_or(0);
        match op {
            21 => {
                // prepare_transition: 準備して ready を返す（tid=0 は (再)初期化）。
                vec![Self::ready(tid)]
            }
            22 => {
                // execute_transition: 実行確定。状態反映のみ。
                if let Some(v) = d.get("protocol_version").and_then(Value::as_u64) {
                    self.protocol_version = v as u16;
                }
                Vec::new()
            }
            24 => {
                // prepare_epoch: epoch=1 で新グループ作成 → key package(26) を送る。
                let epoch = d.get("epoch").and_then(Value::as_u64).unwrap_or(0);
                if epoch >= 1 {
                    if let Some(kp) = self.mls.create_group() {
                        return vec![Self::binary(26, kp)];
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// バイナリ opcode（25 / 27 / 29 / 30）の処理。payload は seq/opcode を除いた本体。
    pub fn handle_binary(&mut self, opcode: u8, payload: &[u8]) -> Vec<DaveOut> {
        match opcode {
            25 => {
                self.mls.set_external_sender(payload);
                Vec::new()
            }
            27 => match self.mls.handle_proposals(payload) {
                Some(commit_welcome) => vec![Self::binary(28, commit_welcome)],
                None => Vec::new(),
            },
            29 => {
                let (tid, commit) = split_transition(payload);
                if self.mls.apply_commit(commit) {
                    vec![Self::ready(tid)]
                } else {
                    vec![Self::invalid(tid)]
                }
            }
            30 => {
                let (tid, welcome) = split_transition(payload);
                if self.mls.join_welcome(welcome) {
                    vec![Self::ready(tid)]
                } else {
                    vec![Self::invalid(tid)]
                }
            }
            _ => Vec::new(),
        }
    }
}

/// 先頭 `uint16 transition_id`(BE) と残りの MLS バイトに分割する。
fn split_transition(payload: &[u8]) -> (i64, &[u8]) {
    if payload.len() >= 2 {
        let tid = u16::from_be_bytes([payload[0], payload[1]]) as i64;
        (tid, &payload[2..])
    } else {
        (0, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct OkMls {
        joined: bool,
    }
    impl MlsBackend for OkMls {
        fn set_external_sender(&mut self, _: &[u8]) {}
        fn create_group(&mut self) -> Option<Vec<u8>> {
            Some(vec![0xAA, 0xBB])
        }
        fn handle_proposals(&mut self, _: &[u8]) -> Option<Vec<u8>> {
            Some(vec![1, 2, 3])
        }
        fn apply_commit(&mut self, _: &[u8]) -> bool {
            true
        }
        fn join_welcome(&mut self, _: &[u8]) -> bool {
            self.joined = true;
            true
        }
        fn sender_base_secret(&self, _: u64) -> Option<[u8; 16]> {
            self.joined.then_some([7u8; 16])
        }
        fn epoch(&self) -> u64 {
            1
        }
    }

    #[test]
    fn prepare_epoch_creates_group_and_sends_key_package() {
        let mut s = DaveSession::new(1, 1, OkMls { joined: false });
        let out = s.handle_json(24, &json!({ "epoch": 1, "protocol_version": 1 }));
        assert_eq!(out, vec![DaveOut::Binary(vec![26, 0xAA, 0xBB])]);
    }

    #[test]
    fn prepare_transition_replies_ready_json() {
        let mut s = DaveSession::new(1, 1, OkMls { joined: false });
        let out = s.handle_json(21, &json!({ "transition_id": 10 }));
        assert_eq!(out.len(), 1);
        match &out[0] {
            DaveOut::Json(t) => {
                assert!(t.contains("\"op\":23"));
                assert!(t.contains("\"transition_id\":10"));
            }
            _ => panic!("expected json"),
        }
    }

    #[test]
    fn proposals_emit_commit_welcome_binary() {
        let mut s = DaveSession::new(1, 1, OkMls { joined: false });
        // op27 payload: [operation_type=0(append)] + proposals...
        let out = s.handle_binary(27, &[0x00, 0x09]);
        assert_eq!(out, vec![DaveOut::Binary(vec![28, 1, 2, 3])]);
    }

    #[test]
    fn welcome_joins_and_replies_ready_then_key_available() {
        let mut s = DaveSession::new(1, 1, OkMls { joined: false });
        // op30 payload: [u16 tid=5][welcome bytes]
        let out = s.handle_binary(30, &[0x00, 0x05, 0xDE, 0xAD]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], DaveOut::Json(t) if t.contains("\"transition_id\":5")));
        assert_eq!(s.sender_secret(), Some([7u8; 16]));
    }

    #[test]
    fn external_sender_no_reply() {
        let mut s = DaveSession::new(1, 1, NoopMls);
        assert!(s.handle_binary(25, &[1, 2, 3]).is_empty());
    }
}
