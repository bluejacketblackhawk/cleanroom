//! Synthetic fixtures for the multitrack tests.
//!
//! Everything the tests assert about — an offset, a drift in ppm, a bleed gain — is *built
//! into* these signals, so the tests can check the estimator against ground truth rather than
//! against itself. No corpus files, no network, no randomness beyond a seeded LCG.

use std::f64::consts::TAU;
use std::path::{Path, PathBuf};

use anvil_media::AudioBuffer;

use crate::align::interpolate;

/// The engine's internal rate.
pub(crate) const SR: u32 = 48_000;

/// Deterministic white noise in −0.5..0.5.
struct Lcg(u32);
impl Lcg {
    fn new(seed: u32) -> Self {
        Self(seed.wrapping_mul(2_654_435_761).max(1))
    }
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) as f32 / (1u32 << 24) as f32 - 0.5
    }
}

/// A speech-like signal: voiced harmonics + a fricative noise band, gated into words with
/// real silence between them, over a low noise floor.
///
/// Broadband (so GCC-PHAT has phase to work with), syllabic (so the VAD has onsets to find),
/// and it actually goes quiet between words (so a noise floor can be estimated at all).
pub(crate) struct Speech {
    /// Word length, seconds.
    pub word: f64,
    /// Gap between words, seconds.
    pub gap: f64,
    /// Time of the first word onset, seconds.
    pub start: f64,
    /// Time the speaker stops, seconds (the noise floor keeps running — a mic that goes to
    /// digital silence is not a mic, and a VAD calibrated on true zeros is a lie).
    pub end: f64,
    /// Peak level of the voiced part, linear.
    pub level: f32,
    /// Noise floor, linear RMS.
    pub floor: f32,
    /// Fundamental, Hz.
    pub f0: f64,
    /// LCG seed.
    pub seed: u32,
}

impl Default for Speech {
    fn default() -> Self {
        Self {
            word: 0.45,
            gap: 0.25,
            start: 0.0,
            end: f64::INFINITY,
            level: 0.35,
            floor: 0.0006, // ≈ −64 dBFS
            f0: 120.0,
            seed: 1,
        }
    }
}

impl Speech {
    /// Render `secs` of it.
    pub fn render(&self, secs: f64) -> Vec<f32> {
        let n = (secs * SR as f64) as usize;
        let mut rng = Lcg::new(self.seed);
        let mut lp = 0.0f32;
        let mut out = Vec::with_capacity(n);
        let period = self.word + self.gap;
        let ramp = 0.02; // 20 ms raised-cosine word edges
        for i in 0..n {
            let t = i as f64 / SR as f64;
            let white = rng.next();
            lp = 0.7 * lp + 0.3 * white; // a little spectral tilt

            let env = if t < self.start || t >= self.end {
                0.0
            } else {
                let phase = (t - self.start) % period;
                if phase >= self.word {
                    0.0
                } else if phase < ramp {
                    (0.5 - 0.5 * (std::f64::consts::PI * phase / ramp).cos()) as f32
                } else if phase > self.word - ramp {
                    (0.5 - 0.5 * (std::f64::consts::PI * (self.word - phase) / ramp).cos()) as f32
                } else {
                    1.0
                }
            };

            // Harmonic stack with a slow pitch glide, plus a fricative band.
            let f0 = self.f0 * (1.0 + 0.03 * (t * 1.7 * TAU).sin());
            let voiced = (0.6 * (t * f0 * TAU).sin()
                + 0.35 * (t * 2.0 * f0 * TAU).sin()
                + 0.2 * (t * 3.0 * f0 * TAU).sin()
                + 0.12 * (t * 5.0 * f0 * TAU).sin()) as f32;
            let sample = env * self.level * (0.7 * voiced + 0.9 * lp) + self.floor * white * 2.0;
            out.push(sample);
        }
        out
    }
}

/// The default speech fixture with a given seed.
pub(crate) fn speechy(secs: f64, seed: u32) -> Vec<f32> {
    Speech {
        seed,
        f0: 100.0 + 17.0 * seed as f64,
        ..Default::default()
    }
    .render(secs)
}

/// A mic's own noise floor: `secs` of quiet white noise at `level` RMS.
pub(crate) fn noise(secs: f64, level: f32, seed: u32) -> Vec<f32> {
    let n = (secs * SR as f64) as usize;
    let mut rng = Lcg::new(seed);
    (0..n).map(|_| rng.next() * level * 3.46).collect()
}

/// `a`, delayed by `delay` samples and scaled by `gain` — the bleed model, and the
/// double-ender model too (same maths, different gain).
pub(crate) fn delayed(a: &[f32], delay: usize, gain: f32) -> Vec<f32> {
    let mut b = vec![0.0f32; a.len()];
    for (i, slot) in b.iter_mut().enumerate() {
        if i >= delay {
            *slot = a[i - delay] * gain;
        }
    }
    b
}

/// `a` as heard by a recorder whose clock is off by `ppm` and which started `delay` samples
/// late: `b[n] = a[(1 − e)·n − delay]`, band-limited-interpolated. This is the exact model
/// [`crate::align`] claims to invert, built independently of the estimator.
pub(crate) fn drifted(a: &[f32], ppm: f64, delay: f64, gain: f32) -> Vec<f32> {
    let e = ppm * 1e-6;
    (0..a.len())
        .map(|n| gain * interpolate(a, (1.0 - e) * n as f64 - delay))
        .collect()
}

/// Add `b` into `a` in place (bleed onto a mic that already has its own voice on it).
pub(crate) fn add(a: &mut [f32], b: &[f32]) {
    for (x, y) in a.iter_mut().zip(b) {
        *x += *y;
    }
}

/// A continuous music-ish bed: a chord plus a slow tremolo, never silent.
pub(crate) fn music(secs: f64, level: f32) -> Vec<f32> {
    let n = (secs * SR as f64) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / SR as f64;
            let trem = 0.85 + 0.15 * (t * 1.3 * TAU).sin();
            let chord = 0.5 * (t * 220.0 * TAU).sin()
                + 0.4 * (t * 277.0 * TAU).sin()
                + 0.35 * (t * 330.0 * TAU).sin()
                + 0.2 * (t * 440.0 * TAU).sin();
            (level as f64 * trem * chord) as f32
        })
        .collect()
}

/// Wrap a mono signal as an [`AudioBuffer`] at 48 kHz.
pub(crate) fn buffer(x: Vec<f32>) -> AudioBuffer {
    AudioBuffer::from_planar(vec![x], SR)
}

/// Seconds → sample index.
pub(crate) fn at(secs: f64) -> usize {
    (secs * SR as f64) as usize
}

/// Write a mono buffer to a 16-bit WAV under the OS temp dir (for the `mix()` file path).
pub(crate) fn write_wav(dir: &Path, name: &str, x: &[f32]) -> PathBuf {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SR,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let path = dir.join(name);
    let mut w = hound::WavWriter::create(&path, spec).expect("create wav");
    for &s in x {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        w.write_sample(v).expect("write sample");
    }
    w.finalize().expect("finalize wav");
    path
}

/// A unique temp directory for one test's fixtures.
pub(crate) fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("anvil-multitrack-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}
