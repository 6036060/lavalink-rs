//! Lavalink 互換のフィルタチェーン（チケット 4-3）。ステレオ f32 インターリーブ上で動作。
//!
//! 実装状況:
//! - 厳密実装: volume / channelMix / lowPass(one-pole) / tremolo
//! - 機能実装(数値は Lavaplayer と完全一致ではない・フェーズ6 で要精緻化):
//!   equalizer(15バンド peaking) / karaoke(センター除去) / vibrato(可変ディレイ) /
//!   rotation(オートパン) / distortion(波形整形)
//! - timescale(speed/pitch/rate) は本チェーンではなく `timescale.rs`（WSOLA＋リサンプル）が
//!   ストリーミング経路(`decode_stream_to_opus`)で適用する。

use std::f32::consts::PI;

use lavalink_protocol::Filters;

use crate::pcm::SAMPLE_RATE;

const SR: f32 = SAMPLE_RATE as f32;

/// RBJ biquad（直接形 I）。
#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn new(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn peaking(freq: f32, q: f32, gain_db: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * freq / SR;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        Biquad::new(
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        )
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

// ------------------------------ 各フィルタ ------------------------------

struct ChannelMix {
    ll: f32,
    lr: f32,
    rl: f32,
    rr: f32,
}
impl ChannelMix {
    fn process(&self, buf: &mut [f32]) {
        for s in buf.chunks_exact_mut(2) {
            let (l, r) = (s[0], s[1]);
            s[0] = l * self.ll + r * self.rl;
            s[1] = l * self.lr + r * self.rr;
        }
    }
}

struct LowPass {
    smoothing: f32,
    sl: f32,
    sr: f32,
}
impl LowPass {
    fn process(&mut self, buf: &mut [f32]) {
        for s in buf.chunks_exact_mut(2) {
            self.sl += (s[0] - self.sl) / self.smoothing;
            self.sr += (s[1] - self.sr) / self.smoothing;
            s[0] = self.sl;
            s[1] = self.sr;
        }
    }
}

struct Tremolo {
    step: f32,
    depth: f32,
    phase: f32,
}
impl Tremolo {
    fn process(&mut self, buf: &mut [f32]) {
        for s in buf.chunks_exact_mut(2) {
            // gain ∈ [1-depth, 1]
            let gain = 1.0 - self.depth * (0.5 - 0.5 * self.phase.sin());
            s[0] *= gain;
            s[1] *= gain;
            self.phase = (self.phase + self.step) % (2.0 * PI);
        }
    }
}

struct Rotation {
    step: f32,
    phase: f32,
}
impl Rotation {
    fn process(&mut self, buf: &mut [f32]) {
        for s in buf.chunks_exact_mut(2) {
            let pan = self.phase.sin(); // -1..1
            // 等パワー的オートパン
            let gl = ((1.0 - pan) * 0.5).sqrt() * std::f32::consts::SQRT_2;
            let gr = ((1.0 + pan) * 0.5).sqrt() * std::f32::consts::SQRT_2;
            s[0] *= gl;
            s[1] *= gr;
            self.phase = (self.phase + self.step) % (2.0 * PI);
        }
    }
}

struct Vibrato {
    step: f32,
    depth: f32,
    phase: f32,
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
    pos: usize,
}
impl Vibrato {
    // lavadsp VibratoFilter に合わせ、最大変調ディレイ ≈2ms（48kHz で 96 サンプル）。
    // 以前の 512 サンプル(≈10.7ms)は変調が過大でピッチが揺れすぎた。
    const MAX_DELAY: usize = 96; // < buffer len
    fn new(freq: f32, depth: f32) -> Self {
        let len = 256;
        Self {
            step: 2.0 * PI * freq / SR,
            depth,
            phase: 0.0,
            buf_l: vec![0.0; len],
            buf_r: vec![0.0; len],
            pos: 0,
        }
    }
    fn read(buf: &[f32], pos: usize, delay: f32) -> f32 {
        let len = buf.len();
        let read_pos = pos as f32 - delay;
        let read_pos = if read_pos < 0.0 { read_pos + len as f32 } else { read_pos };
        let i0 = read_pos.floor() as usize % len;
        let i1 = (i0 + 1) % len;
        let frac = read_pos - read_pos.floor();
        buf[i0] + (buf[i1] - buf[i0]) * frac
    }
    fn process(&mut self, buf: &mut [f32]) {
        for s in buf.chunks_exact_mut(2) {
            self.buf_l[self.pos] = s[0];
            self.buf_r[self.pos] = s[1];
            let delay = self.depth * Self::MAX_DELAY as f32 * (0.5 * (self.phase.sin() + 1.0));
            s[0] = Self::read(&self.buf_l, self.pos, delay);
            s[1] = Self::read(&self.buf_r, self.pos, delay);
            self.pos = (self.pos + 1) % self.buf_l.len();
            self.phase = (self.phase + self.step) % (2.0 * PI);
        }
    }
}

