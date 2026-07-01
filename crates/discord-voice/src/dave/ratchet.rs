//! 送信者鍵ラチェット（MLS sender ratchet 準拠, HKDF-SHA256 / ExpandWithLabel）。
//!
//! base secret = `MLS-Exporter("Discord Secure Frames v0", littleEndian(sender_id), 16)`（MLS 層が供給）。
//! key(generation) = MLS の DeriveTreeSecret を用いてラチェット前進して導出する。
//!
//! 注意: Whitepaper は「MLS の AEAD sender ratchet と *similarly*」とのみ記述。本実装は
//! RFC 9420 §9 の ExpandWithLabel/DeriveTreeSecret に忠実だが、secret 長などの細部は
//! libdave での最終確認が必要（フェーズ3-5c でクロスチェック）。

use std::collections::HashMap;

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// HKDF-Expand (RFC 5869)。PRK は任意長を許容（MLS の secret は短い場合がある）。
fn hkdf_expand(prk: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let mut okm = Vec::with_capacity(len);
    let mut t: Vec<u8> = Vec::new();
    let mut counter: u8 = 1;
    while okm.len() < len {
        let mut mac = HmacSha256::new_from_slice(prk).expect("hmac accepts any key length");
        mac.update(&t);
        mac.update(info);
        mac.update(&[counter]);
        t = mac.finalize().into_bytes().to_vec();
        okm.extend_from_slice(&t);
        counter = counter.wrapping_add(1);
    }
    okm.truncate(len);
    okm
}

/// MLS 可変長ベクタの長さプレフィックス（RFC 9420 §2.1.2）。
fn mls_varint(n: u64) -> Vec<u8> {
    if n < 0x40 {
        vec![n as u8]
    } else if n < 0x4000 {
        ((n as u16) | 0x4000).to_be_bytes().to_vec()
    } else if n < 0x4000_0000 {
        ((n as u32) | 0x8000_0000).to_be_bytes().to_vec()
    } else {
        (n | 0xC000_0000_0000_0000).to_be_bytes().to_vec()
    }
}

/// MLS ExpandWithLabel（RFC 9420 §8）。label には "MLS 1.0 " が前置される。
fn expand_with_label(secret: &[u8], label: &str, context: &[u8], len: usize) -> Vec<u8> {
    let full_label = format!("MLS 1.0 {label}");
    let mut info = Vec::new();
    info.extend_from_slice(&(len as u16).to_be_bytes());
    info.extend_from_slice(&mls_varint(full_label.len() as u64));
    info.extend_from_slice(full_label.as_bytes());
    info.extend_from_slice(&mls_varint(context.len() as u64));
    info.extend_from_slice(context);
    hkdf_expand(secret, &info, len)
}

/// MLS DeriveTreeSecret（RFC 9420 §9）。context は generation の uint32(BE)。
fn derive_tree_secret(secret: &[u8], label: &str, generation: u32, len: usize) -> Vec<u8> {
    expand_with_label(secret, label, &generation.to_be_bytes(), len)
}

/// 単一送信者・単一 epoch の鍵ラチェット。generation ごとに 16byte 鍵を返す。
pub struct KeyRatchet {
    base: [u8; 16],
    cache: HashMap<u32, [u8; 16]>,
}

impl KeyRatchet {
    pub fn new(base_secret: [u8; 16]) -> Self {
        Self { base: base_secret, cache: HashMap::new() }
    }

    /// generation N の AES-128 鍵を返す（キャッシュ付き）。
    pub fn key(&mut self, generation: u32) -> [u8; 16] {
        if let Some(k) = self.cache.get(&generation) {
            return *k;
        }
        // base から generation までラチェット前進。
        // secret 長は mlspp HashRatchet と同じく KDF.Nh(SHA256)=32。key は AES-128=16。
        let mut secret = self.base.to_vec();
        for g in 0..generation {
            secret = derive_tree_secret(&secret, "secret", g, 32);
        }
        let key_vec = derive_tree_secret(&secret, "key", generation, 16);
        let mut key = [0u8; 16];
        key.copy_from_slice(&key_vec);
        self.cache.insert(generation, key);
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hkdf_expand_length_and_determinism() {
        let a = hkdf_expand(b"prk-short", b"info", 16);
        let b = hkdf_expand(b"prk-short", b"info", 16);
        assert_eq!(a.len(), 16);
        assert_eq!(a, b);
        // 40 バイト（複数ブロック）でも長さ通り
        assert_eq!(hkdf_expand(b"prk", b"info", 40).len(), 40);
    }

    #[test]
    fn ratchet_is_deterministic_and_distinct() {
        let base = [7u8; 16];
        let mut r1 = KeyRatchet::new(base);
        let mut r2 = KeyRatchet::new(base);
        // 同じ base/generation は一致
        assert_eq!(r1.key(0), r2.key(0));
        assert_eq!(r1.key(5), r2.key(5));
        // generation が違えば鍵も異なる
        assert_ne!(r1.key(0), r1.key(1));
        assert_ne!(r1.key(1), r1.key(2));
    }

    #[test]
    fn different_base_different_keys() {
        let mut a = KeyRatchet::new([1u8; 16]);
        let mut b = KeyRatchet::new([2u8; 16]);
        assert_ne!(a.key(0), b.key(0));
    }
}
