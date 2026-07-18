//! 映像 RTP パケット化 (実験的・H264 / RFC 6184)。
//!
//! Discord の映像は音声と同じ UDP ソケット・同じトランスポート暗号で、
//! 別 SSRC / payload type の RTP として送る。クロックは 90kHz。
//! 1 フレーム (アクセスユニット) 内の全パケットは同じ timestamp を持ち、
//! 最後のパケットにマーカービットを立てる。
//!
//! ⚠️ ボットの映像送信は Discord 非公式。結線手順と検証計画は
//! docs/video-streaming-plan.md を参照。このモジュール自体は純粋なパケット化
//! ロジックで、既存の音声経路には影響しない。

use crate::crypto::Cipher;

/// 送出する映像 1 フレーム (アクセスユニット)。
#[derive(Debug)]
pub struct VideoFrame {
    /// 開始コード / 長さプレフィックスを剥がした NAL unit 列 (SPS/PPS 含んでよい)。
    pub nals: Vec<Vec<u8>>,
    /// 90kHz タイムスタンプ (コンテナ由来、または fps 換算で単調増加させる)。
    pub timestamp_90k: u32,
}

/// 90kHz クロック。フレームレート fps のとき 1 フレームの増分は 90000/fps。
pub const VIDEO_CLOCK_HZ: u32 = 90_000;
/// Discord クライアント慣例の H264 payload type (select_protocol の codecs と一致させる)。
pub const H264_PAYLOAD_TYPE: u8 = 101;
/// 暗号化オーバーヘッド (tag16 + nonce4) を見込んだ 1 パケットの最大 NAL ペイロード。
pub const MAX_PAYLOAD: usize = 1200;

/// H264 の NAL 単位を RTP パケット列にする (単一 NAL / FU-A 分割)。
pub struct H264Packetizer {
    ssrc: u32,
    sequence: u16,
    /// 現在のフレームの RTP timestamp (90kHz)。フレームごとに呼び出し側が進める。
    timestamp: u32,
    nonce_counter: u32,
}

impl H264Packetizer {
    pub fn new(ssrc: u32) -> Self {
        Self { ssrc, sequence: rand::random(), timestamp: rand::random(), nonce_counter: 1 }
    }

    /// フレームレートに応じて timestamp を進める (次のフレームへ)。
    pub fn advance_frame(&mut self, fps: u32) {
        self.timestamp = self.timestamp.wrapping_add(VIDEO_CLOCK_HZ / fps.max(1));
    }

    /// 90kHz timestamp を直接指定する (コンテナのタイムスタンプに追従する場合)。
    pub fn set_timestamp_90k(&mut self, ts: u32) {
        self.timestamp = ts;
    }

    fn write_header(&self, buf: &mut Vec<u8>, marker: bool) {
        buf.push(0x80);
        buf.push(if marker { 0x80 | H264_PAYLOAD_TYPE } else { H264_PAYLOAD_TYPE });
        buf.extend_from_slice(&self.sequence.to_be_bytes());
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.extend_from_slice(&self.ssrc.to_be_bytes());
    }

    /// RTP ペイロード 1 つを暗号化してパケット化する。
    fn build_packet(&mut self, cipher: &Cipher, payload: &[u8], marker: bool) -> Option<Vec<u8>> {
        let mut packet = Vec::with_capacity(12 + payload.len() + 16 + 4);
        self.write_header(&mut packet, marker);
        let aad = packet.clone();
        let ciphertext = cipher.encrypt(self.nonce_counter, &aad, payload)?;
        packet.extend_from_slice(&ciphertext);
        packet.extend_from_slice(&self.nonce_counter.to_be_bytes());
        self.sequence = self.sequence.wrapping_add(1);
        self.nonce_counter = self.nonce_counter.wrapping_add(1);
        Some(packet)
    }

    /// 1 フレームぶんの NAL 群を RTP パケット列にする。
    /// `nals` は開始コード/長さプレフィックスを剥がした生の NAL unit 列。
    /// フレーム最後のパケットにマーカービットを立てる。
    pub fn packetize_frame(&mut self, cipher: &Cipher, nals: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for (ni, nal) in nals.iter().enumerate() {
            if nal.is_empty() {
                continue;
            }
            let last_nal = ni + 1 == nals.len();
            if nal.len() <= MAX_PAYLOAD {
                // 単一 NAL ユニットパケット
                if let Some(p) = self.build_packet(cipher, nal, last_nal) {
                    out.push(p);
                }
            } else {
                // FU-A 分割 (RFC 6184 §5.8)
                let indicator = (nal[0] & 0xE0) | 28; // NRI 維持 + type=28 (FU-A)
                let nal_type = nal[0] & 0x1F;
                let body = &nal[1..];
                let chunks: Vec<&[u8]> = body.chunks(MAX_PAYLOAD - 2).collect();
                let n = chunks.len();
                for (ci, chunk) in chunks.into_iter().enumerate() {
                    let start = ci == 0;
                    let end = ci + 1 == n;
                    let fu_header = ((start as u8) << 7) | ((end as u8) << 6) | nal_type;
                    let mut payload = Vec::with_capacity(2 + chunk.len());
                    payload.push(indicator);
                    payload.push(fu_header);
                    payload.extend_from_slice(chunk);
                    if let Some(p) = self.build_packet(cipher, &payload, last_nal && end) {
                        out.push(p);
                    }
                }
            }
        }
        out
    }
}

