//! ストリーミング timescale フィルタ（speed / pitch / rate）。
//!
//! 本家 Lavalink（SoundTouch）と同じセマンティクス:
//! - `speed`: テンポのみ変更（ピッチ不変）
//! - `pitch`: ピッチのみ変更（テンポ不変）
//! - `rate` : リサンプル（テンポ＋ピッチを同時に変更）
//!
//! 実装: WSOLA タイムストレッチ（係数 `T = pitch / speed`）→ 線形リサンプル
//! （係数 `R = pitch * rate`）の 2 段。48kHz ステレオ(インターリーブ)を入出力する。
//!
//! 導出（出力長 = 入力長 × T / R、出力ピッチ = R とおく）:
//!   出力長 = 入力長 / (speed * rate)  →  T / R = 1/(speed*rate)
//!   出力ピッチ = pitch * rate         →  R = pitch * rate
//!   ⇒ T = R / (speed*rate) = pitch / speed
//! 検算:
//!   speed のみ: R=1, T=1/speed → テンポ×speed・ピッチ不変 ✓
//!   pitch のみ: R=pitch, T=pitch → 長さ不変・ピッチ×pitch ✓
//!   rate のみ : R=rate, T=1 → テンポ×rate・ピッチ×rate ✓

use lavalink_protocol::Filters;

/// speed/pitch/rate を読み出し、健全な範囲にクランプする。
fn read_params(f: &Filters) -> (f64, f64, f64) {
    let clamp = |v: f32| (v as f64).clamp(0.1, 10.0);
    match &f.timescale {
        Some(ts) => (
            clamp(ts.speed.unwrap_or(1.0)),
            clamp(ts.pitch.unwrap_or(1.0)),
            clamp(ts.rate.unwrap_or(1.0)),
        ),
        None => (1.0, 1.0, 1.0),
    }
}

#[inline]
fn approx1(v: f64) -> bool {
    (v - 1.0).abs() < 1e-4
}

/// timescale 全体（WSOLA → リサンプル）。
pub struct Timescale {
    enabled: bool,
    wsola: Wsola,
    resampler: RatioResampler,
}

impl Timescale {
    pub fn from_filters(f: &Filters) -> Self {
        let (speed, pitch, rate) = read_params(f);
        let enabled = !(approx1(speed) && approx1(pitch) && approx1(rate));
        Self {
            enabled,
            wsola: Wsola::new(pitch / speed),
            resampler: RatioResampler::new(pitch * rate),
        }
    }

    /// 再生中のフィルタ変更を反映（WSOLA/リサンプルの内部状態は維持）。
    pub fn reconfigure(&mut self, f: &Filters) {
        let (speed, pitch, rate) = read_params(f);
        self.enabled = !(approx1(speed) && approx1(pitch) && approx1(rate));
        self.wsola.set_factor(pitch / speed);
        self.resampler.set_factor(pitch * rate);
    }

    /// 48kHz ステレオ(インターリーブ)を投入し、変換後の 48kHz ステレオを返す。
    pub fn push(&mut self, stereo: &[f32]) -> Vec<f32> {
        if !self.enabled {
            return stereo.to_vec();
        }
        let stretched = self.wsola.push(stereo);
        self.resampler.push(&stretched)
    }

    /// 終端で内部に残った分を吐き出す。
    pub fn flush(&mut self) -> Vec<f32> {
        if !self.enabled {
            return Vec::new();
        }
        let tail = self.wsola.flush();
        self.resampler.push(&tail)
    }
}

// ----------------------------- WSOLA -----------------------------

const WIN: usize = 1024; // 1ch あたりの窓長（≈21ms）
const HOP: usize = 512; // 合成ホップ（窓長の 50%）
const OV: usize = WIN - HOP; // オーバーラップ長（相関に使う）
const SEARCH: usize = 256; // 類似度探索範囲 ±SEARCH

/// バッファ（インターリーブ）上の絶対サンプル位置 `abs`(1ch フレーム) の ch を読む。範囲外は 0。
#[inline]
fn rd(buf: &[f32], origin: i64, abs: i64, ch: usize) -> f32 {
    let rel = abs - origin;
    if rel < 0 {
        return 0.0;
    }
    let idx = rel as usize * 2 + ch;
    if idx < buf.len() {
        buf[idx]
    } else {
        0.0
    }
}

