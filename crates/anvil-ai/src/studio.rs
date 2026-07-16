//! **Studio** — the heavy, GPU-preferred, chunked-offline tier (03 §4.4, §7).
//!
//! # What the bake-off actually found
//!
//! Spec §4.4 names three candidates: **MossFormer2-SE-48K** (Apache-2.0), **resemble-enhance**
//! (MIT), **VoiceFixer** (MIT). Criterion #3 in that list is *ONNX exportability*, and that is
//! where all three fail today: none of them publishes an ONNX export, and the upstream
//! artifacts are PyTorch checkpoints
//! (`alibabasglab/MossFormer2_SE_48K` → `last_best_checkpoint.pt`; `ResembleAI/resemble-enhance`
//! → DeepSpeed `mp_rank_00_model_states.pt`; VoiceFixer → `.ckpt`). Exporting any of them means
//! standing up their training-time Python stacks and hand-porting graph ops that do not export
//! cleanly (MossFormer2's FSMN + attention, resemble-enhance's CFM sampler + UnivNet vocoder).
//! That is a research task, not a time-boxed one, and shipping a *broken* Studio tier is worse
//! than shipping an honest one. See the crate-level notes and the final report.
//!
//! # What ships instead — and what it is not
//!
//! Be clear about this: **today the Studio tier is not a better denoiser than Standard.** What
//! it is, is the whole rig a heavier model needs, built and proven, with DFN3 in the socket:
//!
//! - **DFN3 at full suppression** — Studio ignores a timid `strength` and keeps the attenuation
//!   limit inert, because you only reach for Studio when the audio is already bad.
//! - **Chunked at 8 s windows / 1 s crossfaded overlap** (§4.4), with the boundary guarantee
//!   proven by a spectral-discontinuity test. This is what keeps memory flat on a 3 h file and
//!   is exactly what a heavy model will need.
//! - **GPU EP (DirectML on Windows, CoreML on macOS) with an automatic CPU fallback**: a canary
//!   forward pass runs on the GPU at session build; a driver that will not load, will not run, or
//!   returns garbage downgrades us to CPU instead of crashing (07 "GPU driver zoo"). See
//!   [`Studio::new`] for why the *default* is CPU today on every platform.
//! - **Optional late-reverberation suppression** ([`crate::dereverb`]), default off — see
//!   [`DEFAULT_DEREVERB`] for the measurement that put it there.
//!
//! Only the inner enhancer changes when a Studio-grade ONNX export exists. The seam is ready.
//!
//! # Speech / noise / music sliders
//!
//! §4.4 wants separated stem gains. DFN3 is a masker, not a separator — it has no music stem —
//! so those sliders are **not exposed**. Faking them (e.g. by band-limiting the mask) would be a
//! lie in the UI. Stated honestly rather than shimmed.

use crate::dfn3::{Dfn3, Dfn3Params, MAX_ATTEN_DB};
use crate::stft::SR;
use crate::{AiError, DenoiseConfig, Device};

/// Chunk length, samples (8 s @ 48 kHz — spec §4.4).
pub const CHUNK: usize = 8 * SR;
/// Crossfaded overlap between consecutive chunks, samples (1 s — spec §4.4).
pub const OVERLAP: usize = SR;
/// Distance between chunk starts.
const HOP: usize = CHUNK - OVERLAP;

/// Studio configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StudioParams {
    /// Maximum noise attenuation, dB (the model's native control). Studio defaults to inert.
    pub atten_lim_db: f32,
    /// Late-reverberation suppression, 0..1.
    pub dereverb: f32,
    /// Valin post-filter beta.
    pub post_filter_beta: f32,
}

/// Dereverb default. **Off**, and that is a measured decision, not an oversight.
///
/// [`crate::dereverb`] works — its unit test pulls a synthetic RT60 = 0.8 s tail down by more
/// than half. But run against the corpus it is *neutral at best*: on a reverb + noise fixture
/// (RT60 0.79 s) it moved DNSMOS by SIG +1.02 / OVRL +0.75 with it on versus SIG +1.07 /
/// OVRL +0.78 with it off. DNSMOS P.835 scores background *noise* and speech distortion; it has
/// no reverb axis, so it cannot reward dereverberation and can only bill us for the speech it
/// costs. Turning it on to chase a number it cannot see would be backwards.
///
/// It ships off, behind a knob, until there is a reverb-specific metric (SRMR or C50) in the
/// eval harness to tune it against. See the final report.
pub const DEFAULT_DEREVERB: f32 = 0.0;

