//! 送信フレーム暗号器: ラチェット鍵 + nonce 管理で OPUS を E2EE フレーム化する。

use super::frame::{self, FrameError};
use super::ratchet::KeyRatchet;

/// 単一 epoch の送信者フレーム暗号器。
pub struct FrameCryptor {
    ratchet: KeyRatchet,
    nonce: u32,
    epoch: u64,
}

impl FrameCryptor {
    pub fn new(base_secret: [u8; 16]) -> Self {
        Self { ratchet: KeyRatchet::new(base_secret), nonce: 0, epoch: 0 }
    }

    /// epoch を指定して生成（鍵更新の検知用）。
    pub fn with_epoch(base_secret: [u8; 16], epoch: u64) -> Self {
        Self { ratchet: KeyRatchet::new(base_secret), nonce: 0, epoch }
    }

    /// この cryptor が属する MLS epoch。
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// epoch 変更時: 新しい base secret でリセット（nonce=0, generation=0）。
    pub fn reset(&mut self, base_secret: [u8; 16]) {
        self.ratchet = KeyRatchet::new(base_secret);
        self.nonce = 0;
    }

    /// 次の OPUS フレームを暗号化する。generation は nonce 最上位バイト。
    pub fn encrypt(&mut self, opus: &[u8]) -> Result<Vec<u8>, FrameError> {
        let generation = frame::generation(self.nonce) as u32;
        let key = self.ratchet.key(generation);
        let out = frame::encrypt_opus(&key, self.nonce, opus)?;
        self.nonce = self.nonce.wrapping_add(1);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dave::ratchet::KeyRatchet;

    #[test]
    fn encrypts_decryptable_frames() {
        let base = [9u8; 16];
        let mut cryptor = FrameCryptor::new(base);
        let f0 = cryptor.encrypt(b"frame zero").unwrap();
        let f1 = cryptor.encrypt(b"frame one").unwrap();
        assert_ne!(f0, f1);

        // generation 0 の鍵で復号できる（nonce 0,1 とも gen=0）。
        let mut r = KeyRatchet::new(base);
        let key0 = r.key(0);
        assert_eq!(frame::decrypt_opus(&key0, &f0).unwrap(), b"frame zero");
        assert_eq!(frame::decrypt_opus(&key0, &f1).unwrap(), b"frame one");
    }
}
