//! RTP パケット組み立て（Discord 音声フォーマット）。
//!
//! パケット = RTP ヘッダ(12) || 暗号文(opus+tag) || nonce(4, ビッグエンディアン counter)
//! ヘッダ = 0x80 0x78 seq(u16 BE) timestamp(u32 BE) ssrc(u32 BE)

use crate::crypto::Cipher;

/// 無音 Opus フレーム（送出停止前に 5 回送る）。
pub const SILENCE_FRAME: [u8; 3] = [0xF8, 0xFF, 0xFE];
/// 48kHz・20ms = 960 サンプル。timestamp の増分。
pub const TIMESTAMP_STEP: u32 = 960;

pub struct Packetizer {
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
    nonce_counter: u32,
}

impl Packetizer {
    pub fn new(ssrc: u32) -> Self {
        Self {
            ssrc,
            sequence: rand::random(),
            timestamp: rand::random(),
            nonce_counter: 1,
        }
    }

    fn write_header(&self, buf: &mut Vec<u8>) {
        buf.push(0x80);
        buf.push(0x78);
        buf.extend_from_slice(&self.sequence.to_be_bytes());
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.extend_from_slice(&self.ssrc.to_be_bytes());
    }

    /// 1 つの Opus フレームを RTP パケット化（暗号化込み）。seq/timestamp/nonce を進める。
    pub fn build(&mut self, cipher: &Cipher, opus: &[u8]) -> Option<Vec<u8>> {
        let mut packet = Vec::with_capacity(12 + opus.len() + 16 + 4);
        self.write_header(&mut packet);
        let aad = packet.clone(); // 12 バイトの RTP ヘッダ
        let ciphertext = cipher.encrypt(self.nonce_counter, &aad, opus)?;
        packet.extend_from_slice(&ciphertext);
        packet.extend_from_slice(&self.nonce_counter.to_be_bytes());

        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(TIMESTAMP_STEP);
        self.nonce_counter = self.nonce_counter.wrapping_add(1);
        Some(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{Cipher, Mode};

    #[test]
    fn packet_layout_is_header_ct_nonce() {
        let cipher = Cipher::new(Mode::XChaCha20Poly1305, &[1u8; 32]);
        let mut p = Packetizer::new(12345);
        let opus = vec![0xAA; 40];
        let pkt = p.build(&cipher, &opus).unwrap();
        // 12 (header) + 40 (opus) + 16 (tag) + 4 (nonce)
        assert_eq!(pkt.len(), 12 + 40 + 16 + 4);
        assert_eq!(pkt[0], 0x80);
        assert_eq!(pkt[1], 0x78);
        // ssrc (header bytes 8..12)
        assert_eq!(&pkt[8..12], &12345u32.to_be_bytes());
        // 末尾 4 バイト = nonce counter（最初のパケットは 1）
        let n = &pkt[pkt.len() - 4..];
        assert_eq!(n, &1u32.to_be_bytes());
    }
}