impl Default for StudioParams {
    fn default() -> Self {
        Self {
            atten_lim_db: MAX_ATTEN_DB,
            dereverb: DEFAULT_DEREVERB,
            // Also off. The Valin post-filter is *available* (03 §4.4 lists it) but measured it
            // costs ~0.06 DNSMOS SIG and buys ~0.00 BAK — it sharpens a gain curve DFN3 has
            // already sharpened.
            post_filter_beta: 0.0,
        }
    }
}

impl From<DenoiseConfig> for StudioParams {
    /// Studio is chosen precisely *because* the audio is bad, so it does not let a timid
    /// `strength` weaken the denoise: attenuation stays at the model's full suppression. That is
    /// the one thing Studio does differently from Standard today.
    fn from(_cfg: DenoiseConfig) -> Self {
        Self::default()
    }
}

impl From<StudioParams> for Dfn3Params {
    fn from(p: StudioParams) -> Self {
        Dfn3Params {
            atten_lim_db: p.atten_lim_db,
            post_filter_beta: p.post_filter_beta,
            dereverb: p.dereverb,
        }
    }
}

/// The Studio enhancer.
#[derive(Debug)]
pub struct Studio {
    inner: Dfn3,
    params: StudioParams,
}

impl Studio {
    /// Build the Studio enhancer.
    ///
    /// # Device policy (measured, not assumed)
    ///
    /// §7 makes Studio "GPU-preferred", and both GPU paths are fully wired
    /// ([`Studio::with_params_on_gpu`], canary fallback and all) because that is what a heavy
    /// model will need. But the enhancer we actually ship today is DFN3, and DFN3 is **faster
    /// on the CPU on every GPU EP we have measured**. Its graph is a pile of small GRU and
    /// grouped-linear ops, so it is dispatch-bound rather than compute-bound and the GPU
    /// round-trip is pure loss:
    ///
    /// - **Windows / DirectML:** 39x realtime on CPU against 0.96x on DirectML (STATE.md: "75x
    ///   slower").
    /// - **macOS / CoreML (M6.S5, measured on this M-series, arm64, release, 60 s fixture):**
    ///   CPU ~167x vs CoreML ~175x realtime — a ~1.04x delta that is inside measurement noise,
    ///   because with `with_static_input_shapes(true)` CoreML declines the dynamic-`S` recurrent
    ///   graph and the CPU EP runs it anyway (output stays bit-identical, corr = 1.000000; see
    ///   `dfn3::tests::coreml_vs_cpu_rtf` / `coreml_output_tracks_cpu_within_tolerance`). Letting
    ///   CoreML take dynamic shapes measured ~0.75x, i.e. slower still.
    ///
    /// The default-device rule is "**GPU only if it is >= 1.3x faster than CPU**". Neither EP
    /// clears it, so the default is CPU on every platform — the fastest thing that meets the
    /// budget — and the GPU path stays one call away ([`Studio::with_params_on_gpu`] →
    /// [`crate::probe_device`]) for the model it was built for. On macOS, flip the (probe-honored)
    /// device the same way the Windows "Settings > GPU on/off" toggle will: the probe reports
    /// CoreML as *available* and `with_params_on_gpu` opts into it; `Studio::new` stays CPU.
    /// Choosing the slower device just to be able to say "GPU" would be a worse product.
    pub fn new(config: DenoiseConfig) -> Result<Self, AiError> {
        Self::with_params(StudioParams::from(config))
    }

    /// Studio on the CPU (the default — see [`Studio::new`]).
    pub fn with_params(params: StudioParams) -> Result<Self, AiError> {
        Self::on(params, Device::Cpu)
    }

    /// Studio on the best GPU the probe finds (DirectML on Windows, CoreML on macOS), with an
    /// automatic CPU fallback: if the EP will not load, or the canary forward pass fails or
    /// returns garbage, we silently come back to CPU rather than crash (07 "GPU driver zoo").
    /// [`Studio::device`] reports what we got. This is the opt-in seam the "Settings > GPU
    /// on/off" toggle (04 §S8) drives; [`Studio::new`] stays on CPU because the measurements in
    /// its doc show no GPU EP beats CPU for DFN3 today.
    pub fn with_params_on_gpu(params: StudioParams) -> Result<Self, AiError> {
        Self::on(params, crate::probe_device())
    }

    /// Force a specific device.
    pub fn on(params: StudioParams, device: Device) -> Result<Self, AiError> {
        let inner = Dfn3::with_device(params.into(), device)?;
        tracing::info!(requested = ?device, actual = ?inner.device(), "Studio tier ready");
        Ok(Self { inner, params })
    }

    /// Studio on the CPU. Kept as an explicit alias for tests that must not touch a GPU.
    pub fn with_params_on_cpu(params: StudioParams) -> Result<Self, AiError> {
        Self::on(params, Device::Cpu)
    }

