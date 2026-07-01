//! AES-128-GCM（自前実装）。aes-gcm crate はタグ長 12-16 のみ許容のため、
//! DAVE の 8byte 切詰めタグに対応すべく aes + ghash で GCM を構成する。
//! 96bit nonce 前提（J0 = nonce || 0x00000001）。

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};
use aes::Aes128;
use ghash::universal_hash::UniversalHash;
use ghash::GHash;

fn aes_block(aes: &Aes128, input: &[u8; 16]) -> [u8; 16] {
    let mut blk = *GenericArray::from_slice(input);
    aes.encrypt_block(&mut blk);
    let mut out = [0u8; 16];
    out.copy_from_slice(&blk);
    out
}

fn inc32(ctr: &mut [u8; 16]) {
    let c = u32::from_be_bytes([ctr[12], ctr[13], ctr[14], ctr[15]]).wrapping_add(1);
    ctr[12..16].copy_from_slice(&c.to_be_bytes());
}

/// CTR モード XOR（鍵ストリームは inc32(J0) から）。encrypt/decrypt 共通。
fn ctr_xor(aes: &Aes128, j0: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; data.len()];
    let mut ctr = *j0;
    for (i, chunk) in data.chunks(16).enumerate() {
        inc32(&mut ctr);
        let ks = aes_block(aes, &ctr);
        for (j, b) in chunk.iter().enumerate() {
            out[i * 16 + j] = b ^ ks[j];
        }
    }
    out
}

/// GCM のフル 16byte 認証タグを計算する（AAD と暗号文に対する GHASH ⊕ E(J0)）。
fn gcm_tag(aes: &Aes128, h: &[u8; 16], j0: &[u8; 16], aad: &[u8], ct: &[u8]) -> [u8; 16] {
    let mut gh = GHash::new(GenericArray::from_slice(h));
    gh.update_padded(aad);
    gh.update_padded(ct);
    let mut len_block = [0u8; 16];
    len_block[..8].copy_from_slice(&((aad.len() as u64) * 8).to_be_bytes());
    len_block[8..].copy_from_slice(&((ct.len() as u64) * 8).to_be_bytes());
    gh.update(&[*GenericArray::from_slice(&len_block)]);
    let s = gh.finalize();

    let ej0 = aes_block(aes, j0);
    let mut tag = [0u8; 16];
    for i in 0..16 {
        tag[i] = ej0[i] ^ s[i];
    }
    tag
}

fn setup(key: &[u8; 16], nonce: &[u8; 12]) -> (Aes128, [u8; 16], [u8; 16]) {
    let aes = Aes128::new(GenericArray::from_slice(key));
    let h = aes_block(&aes, &[0u8; 16]); // ハッシュ副鍵 H = E(0^128)
    let mut j0 = [0u8; 16];
    j0[..12].copy_from_slice(nonce);
    j0[15] = 1; // 96bit nonce のとき J0 = nonce || 0^31 || 1
    (aes, h, j0)
}

/// 暗号化して (暗号文, フル16byteタグ) を返す。
pub fn encrypt_detached(key: &[u8; 16], nonce: &[u8; 12], aad: &[u8], pt: &[u8]) -> (Vec<u8>, [u8; 16]) {
    let (aes, h, j0) = setup(key, nonce);
    let ct = ctr_xor(&aes, &j0, pt);
    let tag = gcm_tag(&aes, &h, &j0, aad, &ct);
    (ct, tag)
}

/// 8byte 切詰めタグを検証して平文を返す（タグ不一致なら None）。
pub fn decrypt_trunc8(
    key: &[u8; 16],
    nonce: &[u8; 12],
    aad: &[u8],
    ct: &[u8],
    tag8: &[u8],
) -> Option<Vec<u8>> {
    if tag8.len() < 8 {
        return None;
    }
    let (aes, h, j0) = setup(key, nonce);
    let full = gcm_tag(&aes, &h, &j0, aad, ct);
    let mut diff = 0u8;
    for i in 0..8 {
        diff |= full[i] ^ tag8[i];
    }
    if diff != 0 {
        return None;
    }
    Some(ctr_xor(&aes, &j0, ct))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcm_round_trip_8byte_tag() {
        let key = [0x11u8; 16];
        let mut nonce = [0u8; 12];
        nonce[8..].copy_from_slice(&0x2A01u32.to_be_bytes());
        let pt = b"opus payload bytes that span more than one block!!";
        let (ct, tag) = encrypt_detached(&key, &nonce, &[], pt);
        let back = decrypt_trunc8(&key, &nonce, &[], &ct, &tag[..8]).unwrap();
        assert_eq!(back, pt);
        // タグ改ざんで失敗
        let mut bad = tag;
        bad[0] ^= 1;
        assert!(decrypt_trunc8(&key, &nonce, &[], &ct, &bad[..8]).is_none());
    }
}