/// WSOLA（Waveform Similarity Overlap-Add）タイムストレッチ。
/// `factor = 出力長 / 入力長`。ピッチは変えずにテンポ（長さ）だけ変える。
struct Wsola {
    ana_hop: f64,   // 解析ホップ = HOP / factor
    buf: Vec<f32>,  // 未処理の入力（ステレオインターリーブ）
    origin: i64,    // buf[0] の絶対 1ch フレーム位置
    ana_pos: f64,   // 解析位置（絶対, フラクショナル）
    prev_pos: i64,  // 直前に採用したグレインの開始位置（絶対）
    acc: Vec<f32>,  // OLA 累積（index0 = 次に出力するサンプル, ステレオ）
    hann: Vec<f32>, // 長さ WIN の周期 Hann 窓（50% OLA で利得 1）
    started: bool,
}

impl Wsola {
    fn new(factor: f64) -> Self {
        let factor = factor.clamp(0.1, 10.0);
        let hann: Vec<f32> = (0..WIN)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / WIN as f32).cos())
            .collect();
        Self {
            ana_hop: HOP as f64 / factor,
            buf: Vec::new(),
            origin: 0,
            ana_pos: 0.0,
            prev_pos: 0,
            acc: Vec::new(),
            hann,
            started: false,
        }
    }

    fn set_factor(&mut self, factor: f64) {
        let factor = factor.clamp(0.1, 10.0);
        self.ana_hop = HOP as f64 / factor;
    }

    fn push(&mut self, stereo: &[f32]) -> Vec<f32> {
        self.buf.extend_from_slice(stereo);
        self.process(false)
    }

    fn flush(&mut self) -> Vec<f32> {
        let mut out = self.process(true);
        // 最後のグレインの尾（acc の残り）を出力。
        out.extend_from_slice(&self.acc);
        self.acc.clear();
        out
    }

    fn process(&mut self, final_flush: bool) -> Vec<f32> {
        let mut out = Vec::new();
        loop {
            let buf_end = self.origin + (self.buf.len() / 2) as i64; // 排他的上限
            let base = self.ana_pos.round() as i64;
            // 読みに必要な絶対上限（グレイン探索 + ターゲット）。
            let need =
                (base + SEARCH as i64 + WIN as i64).max(self.prev_pos + HOP as i64 + OV as i64);
            if final_flush {
                // 終端: 実入力を越えてグレインを置く必要が出たら終了。
                if base - SEARCH as i64 >= buf_end {
                    break;
                }
            } else if need > buf_end {
                break; // 入力待ち
            }

            // --- 類似度探索: 直前グレインの「自然な続き」に最も近い delta を選ぶ ---
            let mut best_delta = 0i64;
            if self.started {
                let mut best = f32::NEG_INFINITY;
                for d in -(SEARCH as i64)..=(SEARCH as i64) {
                    let s = base + d;
                    let mut score = 0.0f32;
                    for i in 0..OV as i64 {
                        let x = rd(&self.buf, self.origin, s + i, 0)
                            + rd(&self.buf, self.origin, s + i, 1);
                        let y = rd(&self.buf, self.origin, self.prev_pos + HOP as i64 + i, 0)
                            + rd(&self.buf, self.origin, self.prev_pos + HOP as i64 + i, 1);
                        score += x * y;
                    }
                    if score > best {
                        best = score;
                        best_delta = d;
                    }
                }
            }
            let grain_start = base + best_delta;

            // --- 窓掛け OLA ---
            if self.acc.len() < WIN * 2 {
                self.acc.resize(WIN * 2, 0.0);
            }
            for i in 0..WIN {
                let a = grain_start + i as i64;
                let w = self.hann[i];
                self.acc[i * 2] += rd(&self.buf, self.origin, a, 0) * w;
                self.acc[i * 2 + 1] += rd(&self.buf, self.origin, a, 1) * w;
            }
            // 確定分（先頭 HOP フレーム）を出力。
            out.extend_from_slice(&self.acc[0..HOP * 2]);
            self.acc.drain(0..HOP * 2);

            self.prev_pos = grain_start;
            self.started = true;
            self.ana_pos += self.ana_hop;

            // もう読まない古い入力を破棄（探索/前グレインに必要な範囲は残す）。
            let keep = (self.prev_pos.min(base) - SEARCH as i64 - 2).max(self.origin);
            if keep > self.origin {
                let drop = (keep - self.origin) as usize;
                self.buf.drain(0..drop * 2);
                self.origin = keep;
            }
        }
        out
    }
}

