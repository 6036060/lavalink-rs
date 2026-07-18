//! DAVE 映像フレーム E2EE (H264)。libdave の codec_utils / encryptor 準拠。
//!
//! OPUS (frame.rs) は全暗号だが、H264 は「codec-aware 部分暗号」が必要:
//! - VCL NAL (slice=1 / IDR=5): NAL ヘッダ + スライスヘッダ先頭 (PPS ID までの
//!   exp-golomb 3 値) を平文に残し、残り (スライスデータ) を暗号化。
//! - 非 VCL NAL (SPS/PPS/SEI/AUD 等): NAL 全体を平文。
//! - 各 NAL の直前には常に 4byte スタートコード {0,0,0,1} を平文で書く (正規化)。
//!
//! フレーム形式 (protocol.md / libdave):
//!   [interleaved frame] [tag(8)] [nonce(ULEB128)] [unencrypted ranges(ULEB128 pairs)]
//!   [supplemental size(1)] [0xFAFA]
//! - 暗号化: 全暗号レンジを結合した平文を 1 ブロックとして AES-128-GCM。
//!   **AAD = 全平文レンジ (unencrypted bytes) の結合**。
//!
//! ⚠️ 未実装: エミュレーション防止スキャン (暗号文中に 00 00 01 が現れると受信側の
//! depacketizer が誤検出する)。稀な確率で 1 フレームが乱れるが致命的ではない。
//! docs/video-streaming-plan.md 参照。

use super::frame::{full_nonce, generation, FrameError};
use super::ratchet::KeyRatchet;
use super::{gcm, uleb128, MAGIC_MARKER};

const NAL_TYPE_MASK: u8 = 0x1F;
const NAL_TYPE_SLICE: u8 = 1;
const NAL_TYPE_IDR: u8 = 5;
const LONG_START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Annex-B フレームから NAL 境界を探す。返り値は各 NAL の
/// (ペイロード開始 index = スタートコード直後, スタートコード長)。
fn find_nalus(data: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            // 直前がさらに 0 なら 4byte スタートコード
            let long = i >= 1 && data[i - 1] == 0;
            let sc = if long { 4 } else { 3 };
            let start = i + 3;
            out.push((start, sc));
            i = start;
        } else {
            i += 1;
        }
    }
    out
}

/// VCL スライスヘッダのうち平文に残すバイト数 (NAL ヘッダの次から数える)。
/// exp-golomb で 3 値 (first_mb_in_slice, slice_type, pic_parameter_set_id) を
/// 読み飛ばし、その最終ビットを含むバイトまで + 1 を返す。libdave BytesCoveringH264PPS 準拠。
fn bytes_covering_h264_pps(payload: &[u8]) -> usize {
    const EMULATION_PREVENTION_BYTE: u8 = 0x03;
    let size_remaining = payload.len();
    let mut bit_index: usize = 0;
    let mut zero_bits = 0i32;
    let mut parsed = 0i32;

    while bit_index < size_remaining * 8 && parsed < 3 {
        let bit_in_byte = bit_index % 8;
        let byte_index = bit_index / 8;
        let byte = payload[byte_index];

        // エミュレーション防止バイト (00 00 03) の 03 はスキップ
        if bit_in_byte == 0
            && byte_index >= 2
            && byte == EMULATION_PREVENTION_BYTE
            && payload[byte_index - 1] == 0
            && payload[byte_index - 2] == 0
        {
            bit_index += 8;
            continue;
        }

        if byte & (1 << (7 - bit_in_byte)) == 0 {
            zero_bits += 1;
            bit_index += 1;
            if zero_bits >= 32 {
                return 0; // 異常
            }
        } else {
            parsed += 1;
            bit_index += 1 + zero_bits as usize;
            zero_bits = 0;
        }
    }
    (bit_index / 8) + 1
}

/// unencrypted ranges を末尾へ追記 (直前と連続なら結合)。
fn push_unenc(ranges: &mut Vec<(u64, u64)>, off: u64, size: u64) {
    if size == 0 {
        return;
    }
    if let Some(last) = ranges.last_mut() {
        if last.0 + last.1 == off {
            last.1 += size;
            return;
        }
    }
    ranges.push((off, size));
}

