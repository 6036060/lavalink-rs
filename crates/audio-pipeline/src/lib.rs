//! 音声処理パイプライン（フェーズ4）。
//!
//! 役割: 圧縮音声(WebM/MP4 等) → デコード(symphonia) → 48kHz/stereo 化 →
//!       フィルタチェーン(4-3) → Opus エンコード(4-4) → 20ms Opus フレーム列。
//!       `discord-voice::VoiceConnection::send_opus_frame` に供給する。
//!
//! パススルー最適化(4-5): ソースが既に Opus かつフィルタ未適用なら、デコード/再エンコードを
//! 省略してそのまま送出すべき（[`can_passthrough`]）。Opus パケットの抽出（mkv/ogg デマクサ）
//! は今後の実装。

#![forbid(unsafe_code)]

pub mod decoder;
pub mod encoder;
pub mod error;
pub mod filters;
pub mod mpegts;
pub mod pcm;
pub mod stream_source;
pub mod timescale;

pub use error::AudioError;
pub use encoder::OpusEncoder;
pub use filters::FilterChain;
pub use mpegts::TsToAdts;
pub use stream_source::SharedBuffer;
pub use timescale::Timescale;

use lavalink_protocol::Filters;
use pcm::FRAME_LEN;

/// 再生中にライブ更新できるフィルタ設定。`version` が変わると再生タスクが
/// フィルタチェーン / timescale を作り直す。
#[derive(Clone, Default)]
pub struct SharedFilters {
    pub version: u64,
    pub filters: Filters,
}

impl SharedFilters {
    pub fn new(filters: Filters) -> Self {
        Self { version: 0, filters }
    }
    /// フィルタを差し替え、バージョンを進める（再生タスクへ変更を通知）。
    pub fn update(&mut self, filters: Filters) {
        self.filters = filters;
        self.version = self.version.wrapping_add(1);
    }
}

/// ソースが Opus かつフィルタが無変更ならパススルー可能。
pub fn can_passthrough(source_is_opus: bool, filters: &Filters) -> bool {
    source_is_opus && FilterChain::from_filters(filters).is_identity()
}

/// デコード済み PCM を フィルタ → Opus する変換器。
pub struct AudioPipeline {
    chain: FilterChain,
    encoder: OpusEncoder,
}

impl AudioPipeline {
    pub fn new(filters: &Filters) -> Result<Self, AudioError> {
        // timescale(speed/pitch/rate) はストリーミング経路(`decode_stream_to_opus`)で
        // WSOLA+リサンプルとして適用する（バッチ経路では未適用）。
        let chain = FilterChain::from_filters(filters);
        Ok(Self { chain, encoder: OpusEncoder::new()? })
    }

