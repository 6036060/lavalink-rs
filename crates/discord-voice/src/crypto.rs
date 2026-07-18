//! トランスポート暗号（Discord SFU との間）。
//! 必須: `aead_xchacha20_poly1305_rtpsize` / 優先: `aead_aes256_gcm_rtpsize`。
//!
//! どちらの `_rtpsize` AEAD モードも:
//! - 4 バイトのインクリメンタル nonce を cipher nonce の先頭に書き（残りは 0）、
//! - 同じ 4 バイトをパケット末尾に付加する、
//! - AAD は RTP ヘッダ（本実装では先頭 12 バイト）。

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

pub const REQUIRED_MODE: &str = "aead_xchacha20_poly1305_rtpsize";
pub const PREFERRED_MODE: &str = "aead_aes256_gcm_rtpsize";

#[derive(Debug, Clone, Copy)]
pub enum Mode {
    Aes256Gcm,
    XChaCha20Poly1305,
}

/// Discord が `Ready` で提示した modes から、優先順に対応モードを選ぶ。
pub fn select_mode(modes: &[String]) -> Option<(&'static str, Mode)> {
    if modes.iter().any(|m| m == PREFERRED_MODE) {
        Some((PREFERRED_MODE, Mode::Aes256Gcm))
    } else if modes.iter().any(|m| m == REQUIRED_MODE) {
        Some((REQUIRED_MODE, Mode::XChaCha20Poly1305))
    } else {
        None
    }
}

pub enum Cipher {
    Gcm(Box<Aes256Gcm>),
    XCha(Box<XChaCha20Poly1305>),
}

impl Cipher {
    pub fn new(mode: Mode, key: &[u8; 32]) -> Self {
        match mode {
            Mode::Aes256Gcm => {
                Cipher::Gcm(Box::new(Aes256Gcm::new_from_slice(key).expect("32-byte key")))
            }
            Mode::XChaCha20Poly1305 => {
                Cipher::XCha(Box::new(XChaCha20Poly1305::new_from_slice(key).expect("32-byte key")))
            }
        }
    }

    /// 平文を暗号化して `ciphertext||tag` を返す（末尾 nonce はパケット側で付加）。
    pub fn encrypt(&self, counter: u32, aad: &[u8], plaintext: &[u8]) -> Option<Vec<u8>> {
        let payload = Payload { msg: plaintext, aad };
        match self {
            Cipher::Gcm(c) => {
                let mut nonce = [0u8; 12];
                nonce[0..4].copy_from_slice(&counter.to_be_bytes());
                c.encrypt(Nonce::from_slice(&nonce), payload).ok()
            }
            Cipher::XCha(c) => {
                let mut nonce = [0u8; 24];
                nonce[0..4].copy_from_slice(&counter.to_be_bytes());
                c.encrypt(XNonce::from_slice(&nonce), payload).ok()
            }
        }
    }

    /// `ciphertext||tag` を復号する（encrypt の逆。テスト・受信経路用）。
    pub fn decrypt(&self, counter: u32, aad: &[u8], ciphertext: &[u8]) -> Option<Vec<u8>> {
        let payload = Payload { msg: ciphertext, aad };
        match self {
            Cipher::Gcm(c) => {
                let mut nonce = [0u8; 12];
                nonce[0..4].copy_from_slice(&counter.to_be_bytes());
                c.decrypt(Nonce::from_slice(&nonce), payload).ok()
            }
            Cipher::XCha(c) => {
                let mut nonce = [0u8; 24];
                nonce[0..4].copy_from_slice(&counter.to_be_bytes());
                c.decrypt(XNonce::from_slice(&nonce), payload).ok()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_prefers_aes_then_xchacha() {
        let modes = vec![REQUIRED_MODE.to_string(), PREFERRED_MODE.to_string()];
        assert!(matches!(select_mode(&modes), Some((PREFERRED_MODE, Mode::Aes256Gcm))));
        let only_x = vec![REQUIRED_MODE.to_string()];
        assert!(matches!(select_mode(&only_x), Some((REQUIRED_MODE, Mode::XChaCha20Poly1305))));
        assert!(select_mode(&[]).is_none());
    }

    #[test]
    fn xchacha_round_trip() {
        let key = [7u8; 32];
        let aad = [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        let cipher = Cipher::new(Mode::XChaCha20Poly1305, &key);
        let ct = cipher.encrypt(42, &aad, b"opus-frame").unwrap();
        assert_eq!(ct.len(), b"opus-frame".len() + 16); // +16 byte tag

        let dec = XChaCha20Poly1305::new_from_slice(&key).unwrap();
        let mut nonce = [0u8; 24];
        nonce[0..4].copy_from_slice(&42u32.to_be_bytes());
        let pt = dec
            .decrypt(XNonce::from_slice(&nonce), Payload { msg: &ct, aad: &aad })
            .unwrap();
        assert_eq!(pt, b"opus-frame");
    }

    #[test]
    fn aes_gcm_round_trip() {
        let key = [3u8; 32];
        let aad = [9u8; 12];
        let cipher = Cipher::new(Mode::Aes256Gcm, &key);
        let ct = cipher.encrypt(1, &aad, b"hello").unwrap();
        let dec = Aes256Gcm::new_from_slice(&key).unwrap();
        let mut nonce = [0u8; 12];
        nonce[0..4].copy_from_slice(&1u32.to_be_bytes());
        let pt = dec
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: &aad })
            .unwrap();
        assert_eq!(pt, b"hello");
    }
}