/// H264 Annex-B の 1 アクセスユニットを DAVE E2EE フレームへ変換する。
pub fn encrypt_h264(key: &[u8; 16], nonce: u32, frame: &[u8]) -> Vec<u8> {
    let nalus = find_nalus(frame);
    // 正規化フレーム (スタートコードを 4byte に統一) を組み立てつつ、
    // 平文レンジ / 暗号化位置 / AAD / 暗号化平文を収集する。
    let mut recon: Vec<u8> = Vec::with_capacity(frame.len() + 16);
    let mut unenc_ranges: Vec<(u64, u64)> = Vec::new();
    let mut enc_positions: Vec<(usize, usize)> = Vec::new(); // recon 内 (offset, size)
    let mut aad: Vec<u8> = Vec::new();
    let mut plaintext: Vec<u8> = Vec::new();

    for (idx, &(nal_start, sc)) in nalus.iter().enumerate() {
        let nal_end = nalus
            .get(idx + 1)
            .map(|&(next_start, next_sc)| next_start - next_sc)
            .unwrap_or(frame.len());
        if nal_start > nal_end {
            continue;
        }
        let nal = &frame[nal_start..nal_end];
        let nal_type = frame.get(nal_start).map(|b| b & NAL_TYPE_MASK).unwrap_or(0);

        // 常に 4byte スタートコードを平文で書く
        let sc_off = recon.len();
        recon.extend_from_slice(&LONG_START_CODE);
        push_unenc(&mut unenc_ranges, sc_off as u64, 4);
        aad.extend_from_slice(&LONG_START_CODE);
        let _ = sc;

        if nal_type == NAL_TYPE_SLICE || nal_type == NAL_TYPE_IDR {
            // NAL ヘッダ(1) + PPS ID までのスライスヘッダを平文
            let pps = bytes_covering_h264_pps(&nal[1.min(nal.len())..]);
            let header = (1 + pps).min(nal.len());
            let h_off = recon.len();
            recon.extend_from_slice(&nal[..header]);
            push_unenc(&mut unenc_ranges, h_off as u64, header as u64);
            aad.extend_from_slice(&nal[..header]);
            // 残り (スライスデータ) を暗号化
            let rest = &nal[header..];
            let e_off = recon.len();
            recon.extend_from_slice(rest); // 一旦平文を置く
            enc_positions.push((e_off, rest.len()));
            plaintext.extend_from_slice(rest);
        } else {
            // 非 VCL: NAL 全体を平文
            let n_off = recon.len();
            recon.extend_from_slice(nal);
            push_unenc(&mut unenc_ranges, n_off as u64, nal.len() as u64);
            aad.extend_from_slice(nal);
        }
    }

    // NAL が見つからない (スタートコード無し) 場合は全体を平文フレームとして扱う。
    if nalus.is_empty() {
        recon.extend_from_slice(frame);
        push_unenc(&mut unenc_ranges, 0, frame.len() as u64);
        aad.extend_from_slice(frame);
    }

    // 暗号化 (AAD = 平文レンジ結合)。
    let full = full_nonce(nonce);
    let (ct, tag) = gcm::encrypt_detached(key, &full, &aad, &plaintext);
    // 暗号文を recon の暗号化位置へ戻す。
    let mut ci = 0;
    for &(off, size) in &enc_positions {
        recon[off..off + size].copy_from_slice(&ct[ci..ci + size]);
        ci += size;
    }

    // supplemental: [tag8][nonce uleb][ranges uleb pairs][size 1][magic 2]
    let nonce_uleb = uleb128::encode(nonce as u64);
    let mut ranges_bytes = Vec::new();
    for &(off, size) in &unenc_ranges {
        uleb128::encode_to(&mut ranges_bytes, off);
        uleb128::encode_to(&mut ranges_bytes, size);
    }
    let suppl_size = 8 + nonce_uleb.len() + ranges_bytes.len() + 1 + 2;

    let mut out = recon;
    out.extend_from_slice(&tag[..8]);
    out.extend_from_slice(&nonce_uleb);
    out.extend_from_slice(&ranges_bytes);
    // supplemental size は 1byte。巨大フレーム (多スライス) では溢れ得るが
    // baseline 単一スライスでは収まる。溢れたら 0 を書いて無効フレーム化を避ける。
    out.push((suppl_size & 0xFF) as u8);
    out.extend_from_slice(&MAGIC_MARKER);
    out
}

