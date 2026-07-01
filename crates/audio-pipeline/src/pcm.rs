//! 正準 PCM フォーマットと変換ユーティリティ。
//! 内部表現: 48kHz / ステレオ / f32 インターリーブ（L,R,L,R,...）。

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 1 チャンネルあたりのサンプル数（20ms）。
pub const FRAME_SAMPLES: usize = 960;
/// インターリーブ 1 フレームの長さ（960 * 2ch）。
pub const FRAME_LEN: usize = FRAME_SAMPLES * CHANNELS;

/// 任意 ch のインターリーブ f32 をステレオ(2ch)へ整える。
pub fn to_stereo(samples: &[f32], channels: usize) -> Vec<f32> {
    match channels {
        2 => samples.to_vec(),
        1 => {
            let mut out = Vec::with_capacity(samples.len() * 2);
            for &s in samples {
                out.push(s);
                out.push(s);
            }
            out
        }
        n if n > 2 => {
            // 先頭 2ch を採用（簡易ダウンミックス）。
            let mut out = Vec::with_capacity(samples.len() / n * 2);
            for frame in samples.chunks_exact(n) {
                out.push(frame[0]);
                out.push(frame[1]);
            }
            out
        }
        _ => samples.to_vec(),
    }
}

/// ステレオインターリーブを src_rate → 48kHz に線形補間でリサンプルする。
/// （rubato による高品質リサンプルは将来の改善。まずは依存を増やさず線形で。）
pub fn resample_to_48k(stereo: &[f32], src_rate: u32) -> Vec<f32> {
    if src_rate == SAMPLE_RATE || stereo.is_empty() {
        return stereo.to_vec();
    }
    let in_frames = stereo.len() / 2;
    let ratio = SAMPLE_RATE as f64 / src_rate as f64;
    let out_frames = ((in_frames as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_frames * 2);
    for i in 0..out_frames {
        let src_pos = i as f64 / ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;
        let i0 = idx.min(in_frames - 1);
        let i1 = (idx + 1).min(in_frames - 1);
        for ch in 0..2 {
            let a = stereo[i0 * 2 + ch];
            let b = stereo[i1 * 2 + ch];
            out.push(a + (b - a) * frac);
        }
    }
    out
}

/// ストリーミング用の状態付き線形リサンプラ（src_rate → 48kHz）。
/// チャンク境界をまたいで補間の連続性を保つ。`push` で 48kHz ステレオを返す。
pub struct StreamResampler {
    src_rate: u32,
    pending: Vec<f32>, // 未消費のソースステレオ（インターリーブ）
    frac_pos: f64,     // pending 内のフラクショナルなソースフレーム位置
}

impl StreamResampler {
    pub fn new(src_rate: u32) -> Self {
        Self { src_rate, pending: Vec::new(), frac_pos: 0.0 }
    }

    /// ソースのステレオ(インターリーブ)を投入し、48kHz ステレオ(インターリーブ)を返す。
    pub fn push(&mut self, src_stereo: &[f32]) -> Vec<f32> {
        if self.src_rate == SAMPLE_RATE {
            return src_stereo.to_vec();
        }
        self.pending.extend_from_slice(src_stereo);
        let in_frames = self.pending.len() / 2;
        // 出力 1 フレームあたりのソース前進量。
        let step = self.src_rate as f64 / SAMPLE_RATE as f64;
        let mut out = Vec::new();
        loop {
            let idx = self.frac_pos.floor() as usize;
            if idx + 1 >= in_frames {
                break; // 線形補間に次サンプルが必要。
            }
            let frac = (self.frac_pos - idx as f64) as f32;
            for ch in 0..2 {
                let a = self.pending[idx * 2 + ch];
                let b = self.pending[(idx + 1) * 2 + ch];
                out.push(a + (b - a) * frac);
            }
            self.frac_pos += step;
        }
        // 消費済みソースフレームを破棄（連続性のため floor まで残す）。
        let consumed = self.frac_pos.floor() as usize;
        if consumed > 0 {
            self.pending.drain(0..consumed * 2);
            self.frac_pos -= consumed as f64;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_resampler_upsamples() {
        let mut r = StreamResampler::new(24_000);
        let mut total = 0;
        for _ in 0..4 {
            total += r.push(&[0.0, 0.0, 1.0, 1.0]).len() / 2;
        }
        assert!((14..=18).contains(&total), "got {total}");
    }

    #[test]
    fn mono_to_stereo_duplicates() {
        let s = to_stereo(&[1.0, 2.0], 1);
        assert_eq!(s, vec![1.0, 1.0, 2.0, 2.0]);
    }

    #[test]
    fn resample_noop_at_48k() {
        let v = vec![0.1, 0.2, 0.3, 0.4];
        assert_eq!(resample_to_48k(&v, 48_000), v);
    }

    #[test]
    fn resample_doubles_length_from_24k() {
        let v = vec![0.0, 0.0, 1.0, 1.0];
        let out = resample_to_48k(&v, 24_000);
        assert_eq!(out.len() / 2, 4);
    }
}