    /// 48kHz ステレオ PCM を 20ms ごとに フィルタ→Opus し、Opus パケット列を返す。
    /// 末尾の半端フレームは 0 パディングする。
    pub fn pcm_to_opus_frames(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>, AudioError> {
        let mut out = Vec::with_capacity(pcm.len() / FRAME_LEN + 1);
        for chunk in pcm.chunks(FRAME_LEN) {
            let mut frame = [0.0f32; FRAME_LEN];
            frame[..chunk.len()].copy_from_slice(chunk);
            self.chain.process(&mut frame);
            out.push(self.encoder.encode_frame(&frame)?);
        }
        Ok(out)
    }

    /// 圧縮音声データを デコード→フィルタ→Opus で一括変換する。
    pub fn decode_to_opus(
        &mut self,
        data: Vec<u8>,
        ext_hint: Option<&str>,
    ) -> Result<Vec<Vec<u8>>, AudioError> {
        let pcm = decoder::decode(data, ext_hint)?;
        self.pcm_to_opus_frames(&pcm)
    }

    /// ストリーミング変換: 成長する共有バッファからパケット単位でデコード→リサンプル→
    /// timescale→フィルタ→Opus し、20ms フレームごとに `on_frame` を呼ぶ。`on_frame` が
    /// `false` を返したら停止する。全曲をメモリに溜めずに逐次送出できる。
    ///
    /// `skip_ms` は先頭からのスキップ量（seek 用）。リサンプル・エンコードより手前の
    /// デコード直後で安価に読み捨てるため、長距離シークでも数秒で追いつく。
    ///
    /// `filters` は再生中にライブ更新できる共有設定。`version` が変わるとフィルタチェーンと
    /// timescale を作り直す（一時停止やピッチ/速度/音量の変更が即座に反映される）。
    pub fn decode_stream_to_opus<F>(
        &mut self,
        buf: std::sync::Arc<std::sync::Mutex<SharedBuffer>>,
        ext_hint: Option<&str>,
        filters: std::sync::Arc<std::sync::Mutex<SharedFilters>>,
        skip_ms: u64,
        mut on_frame: F,
    ) -> Result<(), AudioError>
    where
        F: FnMut(Vec<u8>) -> bool,
    {
        let src = stream_source::StreamingSource::new(buf);
        let mss =
            symphonia::core::io::MediaSourceStream::new(Box::new(src), Default::default());
        let mut dec = decoder::StreamDecoder::new(mss, ext_hint)?;
        let mut resampler = pcm::StreamResampler::new(dec.src_rate);
        let mut accum: Vec<f32> = Vec::new();

        // seek: ソースレートのステレオサンプル数に換算してデコード直後に読み捨てる。
        let mut skip_samples: usize =
            (skip_ms as usize * dec.src_rate as usize / 1000).saturating_mul(2);

        // 初期フィルタ / timescale を共有設定から構築。
        let (mut chain, mut timescale, mut last_ver) = {
            let g = filters.lock().unwrap();
            (
                FilterChain::from_filters(&g.filters),
                Timescale::from_filters(&g.filters),
                g.version,
            )
        };

        while let Some(stereo) = dec.next_stereo()? {
            // スキップ区間の読み捨て（デコードのみで先へ進むので高速）。
            let stereo = if skip_samples > 0 {
                if skip_samples >= stereo.len() {
                    skip_samples -= stereo.len();
                    continue;
                }
                let rest = stereo[skip_samples..].to_vec();
                skip_samples = 0;
                rest
            } else {
                stereo
            };
            // ライブ更新の取り込み（バージョン変化時のみ作り直し）。
            {
                let g = filters.lock().unwrap();
                if g.version != last_ver {
                    last_ver = g.version;
                    chain = FilterChain::from_filters(&g.filters);
                    timescale.reconfigure(&g.filters);
                }
            }
            let resampled = resampler.push(&stereo);
            let scaled = timescale.push(&resampled);
            accum.extend_from_slice(&scaled);
            while accum.len() >= FRAME_LEN {
                let mut frame = [0.0f32; FRAME_LEN];
                frame.copy_from_slice(&accum[..FRAME_LEN]);
                chain.process(&mut frame);
                let opus = self.encoder.encode_frame(&frame)?;
                if !on_frame(opus) {
                    return Ok(());
                }
                accum.drain(0..FRAME_LEN);
            }
        }
        // timescale の残り → accum へ。
        let tail = timescale.flush();
        accum.extend_from_slice(&tail);
        while accum.len() >= FRAME_LEN {
            let mut frame = [0.0f32; FRAME_LEN];
            frame.copy_from_slice(&accum[..FRAME_LEN]);
            chain.process(&mut frame);
            let opus = self.encoder.encode_frame(&frame)?;
            if !on_frame(opus) {
                return Ok(());
            }
            accum.drain(0..FRAME_LEN);
        }
        // 末尾の半端分を 0 パディングして送出。
        if !accum.is_empty() {
            let mut frame = [0.0f32; FRAME_LEN];
            let n = accum.len().min(FRAME_LEN);
            frame[..n].copy_from_slice(&accum[..n]);
            chain.process(&mut frame);
            let opus = self.encoder.encode_frame(&frame)?;
            let _ = on_frame(opus);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lavalink_protocol::Filters;

    #[test]
    fn passthrough_when_opus_and_no_filters() {
        assert!(can_passthrough(true, &Filters::default()));
    }

    #[test]
    fn no_passthrough_when_filters_present() {
        let f = Filters { volume: Some(2.0), ..Default::default() };
        assert!(!can_passthrough(true, &f));
    }

    #[test]
    fn no_passthrough_when_not_opus() {
        assert!(!can_passthrough(false, &Filters::default()));
    }
}
