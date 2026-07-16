//! DC removal + adaptive high-pass (03 §4.1).
//!
//! A 1st-order DC blocker strips any constant offset; a 2nd-order Butterworth high-pass
//! removes sub-sonic rumble/plosive energy. The cutoff is chosen up-stream by the
//! auto-decision (speech → 80 Hz, music present → 40 Hz, rumble → up to 120 Hz); this
//! module just applies the resolved cutoff so it stays a pure, deterministic `Processor`.

use anvil_media::AudioBuffer;
use serde::{Deserialize, Serialize};

use crate::biquad::Biquad;
use crate::Processor;

/// High-pass mode, surfaced in the report so a user sees why a cutoff was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HpfMode {
    /// Cutoff chosen from the analysis (the resolved value lives in [`DcHpfConfig::cutoff_hz`]).
    Auto,
    /// A user-forced cutoff in Hz.
    Fixed,
    /// High-pass disabled (DC blocker may still run).
    Off,
}

/// Resolved DC/HPF configuration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DcHpfConfig {
    /// How the cutoff was decided (report/UX only).
    pub mode: HpfMode,
    /// The high-pass corner in Hz (ignored when `mode == Off`).
    pub cutoff_hz: f32,
    /// Run the 1st-order DC blocker (03: "always cheap-on").
    pub dc_block: bool,
}

impl Default for DcHpfConfig {
    fn default() -> Self {
        Self {
            mode: HpfMode::Auto,
            cutoff_hz: 80.0,
            dc_block: true,
        }
    }
}

/// One-pole DC blocker coefficient: `y[n] = x[n] − x[n−1] + R·y[n−1]`. R = 0.9975 puts the
/// corner near 19 Hz @ 48 kHz — below the musical range but above true DC (Julius O. Smith,
/// "Introduction to Digital Filters", the DC-blocker one-pole).
const DC_BLOCK_R: f32 = 0.9975;

/// Per-channel DC-blocker state.
#[derive(Debug, Clone, Copy, Default)]
struct DcState {
    x_prev: f32,
    y_prev: f32,
}

/// DC removal + adaptive high-pass processor.
#[derive(Debug, Clone)]
pub struct DcHpf {
    config: DcHpfConfig,
    sample_rate: f32,
    dc: Vec<DcState>,
    hpf: Vec<Biquad>,
}

impl DcHpf {
    /// Build for `channels` channels at `sample_rate` with `config`.
    pub fn new(channels: usize, sample_rate: u32, config: DcHpfConfig) -> Self {
        let sample_rate = sample_rate as f32;
        let hpf = (0..channels.max(1))
            .map(|_| {
                Biquad::highpass(
                    sample_rate,
                    config.cutoff_hz,
                    std::f32::consts::FRAC_1_SQRT_2,
                )
            })
            .collect();
        Self {
            config,
            sample_rate,
            dc: vec![DcState::default(); channels.max(1)],
            hpf,
        }
    }

    /// The resolved config.
    pub fn config(&self) -> DcHpfConfig {
        self.config
    }
}

impl Processor for DcHpf {
    fn process(&mut self, buffer: &mut AudioBuffer) {
        let hpf_on = self.config.mode != HpfMode::Off;
        for (ch_idx, channel) in buffer.planar_mut().iter_mut().enumerate() {
            if ch_idx >= self.dc.len() {
                self.dc.push(DcState::default());
                self.hpf.push(Biquad::highpass(
                    self.sample_rate,
                    self.config.cutoff_hz,
                    std::f32::consts::FRAC_1_SQRT_2,
                ));
            }
            let dc = &mut self.dc[ch_idx];
            let hpf = &mut self.hpf[ch_idx];
            for sample in channel.iter_mut() {
                let mut x = *sample;
                if self.config.dc_block {
                    let y = x - dc.x_prev + DC_BLOCK_R * dc.y_prev;
                    dc.x_prev = x;
                    dc.y_prev = y;
                    x = y;
                }
                if hpf_on {
                    x = hpf.process(x);
                }
                *sample = x;
            }
        }
    }

    fn reset(&mut self) {
        for d in &mut self.dc {
            *d = DcState::default();
        }
        for h in &mut self.hpf {
            h.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn tone(freq: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (i as f32 * freq * TAU / 48_000.0).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|&s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn removes_dc_offset() {
        let mut buf = AudioBuffer::from_planar(vec![vec![0.5; 48_000]], 48_000);
        DcHpf::new(1, 48_000, DcHpfConfig::default()).process(&mut buf);
        // After settling, the constant 0.5 offset is gone.
        let mean: f32 = buf.channel(0)[24_000..].iter().sum::<f32>() / 24_000.0;
        assert!(mean.abs() < 1e-3, "residual DC {mean}");
    }

    #[test]
    fn attenuates_30hz_passes_1khz() {
        let mut low = AudioBuffer::from_planar(vec![tone(30.0, 48_000)], 48_000);
        let mut high = AudioBuffer::from_planar(vec![tone(1000.0, 48_000)], 48_000);
        let before_low = rms(&low.channel(0)[24_000..]);
        let before_high = rms(&high.channel(0)[24_000..]);

        DcHpf::new(1, 48_000, DcHpfConfig::default()).process(&mut low);
        DcHpf::new(1, 48_000, DcHpfConfig::default()).process(&mut high);

        assert!(rms(&low.channel(0)[24_000..]) / before_low < 0.3);
        assert!(rms(&high.channel(0)[24_000..]) / before_high > 0.9);
    }

    #[test]
    fn off_mode_passes_tone_but_still_blocks_dc() {
        let cfg = DcHpfConfig {
            mode: HpfMode::Off,
            cutoff_hz: 80.0,
            dc_block: true,
        };
        // 200 Hz is well above both the 80 Hz HPF and the ~19 Hz DC-blocker corner, so with
        // the HPF off it should pass essentially untouched.
        let mut buf = AudioBuffer::from_planar(vec![tone(200.0, 48_000)], 48_000);
        let before = rms(&buf.channel(0)[24_000..]);
        DcHpf::new(1, 48_000, cfg).process(&mut buf);
        assert!(rms(&buf.channel(0)[24_000..]) / before > 0.95);
    }
}