struct Distortion {
    sin_offset: f32,
    sin_scale: f32,
    cos_offset: f32,
    cos_scale: f32,
    tan_offset: f32,
    tan_scale: f32,
    offset: f32,
    scale: f32,
}
impl Distortion {
    #[inline]
    fn shape(&self, x: f32) -> f32 {
        let s = self.sin_scale * (x * self.sin_scale + self.sin_offset).sin();
        let c = self.cos_scale * (x * self.cos_scale + self.cos_offset).cos();
        let t = (self.tan_scale * (x * self.tan_scale + self.tan_offset).tan()).clamp(-4.0, 4.0);
        ((s + c + t) * x + self.offset) * self.scale
    }
    fn process(&self, buf: &mut [f32]) {
        for v in buf.iter_mut() {
            *v = self.shape(*v).clamp(-1.0, 1.0);
        }
    }
}

struct Equalizer {
    // 各チャンネル 15 バンドの peaking biquad。
    left: Vec<Biquad>,
    right: Vec<Biquad>,
}
impl Equalizer {
    const FREQS: [f32; 15] = [
        25.0, 40.0, 63.0, 100.0, 160.0, 250.0, 400.0, 630.0, 1000.0, 1600.0, 2500.0, 4000.0,
        6300.0, 10000.0, 16000.0,
    ];
    fn new(gains: [f32; 15]) -> Self {
        // Lavalink: band gain 0.25 で「2倍(=+6dB)」。よって dB ≒ gain * 24。
        let make = || -> Vec<Biquad> {
            Self::FREQS
                .iter()
                .zip(gains.iter())
                .filter(|(_, &g)| g != 0.0)
                .map(|(&f, &g)| Biquad::peaking(f, 1.41, g * 24.0))
                .collect()
        };
        Self { left: make(), right: make() }
    }
    fn process(&mut self, buf: &mut [f32]) {
        for s in buf.chunks_exact_mut(2) {
            for b in self.left.iter_mut() {
                s[0] = b.process(s[0]);
            }
            for b in self.right.iter_mut() {
                s[1] = b.process(s[1]);
            }
        }
    }
}

/// lavadsp の KaraokeConverter と同一のアルゴリズム。
/// 左右の差分でセンター（ボーカル）を除去し、filterBand 周辺のモノ成分だけ
/// バンドパスで復元する: `out_l = l - r*level + bandpass(mono)*monoLevel*level`。
struct Karaoke {
    level: f32,
    mono_level: f32,
    // 2 次 IIR バンドパス係数（lavadsp と同じ導出）
    a: f32,
    b: f32,
    c: f32,
    y1: f32,
    y2: f32,
}
impl Karaoke {
    fn new(level: f32, mono_level: f32, filter_band: f32, filter_width: f32) -> Self {
        let c = (-2.0 * PI * filter_width / SR).exp();
        let b = -4.0 * c / (1.0 + c) * (2.0 * PI * filter_band / SR).cos();
        // 数値誤差で負になり得るため 0 でクランプ（Java 実装は NaN になる）。
        let a = (1.0 - b * b / (4.0 * c)).max(0.0).sqrt() * (1.0 - c);
        Self { level, mono_level, a, b, c, y1: 0.0, y2: 0.0 }
    }
    fn process(&mut self, buf: &mut [f32]) {
        for s in buf.chunks_exact_mut(2) {
            let (l, r) = (s[0], s[1]);
            // モノ成分を filterBand 中心のバンドパスへ
            let y = self.a * ((l + r) * 0.5) - self.b * self.y1 - self.c * self.y2;
            self.y2 = self.y1;
            self.y1 = y;
            let o = y * self.mono_level * self.level;
            // センターカット + フィルタ済みモノの復元
            s[0] = l - r * self.level + o;
            s[1] = r - l * self.level + o;
        }
    }
}

// ------------------------------ チェーン ------------------------------

/// 適用順は Lavalink に倣い、volume を最後にする。
pub struct FilterChain {
    volume: f32,
    equalizer: Option<Equalizer>,
    karaoke: Option<Karaoke>,
    distortion: Option<Distortion>,
    rotation: Option<Rotation>,
    channel_mix: Option<ChannelMix>,
    vibrato: Option<Vibrato>,
    tremolo: Option<Tremolo>,
    low_pass: Option<LowPass>,
    timescale_requested: bool,
}

