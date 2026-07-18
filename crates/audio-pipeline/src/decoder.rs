//! コンテナ/コーデックのデコード（チケット 4-1/4-2, symphonia）。
//! 出力は 48kHz・ステレオ・f32 インターリーブに統一する。
//!
//! 注意: symphonia は Opus デコーダを持たない。YouTube の WebM/Opus は *デコードせず*
//! パススルーする方針（4-5）。本関数は AAC(MP4)/Vorbis(WebM)/PCM 等を対象とする。

use std::io::Cursor;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::error::AudioError;
use crate::pcm;

/// 圧縮音声データをデコードし、48kHz ステレオ f32 インターリーブを返す。
pub fn decode(data: Vec<u8>, ext_hint: Option<&str>) -> Result<Vec<f32>, AudioError> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = ext_hint {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| AudioError::Decode(e.to_string()))?;
    let mut format = probed.format;

    let (track_id, src_rate, src_ch) = {
        let track = format.default_track().ok_or(AudioError::Unsupported("no default track"))?;
        (
            track.id,
            track.codec_params.sample_rate.unwrap_or(48_000),
            track.codec_params.channels.map(|c| c.count()).unwrap_or(2),
        )
    };

    let track = format.default_track().ok_or(AudioError::Unsupported("no default track"))?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| AudioError::Decode(e.to_string()))?;

    let mut interleaved: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(_)) => break, // EOF とみなす
            Err(e) => return Err(AudioError::Decode(e.to_string())),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                let spec = *audio_buf.spec();
                let mut sb = SampleBuffer::<f32>::new(audio_buf.capacity() as u64, spec);
                sb.copy_interleaved_ref(audio_buf);
                interleaved.extend_from_slice(sb.samples());
            }
            Err(SymphoniaError::DecodeError(_)) => continue, // 一部破損パケットはスキップ
            Err(e) => return Err(AudioError::Decode(e.to_string())),
        }
    }

    let stereo = pcm::to_stereo(&interleaved, src_ch);
    Ok(pcm::resample_to_48k(&stereo, src_rate))
}

/// ストリーミングデコーダ。`MediaSourceStream`（成長するソース可）からパケット単位で
/// ソートレートのステレオ PCM を取り出す。全曲をメモリに溜めずに逐次デコードできる。
pub struct StreamDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    pub src_rate: u32,
    pub src_ch: usize,
}

impl StreamDecoder {
    pub fn new(mss: MediaSourceStream, ext_hint: Option<&str>) -> Result<Self, AudioError> {
        let mut hint = Hint::new();
        if let Some(ext) = ext_hint {
            hint.with_extension(ext);
        }
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
            .map_err(|e| AudioError::Decode(e.to_string()))?;
        let format = probed.format;
        let (track_id, src_rate, src_ch) = {
            let track =
                format.default_track().ok_or(AudioError::Unsupported("no default track"))?;
            (
                track.id,
                track.codec_params.sample_rate.unwrap_or(48_000),
                track.codec_params.channels.map(|c| c.count()).unwrap_or(2),
            )
        };
        let track = format.default_track().ok_or(AudioError::Unsupported("no default track"))?;
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| AudioError::Decode(e.to_string()))?;
        Ok(Self { format, decoder, track_id, src_rate, src_ch })
    }

    /// 次のステレオ PCM チャンク（ソートレート, インターリーブ）を返す。EOF で `None`。
    pub fn next_stereo(&mut self) -> Result<Option<Vec<f32>>, AudioError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymphoniaError::IoError(_)) => return Ok(None), // EOF
                Err(e) => return Err(AudioError::Decode(e.to_string())),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(audio_buf) => {
                    let spec = *audio_buf.spec();
                    let mut sb = SampleBuffer::<f32>::new(audio_buf.capacity() as u64, spec);
                    sb.copy_interleaved_ref(audio_buf);
                    return Ok(Some(pcm::to_stereo(sb.samples(), self.src_ch)));
                }
                Err(SymphoniaError::DecodeError(_)) => continue, // 破損パケットはスキップ
                Err(e) => return Err(AudioError::Decode(e.to_string())),
            }
        }
    }
}