/// `encrypt_h264` の逆変換。テスト用 (受信側の復号と同じロジック)。
/// 復号すると元の (正規化された) Annex-B フレームが得られる。
pub fn decrypt_h264(key: &[u8; 16], frame: &[u8]) -> Result<Vec<u8>, FrameError> {
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
    // [tag8][nonce uleb][ranges...][size 1][magic 2]
    let tag8 = &frame[suppl_start..suppl_start + 8];
    let mut p = suppl_start + 8;
    let (nonce_val, n) = uleb128::decode(&frame[p..l - 3]).ok_or(FrameError::NotProtocolFrame)?;
    p += n;
    // ranges (size 1byte と magic 2byte を除いた残り)
    let ranges_end = l - 3;
    let mut unenc_ranges: Vec<(u64, u64)> = Vec::new();
    while p < ranges_end {
        let (off, a) = uleb128::decode(&frame[p..ranges_end]).ok_or(FrameError::NotProtocolFrame)?;
        p += a;
        let (size, b) = uleb128::decode(&frame[p..ranges_end]).ok_or(FrameError::NotProtocolFrame)?;
        p += b;
        unenc_ranges.push((off, size));
    }

    let body = &frame[..suppl_start]; // interleaved frame
    // 平文レンジ (AAD) と暗号文を body から分離する。
    let mut aad: Vec<u8> = Vec::new();
    let mut ciphertext: Vec<u8> = Vec::new();
    let mut enc_positions: Vec<(usize, usize)> = Vec::new();
    let mut cursor = 0usize;
    for &(off, size) in &unenc_ranges {
        let off = off as usize;
        let size = size as usize;
        if off > body.len() || off + size > body.len() {
            return Err(FrameError::Truncated);
        }
        if off > cursor {
            enc_positions.push((cursor, off - cursor));
            ciphertext.extend_from_slice(&body[cursor..off]);
        }
        aad.extend_from_slice(&body[off..off + size]);
        cursor = off + size;
    }
    if cursor < body.len() {
        enc_positions.push((cursor, body.len() - cursor));
        ciphertext.extend_from_slice(&body[cursor..]);
    }

    let full = full_nonce(nonce_val as u32);
    let plain = gcm::decrypt_trunc8(key, &full, &aad, &ciphertext, tag8).ok_or(FrameError::Crypto)?;

    // 復号平文を暗号化位置へ戻して元フレームを再構成。
    let mut out = body.to_vec();
    let mut pi = 0;
    for &(off, size) in &enc_positions {
        out[off..off + size].copy_from_slice(&plain[pi..pi + size]);
        pi += size;
    }
    Ok(out)
}

/// 映像フレーム用の送信暗号器 (音声 FrameCryptor の H264 版)。
/// nonce/generation は音声とは独立のカウンタで進める。
pub struct VideoFrameCryptor {
    ratchet: KeyRatchet,
    nonce: u32,
    epoch: u64,
}