impl FilterChain {
    pub fn from_filters(f: &Filters) -> Self {
        let volume = f.volume.unwrap_or(1.0);

        let equalizer = f.equalizer.as_ref().and_then(|bands| {
            let mut gains = [0.0f32; 15];
            for b in bands {
                if (b.band as usize) < 15 {
                    // 公式仕様: gain は -0.25(ミュート)〜1.0。範囲外はクランプ。
                    gains[b.band as usize] = b.gain.clamp(-0.25, 1.0);
                }
            }
            if gains.iter().any(|&g| g != 0.0) {
                Some(Equalizer::new(gains))
            } else {
                None
            }
        });

        let karaoke = f.karaoke.map(|k| {
            Karaoke::new(
                k.level.unwrap_or(1.0),
                k.mono_level.unwrap_or(1.0),
                k.filter_band.unwrap_or(220.0),
                k.filter_width.unwrap_or(100.0).max(1.0),
            )
        });

        let distortion = f.distortion.map(|d| Distortion {
            sin_offset: d.sin_offset.unwrap_or(0.0),
            sin_scale: d.sin_scale.unwrap_or(1.0),
            cos_offset: d.cos_offset.unwrap_or(0.0),
            cos_scale: d.cos_scale.unwrap_or(1.0),
            tan_offset: d.tan_offset.unwrap_or(0.0),
            tan_scale: d.tan_scale.unwrap_or(1.0),
            offset: d.offset.unwrap_or(0.0),
            scale: d.scale.unwrap_or(1.0),
        });

        let rotation = f.rotation.and_then(|r| r.rotation_hz).filter(|h| *h > 0.0).map(|hz| Rotation {
            step: 2.0 * PI * hz / SR,
            phase: 0.0,
        });

        let channel_mix = f.channel_mix.map(|c| ChannelMix {
            ll: c.left_to_left.unwrap_or(1.0),
            lr: c.left_to_right.unwrap_or(0.0),
            rl: c.right_to_left.unwrap_or(0.0),
            rr: c.right_to_right.unwrap_or(1.0),
        });

        // 公式仕様の範囲（vibrato: freq ≤14, depth ≤1 / tremolo: depth ≤1）へ防御的にクランプ。
        // REST 層でも 400 検証するが、内部利用（デフォルト合成等）でも安全にする。
        let vibrato = f.vibrato.and_then(|v| {
            let freq = v.frequency.unwrap_or(0.0).min(14.0);
            let depth = v.depth.unwrap_or(0.0).min(1.0);
            if freq > 0.0 && depth > 0.0 {
                Some(Vibrato::new(freq, depth))
            } else {
                None
            }
        });

        let tremolo = f.tremolo.and_then(|t| {
            let freq = t.frequency.unwrap_or(0.0);
            let depth = t.depth.unwrap_or(0.0).min(1.0);
            if freq > 0.0 && depth > 0.0 {
                Some(Tremolo { step: 2.0 * PI * freq / SR, depth, phase: 0.0 })
            } else {
                None
            }
        });

        let low_pass = f
            .low_pass
            .and_then(|lp| lp.smoothing)
            .filter(|s| *s > 1.0)
            .map(|smoothing| LowPass { smoothing, sl: 0.0, sr: 0.0 });

        let timescale_requested = f.timescale.is_some();

        Self {
            volume,
            equalizer,
            karaoke,
            distortion,
            rotation,
            channel_mix,
            vibrato,
            tremolo,
            low_pass,
            timescale_requested,
        }
    }

    /// 何も変化させない（パススルー可能）か。
    pub fn is_identity(&self) -> bool {
        (self.volume - 1.0).abs() < f32::EPSILON
            && self.equalizer.is_none()
            && self.karaoke.is_none()
            && self.distortion.is_none()
            && self.rotation.is_none()
            && self.channel_mix.is_none()
            && self.vibrato.is_none()
            && self.tremolo.is_none()
            && self.low_pass.is_none()
            && !self.timescale_requested
    }

    /// timescale が要求されたが未実装か（呼び出し側で 1 度警告する用途）。
    pub fn timescale_unsupported(&self) -> bool {
        self.timescale_requested
    }

