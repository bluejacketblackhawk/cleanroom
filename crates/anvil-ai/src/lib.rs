//! # anvil-ai
//!
//! Model runtime (ADR-004). Three denoise tiers behind one seam (03 §4.4):
//!
//! - **Fast** — RNNoise (`nnnoiseless`), weights compiled in. Previews, weak machines.
//! - **Standard (default)** — **DeepFilterNet3** on ONNX Runtime, model baked into the binary.
//!   Driven by the model's native `atten_lim_db` control, *not* a wet/dry blend.
//! - **Studio** — DFN3 at full suppression + a late-reverberation suppressor, run on the GPU
//!   (DirectML) where one is usable and chunked into 8 s / 1 s-crossfaded windows.
//!
//! Everything is airplane-mode: no model is fetched at run time, ever. The DFN3 ONNX bundle is
//! provisioned once at *build* time (hash-verified, `build.rs`) and `include_bytes!`d, so there
//! is nothing to install and nothing to download.

use serde::{Deserialize, Serialize};

pub mod denoise;
pub mod dereverb;
pub mod dfn3;
pub mod rnnoise;
pub mod stft;
pub mod studio;

pub use denoise::{DenoiseConfig, Denoiser};
pub use dfn3::{Dfn3, Dfn3Params};
pub use studio::{Studio, StudioParams};

/// Anything that can go wrong in the model runtime.
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// The ONNX Runtime itself failed (session build, allocation, inference).
    #[error("ONNX Runtime error: {0}")]
    Ort(String),
    /// The model bundle is missing or malformed.
    #[error("model error: {0}")]
    Model(String),
}

/// `ort::Error` is generic over a recovery payload (`ort::Error<SessionBuilder>` and friends);
/// we do not resume from a failed session build, so flatten every flavour to its message.
impl<T> From<ort::Error<T>> for AiError {
    fn from(e: ort::Error<T>) -> Self {
        AiError::Ort(e.to_string())
    }
}

/// Which denoise model runs (03 §4.4). Mirrors `anvil_project::Tier` without depending on it
/// (that dependency would point the wrong way); the chain maps one onto the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenoiseTier {
    /// RNNoise. Realtime on anything; light artifacts acceptable.
    Fast,
    /// DeepFilterNet3. The one-click path.
    #[default]
    Standard,
    /// Heavy, GPU-preferred, chunked offline.
    Studio,
}

/// Execution device chosen per model after the capability probe. CPU is the always-available
/// baseline and must be good on its own; GPU paths accelerate the Studio tier where available.
/// A first-inference canary downgrades to CPU on driver failure so we never crash on a GPU
/// (07 risk: "GPU driver zoo").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Device {
    /// CPU (baseline, always available).
    Cpu,
    /// Windows DirectML EP (NVIDIA/AMD/Intel).
    DirectMl,
    /// macOS CoreML EP.
    CoreMl,
}

/// Probe the machine and return the best available device.
///
/// This only answers "is the EP registered and loadable"; it does not prove the driver will
/// survive an inference. That is what the Studio tier's canary is for — it runs a real forward
/// pass on the GPU at session build and falls back to CPU if anything goes wrong.
pub fn probe_device() -> Device {
    #[cfg(windows)]
    {
        use ort::execution_providers::{DirectMLExecutionProvider, ExecutionProvider};
        if DirectMLExecutionProvider::default()
            .is_available()
            .unwrap_or(false)
        {
            return Device::DirectMl;
        }
    }
    #[cfg(target_os = "macos")]
    {
        // CoreML availability probe, mirroring the DirectML one above. `is_available()` only
        // answers "was ONNX Runtime compiled with the CoreML EP" — true for the static
        // aarch64 archive built with the `coreml` feature, and for a `load-dynamic` Intel build
        // whose bundled onnxruntime dylib carries CoreML. It does NOT prove the driver survives
        // an inference; that is the Studio canary's job (`Dfn3::with_device`), exactly as on
        // Windows. Availability != commit: the default tiers still build on CPU (see
        // `Dfn3::new` / `Studio::new`); CoreML is only *reached* through `with_params_on_gpu`.
        //
        // Uses the canonical `ort::ep` path rather than the `ort::execution_providers` alias the
        // cfg(windows) block above uses: that alias module is `#[deprecated]`, and this branch —
        // unlike the Windows one — is compiled on the mac host, where the clippy `-D warnings`
        // gate would reject a deprecated import.
        use ort::ep::{CoreML, ExecutionProvider};
        if CoreML::default().is_available().unwrap_or(false) {
            return Device::CoreMl;
        }
    }
    Device::Cpu
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_a_usable_device() {
        let d = probe_device();
        assert!(matches!(d, Device::Cpu | Device::DirectMl | Device::CoreMl));
    }

    #[test]
    fn standard_is_the_default_tier() {
        assert_eq!(DenoiseTier::default(), DenoiseTier::Standard);
    }
}
