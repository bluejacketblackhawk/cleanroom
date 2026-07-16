//! **RNNoise** — the Fast-tier denoiser (03 §4.4 "Fast: RNNoise ... realtime on anything").
//!
//! Pure-Rust reimplementation (`nnnoiseless`, MIT) with the weights compiled in, so the Fast
//! tier needs no model file and no network. It is a fixed-strength model: the network decides
//! its own per-band gains and there is no attenuation control to turn down. That is fine for
//! what Fast is *for* (previews, weak machines) and it is exactly why it must not be the
//! Standard tier — the only way to make RNNoise "gentler" is to blend the noisy signal back
//! in, and that is what sank the 06 §2 gate.
//!
//! We therefore run RNNoise at full strength and expose the same `strength` knob honestly: it
//! selects between full suppression and a bounded mix-back, and the reported max attenuation
//! is the mix-back's, not a claim about the model.
//!
//! Determinism (ADR-003): feed-forward + GRU, no entropy source, so identical input and config
//! give bit-identical output.

use nnnoiseless::DenoiseState;

/// RNNoise takes `f32` scaled to `i16` range, not `[-1, 1]`.
const I16_SCALE: f32 = 32_768.0;
/// One RNNoise frame: 480 samples = 10 ms @ 48 kHz (same as [`anvil_core::HOP_SAMPLES`]).
const FRAME: usize = DenoiseState::FRAME_SIZE;

/// Per-channel RNNoise state.
pub struct RnNoise {
    states: Vec<Box<DenoiseState<'static>>>,
}

impl std::fmt::Debug for RnNoise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RnNoise")
            .field("channels", &self.states.len())
            .finish()
    }
}

impl RnNoise {
    pub fn new(channels: usize) -> Self {
        Self {
            states: (0..channels.max(1)).map(|_| DenoiseState::new()).collect(),
        }
    }

    pub fn reset(&mut self) {
        for state in &mut self.states {
            *state = DenoiseState::new();
        }
    }

    /// Denoise one channel in place, mixing the dry signal back at `1 - wet`.
    ///
    /// `wet` is the *only* control RNNoise has. At 1.0 this is the model's full suppression;
    /// below that, `1 - wet` of the untouched noisy signal is passed through by construction,
    /// which bounds the achievable attenuation at `-20·log10(1 - wet)` dB.
    pub fn process_channel(&mut self, channel_index: usize, samples: &mut [f32], wet: f32) {
        while self.states.len() <= channel_index {
            self.states.push(DenoiseState::new());
        }
        let state = &mut self.states[channel_index];

        let mut scaled_in = [0.0f32; FRAME];
        let mut denoised = [0.0f32; FRAME];
        let len = samples.len();
        let mut i = 0;
        while i < len {
            let n = (len - i).min(FRAME);
            for (k, s) in scaled_in.iter_mut().enumerate() {
                *s = if k < n {
                    samples[i + k] * I16_SCALE
                } else {
                    0.0
                };
            }
            state.process_frame(&mut denoised, &scaled_in);
            for k in 0..n {
                let dry = samples[i + k];
                let wet_sample = denoised[k] / I16_SCALE;
                samples[i + k] = dry + wet * (wet_sample - dry);
            }
            i += n;
        }
    }
}