    /// ステレオ f32 インターリーブのフレームを in-place で処理する。
    pub fn process(&mut self, buf: &mut [f32]) {
        if let Some(eq) = self.equalizer.as_mut() {
            eq.process(buf);
        }
        if let Some(k) = self.karaoke.as_mut() {
            k.process(buf);
        }
        if let Some(d) = self.distortion.as_ref() {
            d.process(buf);
        }
        if let Some(r) = self.rotation.as_mut() {
            r.process(buf);
        }
        if let Some(cm) = self.channel_mix.as_ref() {
            cm.process(buf);
        }
        if let Some(v) = self.vibrato.as_mut() {
            v.process(buf);
        }
        if let Some(t) = self.tremolo.as_mut() {
            t.process(buf);
        }
        if let Some(lp) = self.low_pass.as_mut() {
            lp.process(buf);
        }
        // timescale はストリーミング経路（timescale.rs）で適用されるためここでは扱わない。
        if (self.volume - 1.0).abs() > f32::EPSILON {
            for v in buf.iter_mut() {
                *v *= self.volume;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lavalink_protocol::{ChannelMix as ChannelMixDto, Filters, LowPass as LowPassDto};

    fn chain(f: Filters) -> FilterChain {
        FilterChain::from_filters(&f)
    }

    #[test]
    fn empty_filters_is_identity() {
        assert!(chain(Filters::default()).is_identity());
    }

    #[test]
    fn volume_scales_samples() {
        let mut c = chain(Filters { volume: Some(2.0), ..Default::default() });
        assert!(!c.is_identity());
        let mut buf = vec![0.1, -0.2, 0.3, -0.4];
        c.process(&mut buf);
        assert!((buf[0] - 0.2).abs() < 1e-6);
        assert!((buf[3] + 0.8).abs() < 1e-6);
    }

    #[test]
    fn channel_mix_swaps_channels() {
        let mut c = chain(Filters {
            channel_mix: Some(ChannelMixDto {
                left_to_left: Some(0.0),
                left_to_right: Some(1.0),
                right_to_left: Some(1.0),
                right_to_right: Some(0.0),
            }),
            ..Default::default()
        });
        let mut buf = vec![1.0, 2.0]; // L=1,R=2 -> swap -> L=2,R=1
        c.process(&mut buf);
        assert_eq!(buf, vec![2.0, 1.0]);
    }

    #[test]
    fn lowpass_first_output_is_input_over_smoothing() {
        let mut c = chain(Filters {
            low_pass: Some(LowPassDto { smoothing: Some(4.0) }),
            ..Default::default()
        });
        let mut buf = vec![1.0, 1.0]; // 1 stereo frame
        c.process(&mut buf);
        // s += (x - s)/smoothing で s=0 開始 → 1/4
        assert!((buf[0] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn lowpass_converges_to_dc() {
        let mut c = chain(Filters {
            low_pass: Some(LowPassDto { smoothing: Some(2.0) }),
            ..Default::default()
        });
        let mut buf = vec![1.0; 200 * 2];
        c.process(&mut buf);
        assert!((buf[buf.len() - 1] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn karaoke_removes_center_vocal() {
        use lavalink_protocol::Karaoke as KaraokeDto;
        let mut c = chain(Filters {
            karaoke: Some(KaraokeDto {
                level: Some(1.0),
                mono_level: Some(1.0),
                filter_band: Some(220.0),
                filter_width: Some(100.0),
            }),
            ..Default::default()
        });
        // センター定位（左右同一）の 1kHz サイン波 ≒ ボーカル
        let n = 4800; // 100ms
        let mut buf = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = (2.0 * PI * 1000.0 * i as f32 / SR).sin() * 0.5;
            buf.push(v);
            buf.push(v);
        }
        let rms_in = (buf.iter().map(|v| v * v).sum::<f32>() / buf.len() as f32).sqrt();
        c.process(&mut buf);
        let rms_out = (buf.iter().map(|v| v * v).sum::<f32>() / buf.len() as f32).sqrt();
        // filterBand(220Hz) から離れたセンター成分は大幅に減衰する
        assert!(
            rms_out < rms_in * 0.1,
            "center not removed: in={rms_in} out={rms_out}"
        );
    }

    #[test]
    fn karaoke_keeps_side_content() {
        use lavalink_protocol::Karaoke as KaraokeDto;
        let mut c = chain(Filters {
            karaoke: Some(KaraokeDto {
                level: Some(1.0),
                mono_level: Some(1.0),
                filter_band: None, // デフォルト 220/100
                filter_width: None,
            }),
            ..Default::default()
        });
        // 左右逆相（サイド成分 ≒ 伴奏の広がり）はモノ成分ゼロなので残る
        // (lavadsp 準拠で out_l = l - r*level = 2l に増幅される)
        let mut buf = vec![0.25, -0.25, 0.25, -0.25];
        c.process(&mut buf);
        assert!((buf[0] - 0.5).abs() < 1e-6, "side content altered: {}", buf[0]);
        assert!((buf[1] + 0.5).abs() < 1e-6, "side content altered: {}", buf[1]);
    }

    #[test]
    fn tremolo_gain_within_bounds() {
        use lavalink_protocol::Tremolo as TremoloDto;
        let mut c = chain(Filters {
            tremolo: Some(TremoloDto { frequency: Some(5.0), depth: Some(0.5) }),
            ..Default::default()
        });
        let mut buf = vec![1.0; 2000 * 2];
        c.process(&mut buf);
        for v in &buf {
            assert!(*v >= 0.5 - 1e-3 && *v <= 1.0 + 1e-3, "out of bounds: {v}");
        }
    }
}
