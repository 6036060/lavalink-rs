//! DAVE（音声 E2EE）プロトコル土台（フェーズ3-5b, feature = "dave"）。
//!
//! 一次資料: Discord DAVE Protocol Whitepaper v1.1 (daveprotocol.com), Discord Voice docs。
//! 本モジュールは「フレームレベル E2EE」の検証可能な純ロジックを実装する:
//! - [`uleb128`] ULEB128 可変長エンコード
//! - [`frame`]   AES-128-GCM フレーム暗号 + フッタ組立（8byte tag/ULEB128 nonce/0xFAFA）
//! - [`ratchet`] 送信者鍵ラチェット（MLS ExpandWithLabel/HKDF-SHA256）
//! - [`opcodes`] Voice Gateway バイナリ opcode フレーミング(21-31)
//!
//! MLS グループ管理（key package/Welcome/exporter secret）は [`GroupKeySource`] で抽象化し、
//! 実装(openmls)はフェーズ3-5c（feature = "dave-mls", 実機テスト要）。

pub mod cryptor;
pub mod frame;
mod gcm;
#[cfg(feature = "dave-mls")]
pub mod mls;
pub mod opcodes;
pub mod ratchet;
pub mod session;
pub mod uleb128;
pub mod video_frame;

/// 本実装が対応する DAVE プロトコルバージョン。
pub const PROTOCOL_VERSION: u16 = 1;
/// MLS 暗号スイート: MLS_128_DHKEMP256_AES128GCM_SHA256_P256（署名 ECDSA P256）。
pub const MLS_CIPHERSUITE: u16 = 0x0002;
/// 送信者鍵 base secret 用の MLS-Exporter ラベル。
pub const FRAMES_EXPORTER_LABEL: &str = "Discord Secure Frames v0";
/// フレーム末尾のマジックマーカー。
pub const MAGIC_MARKER: [u8; 2] = [0xFA, 0xFA];

/// MLS グループ層（3-5c）が提供すべきインターフェース。
/// frame/ratchet 層はこれ経由で鍵素材を得るため、openmls 無しでもテスト可能。
pub trait GroupKeySource {
    /// `MLS-Exporter("Discord Secure Frames v0", littleEndian(sender_id), 16)`。
    /// epoch が変わるたびに別の値になる。
    fn sender_base_secret(&self, sender_id: u64) -> Option<[u8; 16]>;
    /// 現在の MLS epoch。
    fn epoch(&self) -> u64;
}