// --------------------------- リサンプル ---------------------------

/// 係数 `factor`（出力ピッチ倍率）で線形リサンプル。出力長 = 入力長 / factor。
/// チャンク境界をまたいで連続性を保つストリーミング実装。
struct RatioResampler {
    factor: f64,
    pending: Vec<f32>, // 未消費入力（ステレオインターリーブ）
    pos: f64,          // pending 内のフラクショナル読み位置（1ch フレーム）
}

impl RatioResampler {
    fn new(factor: f64) -> Self {
        Self { factor: factor.clamp(0.1, 10.0), pending: Vec::new(), pos: 0.0 }
    }

    fn set_factor(&mut self, factor: f64) {
        self.factor = factor.clamp(0.1, 10.0);
    }

    fn push(&mut self, stereo: &[f32]) -> Vec<f32> {
        self.pending.extend_from_slice(stereo);
        let in_frames = self.pending.len() / 2;
        let mut out = Vec::new();
        loop {
            let idx = self.pos.floor() as usize;
            if idx + 1 >= in_frames {
                break; // 線形補間に次サンプルが必要。
            }
            let frac = (self.pos - idx as f64) as f32;
            for ch in 0..2 {
                let a = self.pending[idx * 2 + ch];
                let b = self.pending[(idx + 1) * 2 + ch];
                out.push(a + (b - a) * frac);
            }
            self.pos += self.factor;
        }
        // 消費済みフレームを破棄（補間連続性のため floor まで残す）。
        let consumed = (self.pos.floor() as usize).min(in_frames);
        if consumed > 0 {
            self.pending.drain(0..consumed * 2);
            self.pos -= consumed as f64;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lavalink_protocol::Timescale as TsDto;

    fn filters_with(speed: f32, pitch: f32, rate: f32) -> Filters {
        Filters {
            timescale: Some(TsDto { speed: Some(speed), pitch: Some(pitch), rate: Some(rate) }),
            ..Default::default()
        }
    }

    /// 入力（連続した 48kHz ステレオ）を 1 回でまとめて流し、flush までの総出力長を得る。
    fn run(ts: &mut Timescale, in_frames: usize) -> usize {
        // 440Hz 風のなだらかな信号。
        let mut data = Vec::with_capacity(in_frames * 2);
        for n in 0..in_frames {
            let v = (n as f32 * 0.01).sin() * 0.5;
            data.push(v);
            data.push(v);
        }
        let mut out = ts.push(&data);
        out.extend(ts.flush());
        out.len() / 2
    }

    #[test]
    fn identity_when_all_one() {
        let mut ts = Timescale::from_filters(&filters_with(1.0, 1.0, 1.0));
        let n = run(&mut ts, 48_000);
        assert_eq!(n, 48_000, "no timescale must pass through unchanged");
    }

    #[test]
    fn speed_double_halves_duration() {
        let mut ts = Timescale::from_filters(&filters_with(2.0, 1.0, 1.0));
        let n = run(&mut ts, 48_000);
        // speed=2 → 出力長 ≒ 半分（WSOLA の端数で多少前後する）。
        assert!((20_000..=28_000).contains(&n), "got {n}");
    }

    #[test]
    fn pitch_double_keeps_duration() {
        let mut ts = Timescale::from_filters(&filters_with(1.0, 2.0, 1.0));
        let n = run(&mut ts, 48_000);
        // pitch=2 → テンポ不変なので長さはほぼ同じ。
        assert!((42_000..=54_000).contains(&n), "got {n}");
    }

    #[test]
    fn rate_double_halves_duration() {
        let mut ts = Timescale::from_filters(&filters_with(1.0, 1.0, 2.0));
        let n = run(&mut ts, 48_000);
        assert!((20_000..=28_000).contains(&n), "got {n}");
    }
}
