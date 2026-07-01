//! Opus エンコーダ（チケット 4-4）。48kHz / ステレオ / 20ms フレーム。

use opus::{Application, Channels, Encoder};

use crate::error::AudioError;
use crate::pcm::SAMPLE_RATE;

pub struct OpusEncoder {
    enc: Encoder,
    out: Vec<u8>,
}

impl OpusEncoder {
    pub fn new() -> Result<Self, AudioError> {
        let mut enc = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Audio)?;
        // Lavalink 既定相当の品質。失敗しても致命的ではないので無視。
        let _ = enc.set_bitrate(opus::Bitrate::Bits(96_000));
        Ok(Self { enc, out: vec![0u8; 4000] })
    }

    /// 20ms 分のステレオ PCM（インターリーブ, 長さ 960*2）を Opus パケットへ。
    pub fn encode_frame(&mut self, pcm: &[f32]) -> Result<Vec<u8>, AudioError> {
        let n = self.enc.encode_float(pcm, &mut self.out)?;
        Ok(self.out[..n].to_vec())
    }
}
