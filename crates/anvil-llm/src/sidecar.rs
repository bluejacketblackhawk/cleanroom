//! llama.cpp sidecar manager.
//!
//! llama.cpp is **never linked** into Cleanroom — no `llama-cpp-2`, no cmake, no bindgen, no
//! libclang, no MSVC C++ link step. It is run as a separate `llama-cli` child process,
//! exactly like [`anvil_asr::WhisperSidecar`] and [`anvil_media::FfmpegSidecar`]. That is a
//! deliberate repeat of the decision ADR-004's neighbours already made: it keeps `cargo
//! build` a pure-Rust seconds-long build on every dev machine and CI runner, keeps GPU
//! backends (Vulkan/Metal/CUDA) a swap of the *binary* rather than a rebuild of Cleanroom, and
//! keeps the GPL/patent surface of a large C++ dependency at arm's length.
//!
//! Airplane-mode (ADR-005 engine invariant): this module **never downloads** anything. The
//! binary must already be present — pointed at by `CLEANROOM_LLAMA`, bundled next to the app, or
//! on `PATH` — and the gguf must already be on disk (see [`crate::model`]). Inference never
//! fetches a model.
//!
//! ## How we invoke it
//! ```text
//! llama-cli -m <model.gguf> -f <prompt file> -c <ctx> -n <max tokens>
//!           --temp <t> --top-p <p> -s <seed> -no-cnv --no-display-prompt --no-warmup
//!           [-t <threads>] [-ngl <gpu layers>]
//! ```
//! - **`-f <file>`, not `-p <string>`**: a chunk of transcript is tens of kilobytes. Windows
//!   caps a command line at ~32k characters, and quoting a transcript through a shell-less
//!   `CreateProcess` is a bug farm. The prompt goes to a temp file, which is deleted after.
//! - **`-no-cnv`**: we render Qwen2.5's ChatML ourselves ([`crate::prompt`]) and want a raw
//!   completion, not llama.cpp's chat wrapper applied on top of ours.
//! - **`--no-display-prompt`**: stdout is then (mostly) just the generation. We still run it
//!   through [`crate::parse::extract_json`], because "mostly" is doing real work in that
//!   sentence — llama.cpp builds vary in what they print.
//!
//! Requires a reasonably current llama.cpp (`-no-cnv` landed in 2024; on older builds it is
//! `--no-conversation`). The build is the user's, not ours — that is the point of a sidecar.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::LlmError;
use crate::pipeline::GenerateOptions;

/// Anything that can turn a prompt into text.
///
/// The pipeline ([`crate::pipeline::generate_with`]) is written against this rather than
/// against the sidecar, so the whole map-reduce + parse + snap path is testable with a
/// scripted completer and no 4.7 GB download. [`LlamaSidecar`] is the only implementation
/// that ships.
pub trait Completer {
    /// Run `prompt` to completion and return the model's raw output (any surrounding chatter
    /// included — the caller extracts the JSON).
    fn complete(&self, prompt: &str, opts: &GenerateOptions) -> Result<String, LlmError>;
}

/// A located `llama-cli` binary, reusable across generations.
#[derive(Debug, Clone)]
pub struct LlamaSidecar {
    binary: PathBuf,
}

impl LlamaSidecar {
    /// Locate `llama-cli` without touching the network. Search order:
    /// 1. `CLEANROOM_LLAMA` environment variable (explicit path),
    /// 2. a bundled sidecar next to the current executable (`llama-cli`, `sidecar/…`,
    ///    `llama/…`),
    /// 3. `llama-cli` on `PATH`.
    ///
    /// Returns [`LlmError::SidecarNotFound`] if none exist. Callers that want graceful
    /// degradation should use [`crate::suggest`], which falls back to the no-LLM path.
    pub fn locate() -> Result<Self, LlmError> {
        for candidate in Self::candidates() {
            if candidate.is_file() {
                return Self::from_path(candidate);
            }
        }
        if let Some(found) = Self::search_path() {
            return Self::from_path(found);
        }
        Err(LlmError::SidecarNotFound(
            "no bundled sidecar, CLEANROOM_LLAMA unset, and llama-cli not on PATH \
             (airplane-mode: Cleanroom never auto-downloads it)"
                .into(),
        ))
    }

    /// Wrap an explicit `llama-cli` path.
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, LlmError> {
        let binary = path.into();
        if !binary.is_file() {
            return Err(LlmError::SidecarNotFound(binary.display().to_string()));
        }
        Ok(Self { binary })
    }

