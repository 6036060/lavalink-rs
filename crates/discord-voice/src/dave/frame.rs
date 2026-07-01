//! DAVE フレーム E2EE（AES-128-GCM, 8byte 切詰めタグ）。OPUS は全暗号・AAD 空。
//!
//! フレーム形式（Whitepaper Payload Format）:
//!   [暗号文(=interleaved frame)] [8B tag] [ULEB128 nonce] [未暗号range(OPUSは空)]
//!   [1B supplemental size] [0xFAFA]
//!
//! nonce: 32bit を 96bit へ拡張（下位4バイト=LE nonce, 上位8バイト=0）。libdave 準拠。
//! generation = nonce の最上位バイト（鍵ラチェット用）。

use super::gcm;
use super::uleb128;
use super::MAGIC_MARKER;

/// 32bit truncated nonce → 96bit フル nonce（下位4バイトに BE で配置）。
pub fn full_nonce(truncated: u32) -> [u8; 12] {
    let mut n = [0u8; 12];
    // libdave は truncated nonce を fullNonce[8..12] にリトルエンディアンで書く
    // （C++ の memcpy(&iv[8], &nonce_u32, 4) 相当）。
    n[8..12].copy_from_slice(&truncated.to_le_bytes());
    n
}

/// nonce の最上位バイト = 鍵ラチェットの generation。
pub fn generation(truncated: u32) -> u8 {
    (truncated >> 24) as u8
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    Crypto,
    NotProtocolFrame,
    Truncated,
}

/// OPUS フレームを E2EE プロトコルフレームへ変換する。
pub fn encrypt_opus(key: &[u8; 16], nonce: u32, opus: &[u8]) -> Result<Vec<u8>, FrameError> {
    let full = full_nonce(nonce);
    // OPUS は全暗号・追加認証データ無し。
    let (ct, tag) = gcm::encrypt_detached(key, &full, &[], opus);

    let nonce_uleb = uleb128::encode(nonce as u64);
    // supplemental size = tag(8) + nonce + ranges(0) + size(1) + magic(2)
    let suppl_size = 8 + nonce_uleb.len() + 1 + 2;

    let mut out = Vec::with_capacity(ct.len() + 8 + nonce_uleb.len() + 3);
    out.extend_from_slice(&ct);
    out.extend_from_slice(&tag[..8]); // 8byte 切詰めタグ
    out.extend_from_slice(&nonce_uleb);
    // OPUS: 未暗号 range は無し
    out.push(suppl_size as u8);
    out.extend_from_slice(&MAGIC_MARKER);
    Ok(out)
}

/// E2EE プロトコルフレーム（OPUS, 未暗号 range 無し）を復号する。
pub fn decrypt_opus(key: &[u8; 16], frame: &[u8]) -> Result<Vec<u8>, FrameError> {
    let l = frame.len();
    if l < 3 + 8 {
        return Err(FrameError::Truncated);
    }
    if frame[l - 2..] != MAGIC_MARKER {
        return Err(FrameError::NotProtocolFrame);
    }
    let suppl_size = frame[l - 3] as usize;
    if suppl_size < 11 || suppl_size > l {
        return Err(FrameError::NotProtocolFrame);
    }
    let suppl_start = l - suppl_size;
    // [tag8][nonce uleb][ranges(0)][size 1][magic 2]
    let tag8 = &frame[suppl_start..suppl_start + 8];
    let nonce_region = &frame[suppl_start + 8..l - 3];
    let (nonce_val, _n) = uleb128::decode(nonce_region).ok_or(FrameError::NotProtocolFrame)?;

    let ct = &frame[..suppl_start];
    let full = full_nonce(nonce_val as u32);
    gcm::decrypt_trunc8(key, &full, &[], ct, tag8).ok_or(FrameError::Crypto)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_expansion_and_generation() {
        let n = full_nonce(0x0102_0304);
        assert_eq!(n, [0, 0, 0, 0, 0, 0, 0, 0, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(generation(0xAB12_3456), 0xAB);
    }

    #[test]
    fn opus_frame_round_trip() {
        let key = [0x11u8; 16];
        let opus = b"the quick brown fox (opus payload)".to_vec();
        let frame = encrypt_opus(&key, 0x0000_2A01, &opus).unwrap();
        assert_eq!(&frame[frame.len() - 2..], &MAGIC_MARKER);
        assert_eq!(decrypt_opus(&key, &frame).unwrap(), opus);
    }

    #[test]
    fn tampered_frame_fails() {
        let key = [0x22u8; 16];
        let mut frame = encrypt_opus(&key, 5, b"hello").unwrap();
        frame[0] ^= 0xFF;
        assert!(decrypt_opus(&key, &frame).is_err());
    }

    #[test]
    fn wrong_key_fails() {
        let frame = encrypt_opus(&[1u8; 16], 7, b"secret").unwrap();
        assert!(decrypt_opus(&[2u8; 16], &frame).is_err());
    }
}