    /// The device the enhancer actually ended up on.
    pub fn device(&self) -> Device {
        self.inner.device()
    }

    pub fn params(&self) -> StudioParams {
        self.params
    }

    /// Override the post-filter beta and/or the dereverb amount (eval-harness escape hatch).
    pub fn tune(&mut self, post_filter_beta: Option<f32>, dereverb: Option<f32>) {
        if let Some(b) = post_filter_beta {
            self.params.post_filter_beta = b;
        }
        if let Some(r) = dereverb {
            self.params.dereverb = r;
        }
        self.inner.set_params(self.params.into());
    }

    /// Enhance one 48 kHz mono channel in place, chunked at 8 s / 1 s crossfaded overlap.
    ///
    /// Chunking is what makes a heavy model tractable on a 3 h file, and it is also the thing
    /// most likely to leave an audible seam. Two properties keep it clean:
    ///
    /// 1. The 1 s overlap is longer than the model's feature-normalisation time constant
    ///    (`norm_tau` = 1 s), so by the time a chunk's output is being faded *in*, its running
    ///    means have converged — the cold-start transient lives entirely inside the fade.
    /// 2. The crossfade is **linear**, not equal-power: the two chunks are enhanced versions of
    ///    the *same* audio and therefore highly correlated, so amplitudes add and a linear ramp
    ///    is the level-preserving one. An equal-power ramp would bump +3 dB at every seam.
    ///
    /// Verified by `chunk_boundaries_leave_no_spectral_discontinuity`.
    pub fn process_channel(&mut self, samples: &mut [f32]) -> Result<(), AiError> {
        let len = samples.len();
        if len == 0 {
            return Ok(());
        }
        if len <= CHUNK {
            return self.inner.process_channel(samples);
        }

        let mut out = vec![0f32; len];
        let mut weight = vec![0f32; len];
        let mut start = 0usize;
        loop {
            let end = (start + CHUNK).min(len);
            let mut chunk = samples[start..end].to_vec();
            self.inner.process_channel(&mut chunk)?;

            let n = chunk.len();
            for (i, &s) in chunk.iter().enumerate() {
                // Ramp in over the leading overlap (unless this is the first chunk) and out over
                // the trailing one (unless this is the last).
                let w_in = if start > 0 && i < OVERLAP {
                    (i as f32 + 0.5) / OVERLAP as f32
                } else {
                    1.0
                };
                let tail_start = n.saturating_sub(OVERLAP);
                let w_out = if end < len && i >= tail_start {
                    1.0 - ((i - tail_start) as f32 + 0.5) / OVERLAP as f32
                } else {
                    1.0
                };
                let w = w_in * w_out;
                out[start + i] += s * w;
                weight[start + i] += w;
            }

            if end == len {
                break;
            }
            start += HOP;
        }

        for (o, w) in out.iter_mut().zip(weight.iter()) {
            if *w > 1e-6 {
                *o /= *w; // exact reconstruction even where the ramps do not sum to 1
            }
        }
        samples.copy_from_slice(&out);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stft::{FFT_SIZE, HOP_SIZE};

    fn noisy_speech_like(len: usize) -> Vec<f32> {
        let mut seed = 0xC0FF_EE01u32;
        (0..len)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                let t = i as f32 / SR as f32;
                // A crude voiced signal: f0 + harmonics, amplitude-modulated like syllables.
                let env = (1.0 + (t * 6.0 * std::f32::consts::TAU).sin()) * 0.5;
                let voice = (t * 120.0 * std::f32::consts::TAU).sin() * 0.5
                    + (t * 240.0 * std::f32::consts::TAU).sin() * 0.25
                    + (t * 480.0 * std::f32::consts::TAU).sin() * 0.12;
                0.35 * env * voice + 0.06 * noise
            })
            .collect()
    }

    #[test]
    fn studio_is_length_preserving_across_many_chunks() {
        let len = 20 * SR; // 20 s -> 3 chunks
        let mut x = noisy_speech_like(len);
        let mut s = Studio::with_params_on_cpu(StudioParams::default()).unwrap();
        s.process_channel(&mut x).unwrap();
        assert_eq!(x.len(), len);
        assert!(x.iter().all(|v| v.is_finite()));
    }

    /// 06 §2 "Cut artifacts: spectral discontinuity at cut points (frame-boundary flux z-score),
    /// 0 outliers over threshold". Same metric, applied to the chunk seams: if the crossfade
    /// were wrong, the frames straddling a boundary would show a flux spike well outside the
    /// distribution of every other frame.
    #[test]
    fn chunk_boundaries_leave_no_spectral_discontinuity() {
        let len = 25 * SR; // 25 s -> 4 chunks, 3 seams
        let mut x = noisy_speech_like(len);
        let mut s = Studio::with_params_on_cpu(StudioParams::default()).unwrap();
        s.process_channel(&mut x).unwrap();

        let flux = spectral_flux(&x);
        let n = flux.len() as f32;
        let mean = flux.iter().sum::<f32>() / n;
        let sd = (flux.iter().map(|f| (f - mean).powi(2)).sum::<f32>() / n).sqrt();
        assert!(sd > 0.0, "degenerate signal");

        // Seams sit where consecutive chunks stop overlapping: HOP, 2·HOP, ...
        let mut seam_frames = Vec::new();
        let mut start = HOP;
        while start < len {
            let f = start / HOP_SIZE;
            // The seam region is the whole 1 s crossfade; check every frame in it.
            for k in f.saturating_sub(2)..(f + OVERLAP / HOP_SIZE + 2).min(flux.len()) {
                seam_frames.push(k);
            }
            start += HOP;
        }
        assert!(!seam_frames.is_empty());

        for &k in &seam_frames {
            let z = (flux[k] - mean) / sd;
            assert!(
                z < 6.0,
                "spectral-flux outlier at chunk seam: frame {k}, z = {z:.2}"
            );
        }
    }

    /// The crossfade must reconstruct **exactly** — a linear ramp, not an equal-power one, since
    /// the overlapping chunks are enhanced versions of the same audio and their amplitudes add.
    ///
    /// Tested with the model in passthrough (a 0 dB attenuation limit is defined as "do
    /// nothing"), so the only thing acting on the signal is the chunking. Any windowing bug —
    /// a +3 dB equal-power bump, a gap, a double-count — shows up immediately. Pushing a real
    /// signal through the model instead would prove nothing: DFN3 would reshape it and swamp the
    /// thing under test.
    #[test]
    fn crossfade_reconstructs_exactly() {
        let len = 18 * SR + 1234; // 3 chunks, and deliberately not a whole number of them
        let x0: Vec<f32> = (0..len)
            .map(|i| {
                let t = i as f32 / SR as f32;
                (t * 300.0 * std::f32::consts::TAU).sin() * 0.3
                    + (t * 47.0 * std::f32::consts::TAU).sin() * 0.2
            })
            .collect();
        let mut x = x0.clone();
        let mut s = Studio::with_params_on_cpu(StudioParams {
            atten_lim_db: 0.0, // passthrough
            dereverb: 0.0,
            post_filter_beta: 0.0,
        })
        .unwrap();
        s.process_channel(&mut x).unwrap();

        let err = x
            .iter()
            .zip(x0.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(
            err < 1e-6,
            "chunked passthrough is not exact: max error {err}"
        );

        // And specifically across every seam, in RMS terms.
        let rms = |r: std::ops::Range<usize>| {
            let sl = &x[r];
            (sl.iter().map(|v| v * v).sum::<f32>() / sl.len() as f32).sqrt()
        };
        let inside = rms(2 * SR..3 * SR);
        let seam = rms(HOP..HOP + OVERLAP);
        let ratio_db = 20.0 * (seam / inside.max(1e-9)).log10();
        assert!(
            ratio_db.abs() < 0.5,
            "level moved {ratio_db:.2} dB through the seam (equal-power ramp bug?)"
        );
    }

    /// Rough spectral flux per frame: L2 distance between successive magnitude spectra.
    fn spectral_flux(x: &[f32]) -> Vec<f32> {
        let mut planner = realfft::RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let mut scratch = fft.make_scratch_vec();
        let frames = x.len() / HOP_SIZE;
        let mut prev: Option<Vec<f32>> = None;
        let mut flux = Vec::with_capacity(frames);
        for t in 0..frames {
            let lo = t * HOP_SIZE;
            let hi = (lo + FFT_SIZE).min(x.len());
            let mut buf = fft.make_input_vec();
            buf[..hi - lo].copy_from_slice(&x[lo..hi]);
            let mut spec = fft.make_output_vec();
            fft.process_with_scratch(&mut buf, &mut spec, &mut scratch)
                .unwrap();
            let mag: Vec<f32> = spec.iter().map(|c| c.norm()).collect();
            let f = match &prev {
                Some(p) => p
                    .iter()
                    .zip(mag.iter())
                    .map(|(a, b)| (a - b).powi(2))
                    .sum::<f32>()
                    .sqrt(),
                None => 0.0,
            };
            flux.push(f);
            prev = Some(mag);
        }
        flux
    }
}