/// Annex-B ストリーム (00 00 01 / 00 00 00 01 区切り) を NAL unit 列に分割する。
pub fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut nal_start: Option<usize> = None;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = nal_start {
                // 直前の NAL の終端 (00 00 01 の直前、00 00 00 01 なら更に 1 戻す)
                let mut end = i;
                if end > s && data[end - 1] == 0 {
                    end -= 1;
                }
                if end > s {
                    out.push(&data[s..end]);
                }
            }
            nal_start = Some(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some(s) = nal_start {
        if s < data.len() {
            out.push(&data[s..]);
        }
    }
    out
}

/// AVCC 形式 (MP4 内: [len(N バイト BE)][NAL] の繰り返し) を NAL unit 列に分割する。
/// `len_size` は avcC ボックスの lengthSizeMinusOne+1 (通常 4)。
pub fn split_avcc(data: &[u8], len_size: usize) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    if len_size == 0 || len_size > 4 {
        return out;
    }
    while i + len_size <= data.len() {
        let mut len: usize = 0;
        for b in &data[i..i + len_size] {
            len = (len << 8) | *b as usize;
        }
        i += len_size;
        if len == 0 || i + len > data.len() {
            break;
        }
        out.push(&data[i..i + len]);
        i += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{Cipher, Mode};

    fn cipher() -> Cipher {
        Cipher::new(Mode::XChaCha20Poly1305, &[7u8; 32])
    }

    #[test]
    fn single_nal_packet_layout() {
        let c = cipher();
        let mut p = H264Packetizer::new(777);
        let nal = vec![0x65u8; 100]; // IDR (type 5)
        let pkts = p.packetize_frame(&c, &[&nal]);
        assert_eq!(pkts.len(), 1);
        let pkt = &pkts[0];
        // 12 (header) + 100 (nal) + 16 (tag) + 4 (nonce)
        assert_eq!(pkt.len(), 12 + 100 + 16 + 4);
        assert_eq!(pkt[0], 0x80);
        // フレーム最後のパケットなのでマーカービット + PT
        assert_eq!(pkt[1], 0x80 | H264_PAYLOAD_TYPE);
        assert_eq!(&pkt[8..12], &777u32.to_be_bytes());
    }

    #[test]
    fn large_nal_is_fragmented_fu_a() {
        let c = cipher();
        let mut p = H264Packetizer::new(1);
        let mut nal = vec![0x65u8]; // ヘッダ (NRI=3, type=5)
        nal.extend(vec![0xABu8; MAX_PAYLOAD * 2]); // 2 分割強
        let pkts = p.packetize_frame(&c, &[&nal]);
        assert!(pkts.len() >= 3);
        // マーカーは最後のみ
        for (i, pkt) in pkts.iter().enumerate() {
            let marker = pkt[1] & 0x80 != 0;
            assert_eq!(marker, i + 1 == pkts.len());
        }
    }

    #[test]
    fn fu_a_fragments_reassemble_to_original() {
        // 暗号化なしで検証するため build_packet を通さず分割ロジックだけ再現するのは
        // 冗長なので、復号して確かめる。
        let c = cipher();
        let mut p = H264Packetizer::new(42);
        let mut nal = vec![0x41u8]; // NRI=2, type=1 (non-IDR)
        nal.extend((0..3000u32).map(|i| (i % 251) as u8));
        let pkts = p.packetize_frame(&c, &[&nal]);

        let mut reassembled: Vec<u8> = Vec::new();
        for pkt in &pkts {
            let aad = &pkt[..12];
            let nonce = u32::from_be_bytes(pkt[pkt.len() - 4..].try_into().unwrap());
            let ct = &pkt[12..pkt.len() - 4];
            let payload = c.decrypt(nonce, aad, ct).expect("decrypt");
            let indicator = payload[0];
            assert_eq!(indicator & 0x1F, 28, "FU-A type");
            let fu = payload[1];
            let start = fu & 0x80 != 0;
            if start {
                // NAL ヘッダ再構築: F/NRI (indicator 上位) + type (fu 下位)
                reassembled.push((indicator & 0xE0) | (fu & 0x1F));
            }
            reassembled.extend_from_slice(&payload[2..]);
        }
        assert_eq!(reassembled, nal);
    }

    #[test]
    fn annex_b_split() {
        // [00 00 00 01] SPS [00 00 01] PPS [00 00 01] IDR
        let mut data = vec![0, 0, 0, 1, 0x67, 1, 2];
        data.extend_from_slice(&[0, 0, 1, 0x68, 3]);
        data.extend_from_slice(&[0, 0, 1, 0x65, 4, 5, 6]);
        let nals = split_annex_b(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67, 1, 2]);
        assert_eq!(nals[1], &[0x68, 3]);
        assert_eq!(nals[2], &[0x65, 4, 5, 6]);
    }

    #[test]
    fn avcc_split() {
        // [len=3][0x67 1 2] [len=2][0x68 3]
        let data = [0u8, 0, 0, 3, 0x67, 1, 2, 0, 0, 0, 2, 0x68, 3];
        let nals = split_avcc(&data, 4);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x67, 1, 2]);
        assert_eq!(nals[1], &[0x68, 3]);
    }
}