    /// Path of the resolved binary.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    fn exe_name() -> String {
        // `EXE_SUFFIX` is ".exe" on Windows and "" elsewhere — cross-platform without any
        // `#[cfg]` (which the workspace confines to anvil-core::platform).
        format!("llama-cli{}", std::env::consts::EXE_SUFFIX)
    }

    fn candidates() -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(explicit) = std::env::var_os("CLEANROOM_LLAMA") {
            out.push(PathBuf::from(explicit));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let name = Self::exe_name();
                out.push(dir.join(&name));
                out.push(dir.join("sidecar").join(&name));
                out.push(dir.join("llama").join(&name));
            }
        }
        out
    }

    fn search_path() -> Option<PathBuf> {
        let name = Self::exe_name();
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(&name))
            .find(|candidate| candidate.is_file())
    }
}

impl Completer for LlamaSidecar {
    fn complete(&self, prompt: &str, opts: &GenerateOptions) -> Result<String, LlmError> {
        let model = resolve_model(opts)?;
        let prompt_file = temp_path("prompt", "txt");
        std::fs::write(&prompt_file, prompt)?;

        tracing::debug!(
            binary = %self.binary.display(),
            model = %model.display(),
            prompt_tokens = crate::chunk::estimate_tokens(prompt),
            ctx = opts.ctx_tokens,
            "running llama-cli"
        );

        let mut cmd = Command::new(&self.binary);
        cmd.arg("-m")
            .arg(&model)
            .arg("-f")
            .arg(&prompt_file)
            .args(["-c", &opts.ctx_tokens.to_string()])
            .args(["-n", &opts.max_output_tokens.to_string()])
            .args(["--temp", &format!("{:.2}", opts.temperature)])
            .args(["--top-p", &format!("{:.2}", opts.top_p)])
            .args(["-s", &opts.seed.to_string()])
            // We supply ChatML ourselves; give us a raw completion and only the completion.
            .args(["-no-cnv", "--no-display-prompt", "--no-warmup"]);
        if let Some(threads) = opts.threads {
            cmd.args(["-t", &threads.to_string()]);
        }
        if let Some(layers) = opts.gpu_layers {
            cmd.args(["-ngl", &layers.to_string()]);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = cmd.output();
        let _ = std::fs::remove_file(&prompt_file);
        let output = output?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(LlmError::SidecarFailed(format!(
                "llama-cli exited with {}: {}",
                output.status,
                tail(&stderr, 800)
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Resolve the gguf path: explicit `opts.model` wins, then `CLEANROOM_LLM_MODEL`, then the first
/// installed pack in the models dir (7B before 1.5B). Never downloads.
pub fn resolve_model(opts: &GenerateOptions) -> Result<PathBuf, LlmError> {
    if let Some(model) = &opts.model {
        if model.is_file() {
            return Ok(model.clone());
        }
        return Err(LlmError::ModelNotFound(model.display().to_string()));
    }
    if let Some(env) = std::env::var_os("CLEANROOM_LLM_MODEL") {
        let path = PathBuf::from(env);
        if path.is_file() {
            return Ok(path);
        }
        return Err(LlmError::ModelNotFound(path.display().to_string()));
    }
    if let Some(installed) = crate::model::installed_models().into_iter().next() {
        return Ok(installed.path);
    }
    Err(LlmError::ModelNotFound(
        "no model given, CLEANROOM_LLM_MODEL unset, and no Qwen2.5 gguf in the models dir".into(),
    ))
}

/// A unique temp path, e.g. `…/anvil-llm-prompt-1234-987654321.txt`.
fn temp_path(kind: &str, ext: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "anvil-llm-{kind}-{}-{nanos}.{ext}",
        std::process::id()
    ));
    path
}

/// Last `max` chars of `s`, trimmed — keeps a llama stderr tail bounded.
fn tail(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= max {
        return trimmed.to_string();
    }
    let start = trimmed.len() - max;
    let start = (start..trimmed.len())
        .find(|&i| trimmed.is_char_boundary(i))
        .unwrap_or(trimmed.len());
    trimmed[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_binary_is_an_error_not_a_panic() {
        let err = LlamaSidecar::from_path("definitely/not/a/binary").expect_err("must fail");
        assert!(matches!(err, LlmError::SidecarNotFound(_)));
    }

    #[test]
    fn model_resolution_rejects_a_path_that_is_not_there() {
        let opts = GenerateOptions {
            model: Some(PathBuf::from("no/such/model.gguf")),
            ..Default::default()
        };
        let err = resolve_model(&opts).expect_err("must fail");
        assert!(matches!(err, LlmError::ModelNotFound(_)));
    }

    #[test]
    fn the_binary_name_is_platform_correct() {
        let name = LlamaSidecar::exe_name();
        assert!(name.starts_with("llama-cli"));
        assert!(name.ends_with(std::env::consts::EXE_SUFFIX));
    }

    #[test]
    fn temp_paths_are_unique_and_in_the_temp_dir() {
        let a = temp_path("prompt", "txt");
        let b = temp_path("prompt", "txt");
        assert_ne!(a, b);
        assert!(a.starts_with(std::env::temp_dir()));
        assert_eq!(a.extension().unwrap(), "txt");
    }
}