impl VideoFrameCryptor {
    pub fn with_epoch(base_secret: [u8; 16], epoch: u64) -> Self {
        Self { ratchet: KeyRatchet::new(base_secret), nonce: 0, epoch }
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    /// H264 アクセスユニットを暗号化する。
    pub fn encrypt(&mut self, frame: &[u8]) -> Vec<u8> {
        let gen = generation(self.nonce) as u32;
        let key = self.ratchet.key(gen);
        let out = encrypt_h264(&key, self.nonce, frame);
        self.nonce = self.nonce.wrapping_add(1);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dave::ratchet::KeyRatchet;

    /// [startcode] を付けて NAL を Annex-B に連結する。
    fn annexb(nals: &[(bool, &[u8])]) -> Vec<u8> {
        let mut v = Vec::new();
        for (long, nal) in nals {
            if *long {
                v.extend_from_slice(&[0, 0, 0, 1]);
            } else {
                v.extend_from_slice(&[0, 0, 1]);
            }
            v.extend_from_slice(nal);
        }
        v
    }

    #[test]
    fn finds_3_and_4_byte_start_codes() {
        let data = annexb(&[(true, &[0x67, 1, 2]), (false, &[0x68, 3]), (false, &[0x65, 4, 5])]);
        let nalus = find_nalus(&data);
        assert_eq!(nalus.len(), 3);
    }

    #[test]
    fn exp_golomb_counts_three_values() {
        // 3 つの ue(v): 1 ('1'), 1 ('1'), 1 ('1') = ビット列 111... → 1 バイト目で 3 値
        let payload = [0b1110_0000u8, 0x00];
        // 3 値の最終ビットは bit index 2 → byte 0 → +1 = 1
        assert_eq!(bytes_covering_h264_pps(&payload), 1);
    }

    #[test]
    fn round_trip_recovers_normalized_frame() {
        let key = [0x33u8; 16];
        // SPS(非VCL, type7) + PPS(type8) + IDR(type5, スライスデータ長め)
        let mut idr = vec![0x65u8, 0x88]; // header + slice header 開始
        idr.extend((0..500u32).map(|i| (i % 253) as u8)); // スライスデータ
        let frame = annexb(&[(true, &[0x67, 0x42, 0x00]), (true, &[0x68, 0xCE]), (true, &idr)]);

        let enc = encrypt_h264(&key, 7, &frame);
        assert_eq!(&enc[enc.len() - 2..], &MAGIC_MARKER);
        // 暗号文なので元と違う (少なくともスライスデータ部分)
        assert_ne!(enc[..frame.len()], frame[..]);

        let dec = decrypt_h264(&key, &enc).unwrap();
        // 復号すると正規化フレーム (全スタートコード 4byte) が戻る。
        let normalized = annexb(&[
            (true, &[0x67, 0x42, 0x00]),
            (true, &[0x68, 0xCE]),
            (true, &idr),
        ]);
        assert_eq!(dec, normalized);
    }

    #[test]
    fn non_vcl_fully_unencrypted() {
        let key = [1u8; 16];
        // SPS のみ (非 VCL) → 暗号化バイトは 0、interleaved frame は元と同じ (正規化のみ)
        let frame = annexb(&[(true, &[0x67, 0xAA, 0xBB, 0xCC])]);
        let enc = encrypt_h264(&key, 0, &frame);
        // body 部分 = 正規化フレームそのまま (非 VCL は平文)
        assert_eq!(&enc[..frame.len()], &frame[..]);
        let dec = decrypt_h264(&key, &enc).unwrap();
        assert_eq!(dec, frame);
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let mut idr = vec![0x65u8, 0x88];
        idr.extend(vec![0xEE; 100]);
        let frame = annexb(&[(true, &idr)]);
        let enc = encrypt_h264(&[9u8; 16], 3, &frame);
        assert!(decrypt_h264(&[8u8; 16], &enc).is_err());
    }

    #[test]
    fn cryptor_advances_nonce() {
        let base = [5u8; 16];
        let mut c = VideoFrameCryptor::with_epoch(base, 1);
        let mut idr = vec![0x65u8, 0x88];
        idr.extend(vec![0x11; 60]);
        let frame = annexb(&[(true, &idr)]);
        let f0 = c.encrypt(&frame);
        let f1 = c.encrypt(&frame);
        assert_ne!(f0, f1); // nonce 前進で暗号文が変わる

        // generation 0 の鍵で復号できる
        let mut r = KeyRatchet::new(base);
        let k0 = r.key(0);
        assert_eq!(decrypt_h264(&k0, &f0).unwrap(), frame);
        assert_eq!(decrypt_h264(&k0, &f1).unwrap(), frame);
    }
}
