//! Encoders via the ffmpeg sidecar (ADR-005 §Encode).
//!
//! ffmpeg is never linked here either — the same LGPL sidecar [`FfmpegSidecar`] that decodes
//! also encodes, as a second child process invocation. A planar-f32 [`AudioBuffer`] is
//! interleaved (see [`crate::sidecar::interleave_f32le`]) and piped to ffmpeg's stdin as raw
//! `f32le`; ffmpeg writes the encoded container straight to `out_path`. Progress is parsed
//! from `-progress pipe:2` the same way [`FfmpegSidecar::decode_to_buffer`] parses it on the
//! read side.
//!
//! **AAC note (deviation from ADR-005):** the ADR's original plan was OS-native AAC (Media
//! Foundation on Windows, AudioToolbox on Mac) to sidestep ffmpeg's AAC quality/patent
//! questions. M2 ships ffmpeg's *native* AAC encoder instead — it is patent-clear to invoke
//! (ffmpeg is never linked, only run as a sidecar, so ANVIL carries no AAC IP obligation) and
//! modern ffmpeg's native encoder is good enough for podcast-bitrate use. MF/AudioToolbox
//! AAC remains a later quality refinement (tracked, not blocking M2).
//!
//! **Simultaneous outputs:** [`encode_multi`] takes one already-decoded/mastered
//! [`AudioBuffer`] and writes it to several [`OutputSpec`] targets *from that one in-memory
//! buffer* — no re-decode, no re-render, matching the M2 deliverable ("one pass feeding
//! multiple encoders, or sequential from an in-memory buffer"). The buffer is real memory
//! (produced once by `anvil_dsp::master`), so "sequential from an in-memory buffer" is the
//! right shape here: each target is a fresh, independent ffmpeg child, but none of them ever
//! touch the decoder again.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::error::MediaError;
use crate::metadata::{write_ffmetadata_chapters, Chapter};
use crate::sidecar::{interleave_f32le, progress_fraction, push_tail, FfmpegSidecar};
use crate::AudioBuffer;

/// A target encoding format. Every variant maps to an ffmpeg sidecar codec (never a linked
/// library) per ADR-005: MP3 via `libmp3lame`, Opus via `libopus`, Vorbis via `libvorbis`,
/// FLAC via ffmpeg's native (lossless, bit-exact) encoder, AAC/M4B via ffmpeg's native AAC
/// encoder (see the module-level AAC note).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputFormat {
    Mp3,
    Opus,
    Vorbis,
    Flac,
    Aac,
    /// AAC-in-MP4 muxed with the `ipod` muxer (sets the audiobook `stik` atom) so players
    /// recognize the file as an audiobook. See [`FfmpegSidecar::encode_m4b_audiobook`].
    M4b,
}

impl OutputFormat {
    /// The conventional file extension for this format (without the leading dot).
    pub fn extension(self) -> &'static str {
        match self {
            OutputFormat::Mp3 => "mp3",
            OutputFormat::Opus => "opus",
            OutputFormat::Vorbis => "ogg",
            OutputFormat::Flac => "flac",
            OutputFormat::Aac => "m4a",
            OutputFormat::M4b => "m4b",
        }
    }

    /// Whether this format ignores [`OutputSpec::bitrate_kbps`] (lossless: FLAC).
    pub fn is_lossless(self) -> bool {
        matches!(self, OutputFormat::Flac)
    }

    /// A sensible default bitrate for podcast-quality output when the caller doesn't specify
    /// one. Ignored for lossless formats.
    fn default_bitrate_kbps(self) -> u32 {
        match self {
            OutputFormat::Mp3 => 192,
            OutputFormat::Opus => 128,
            OutputFormat::Vorbis => 160,
            OutputFormat::Aac => 160,
            OutputFormat::M4b => 128,
            OutputFormat::Flac => 0,
        }
    }
}

/// Encode target: format plus the knobs the M2 deliverable calls out — bitrate for lossy
/// formats, sample/bit depth for lossless, and mono downmix. Build one with a per-format
/// constructor ([`OutputSpec::mp3`], [`OutputSpec::opus`], …) or [`OutputSpec::new`] plus the
/// `with_*` builders.
#[derive(Debug, Clone)]
pub struct OutputSpec {
    pub format: OutputFormat,
    /// Lossy bitrate in kbps. `None` uses [`OutputFormat::default_bitrate_kbps`]. Ignored for
    /// FLAC.
    pub bitrate_kbps: Option<u32>,
    /// Downmix to a single channel. Implemented as an ffmpeg *output*-side `-ac 1`, which
    /// mixes properly (unlike mismatching the raw-PCM *input* channel count, which corrupts
    /// the stream — see the encode.rs tests for why this module never does that).
    pub mono: bool,
    /// Resample the encoded output to this rate. `None` keeps the buffer's own rate (the
    /// engine's internal 48 kHz, per ADR-002, unless the caller passes something else).
    pub sample_rate: Option<u32>,
    /// Bit depth for lossless output (FLAC only): 16 or 24. `None` defaults to 16.
    pub bit_depth: Option<u16>,
}

impl OutputSpec {
    pub fn new(format: OutputFormat) -> Self {
        Self {
            format,
            bitrate_kbps: None,
            mono: false,
            sample_rate: None,
            bit_depth: None,
        }
    }

    pub fn mp3(bitrate_kbps: u32) -> Self {
        Self::new(OutputFormat::Mp3).with_bitrate(bitrate_kbps)
    }

    pub fn opus(bitrate_kbps: u32) -> Self {
        Self::new(OutputFormat::Opus).with_bitrate(bitrate_kbps)
    }

    pub fn vorbis(bitrate_kbps: u32) -> Self {
        Self::new(OutputFormat::Vorbis).with_bitrate(bitrate_kbps)
    }

    /// `bit_depth` is 16 or 24; anything else falls back to 16 at encode time.
    pub fn flac(bit_depth: u16) -> Self {
        Self::new(OutputFormat::Flac).with_bit_depth(bit_depth)
    }

    pub fn aac(bitrate_kbps: u32) -> Self {
        Self::new(OutputFormat::Aac).with_bitrate(bitrate_kbps)
    }

    pub fn m4b(bitrate_kbps: u32) -> Self {
        Self::new(OutputFormat::M4b).with_bitrate(bitrate_kbps)
    }

    #[must_use]
    pub fn with_bitrate(mut self, bitrate_kbps: u32) -> Self {
        self.bitrate_kbps = Some(bitrate_kbps);
        self
    }

    #[must_use]
    pub fn with_mono(mut self, mono: bool) -> Self {
        self.mono = mono;
        self
    }

    #[must_use]
    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.sample_rate = Some(sample_rate);
        self
    }

    #[must_use]
    pub fn with_bit_depth(mut self, bit_depth: u16) -> Self {
        self.bit_depth = Some(bit_depth);
        self
    }

    fn resolved_bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
            .unwrap_or_else(|| self.format.default_bitrate_kbps())
    }

    fn resolved_bit_depth(&self) -> u16 {
        match self.bit_depth {
            Some(24) => 24,
            _ => 16,
        }
    }
}

impl FfmpegSidecar {
    /// Encode `buffer` to `out_path` per `spec`. See [`Self::encode_with_progress`] for a
    /// progress callback.
    pub fn encode(
        &self,
        buffer: &AudioBuffer,
        spec: &OutputSpec,
        out_path: &Path,
    ) -> Result<(), MediaError> {
        self.encode_with_progress(buffer, spec, out_path, |_| {})
    }

    /// Encode `buffer` to `out_path` per `spec`, reporting completion fraction in `[0, 1]`.
    pub fn encode_with_progress(
        &self,
        buffer: &AudioBuffer,
        spec: &OutputSpec,
        out_path: &Path,
        progress: impl FnMut(f32),
    ) -> Result<(), MediaError> {
        let child = self.spawn_encode(buffer, spec, out_path, None)?;
        run_encode_child(child, buffer, progress)
    }

    /// Encode the same in-memory `buffer` to several targets without re-decoding or
    /// re-rendering (M2 "simultaneous outputs" requirement — see module docs). Each target is
    /// a fresh ffmpeg child, run sequentially; stops at the first failure.
    pub fn encode_multi(
        &self,
        buffer: &AudioBuffer,
        targets: &[(OutputSpec, PathBuf)],
    ) -> Result<(), MediaError> {
        for (spec, out_path) in targets {
            self.encode(buffer, spec, out_path)?;
        }
        Ok(())
    }

    /// Encode an M4B audiobook: AAC in an MP4 (`ipod` muxer) container with chapters, in one
    /// pass — no separate chapter-embedding remux. `spec.format` is forced to
    /// [`OutputFormat::M4b`] regardless of what's passed in. Pass an empty `chapters` slice
    /// for a plain M4B with no chapter list.
    pub fn encode_m4b_audiobook(
        &self,
        buffer: &AudioBuffer,
        spec: &OutputSpec,
        chapters: &[Chapter],
        out_path: &Path,
        progress: impl FnMut(f32),
    ) -> Result<(), MediaError> {
        let mut spec = spec.clone();
        spec.format = OutputFormat::M4b;

        if chapters.is_empty() {
            return self.encode_with_progress(buffer, &spec, out_path, progress);
        }

        let meta_path = write_ffmetadata_chapters(chapters)?;
        let child = self.spawn_encode(buffer, &spec, out_path, Some(&meta_path));
        let result = child.and_then(|c| run_encode_child(c, buffer, progress));
        let _ = std::fs::remove_file(&meta_path);
        result
    }

    /// Build the encode child: raw `f32le` PCM piped in on stdin, encoded per `spec` to
    /// `out_path`. `chapters_ffmeta`, if given, is a second ffmpeg input (an `ffmetadata`
    /// file — see [`crate::metadata`]) whose chapters are mapped into the output without
    /// touching the audio's own stream metadata (`-map_metadata 1` here is safe because input
    /// 1 is a synthetic file we just wrote — there's no caller metadata to lose).
    fn spawn_encode(
        &self,
        buffer: &AudioBuffer,
        spec: &OutputSpec,
        out_path: &Path,
        chapters_ffmeta: Option<&Path>,
    ) -> Result<Child, MediaError> {
        let channels = buffer.channel_count().max(1);

        let mut cmd = Command::new(self.binary());
        cmd.args(["-y", "-nostdin", "-hide_banner", "-loglevel", "error"])
            .args(["-f", "f32le"])
            .args(["-ar", &buffer.sample_rate().to_string()])
            .args(["-ac", &channels.to_string()])
            .arg("-i")
            .arg("pipe:0");

        if let Some(meta) = chapters_ffmeta {
            cmd.arg("-i").arg(meta).args([
                "-map_metadata",
                "1",
                "-map_chapters",
                "1",
                "-map",
                "0:a",
            ]);
        }

        apply_output_spec(&mut cmd, spec);

        cmd.args(["-progress", "pipe:2"])
            .arg(out_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        cmd.spawn().map_err(MediaError::from)
    }
}

/// Append the codec/bitrate/downmix/resample output options for `spec` to `cmd`. Shared with
/// [`crate::video::remux_with_audio_spec`], which encodes the mastered-audio leg of a video
/// remux through the exact same codec logic.
pub(crate) fn apply_output_spec(cmd: &mut Command, spec: &OutputSpec) {
    match spec.format {
        OutputFormat::Mp3 => {
            cmd.args(["-c:a", "libmp3lame"]);
            push_bitrate(cmd, spec);
        }
        OutputFormat::Opus => {
            cmd.args(["-c:a", "libopus"]);
            push_bitrate(cmd, spec);
        }
        OutputFormat::Vorbis => {
            cmd.args(["-c:a", "libvorbis"]);
            push_bitrate(cmd, spec);
        }
        OutputFormat::Aac => {
            cmd.args(["-c:a", "aac"]);
            push_bitrate(cmd, spec);
        }
        OutputFormat::M4b => {
            cmd.args(["-c:a", "aac"]);
            push_bitrate(cmd, spec);
            // `ipod` sets the MP4 `stik` atom so players treat this as an audiobook.
            cmd.args(["-f", "ipod"]);
        }
        OutputFormat::Flac => {
            cmd.args(["-c:a", "flac"]);
            if spec.resolved_bit_depth() == 24 {
                cmd.args(["-sample_fmt", "s32", "-bits_per_raw_sample", "24"]);
            } else {
                cmd.args(["-sample_fmt", "s16"]);
            }
        }
    }

    if spec.mono {
        cmd.args(["-ac", "1"]);
    }
    if let Some(sr) = spec.sample_rate {
        cmd.args(["-ar", &sr.to_string()]);
    }
}

fn push_bitrate(cmd: &mut Command, spec: &OutputSpec) {
    cmd.args(["-b:a", &format!("{}k", spec.resolved_bitrate_kbps())]);
}

/// Drive an already-spawned encode child to completion: write `buffer`'s interleaved PCM to
/// stdin on a worker thread (mirrors the decode side's stdout-drain thread — this prevents a
/// full stderr pipe from deadlocking a full stdin pipe), parse `-progress` lines from stderr
/// on the calling thread, then wait and check the exit status. Shared by every encode/remux
/// path in this crate.
pub(crate) fn run_encode_child(
    mut child: Child,
    buffer: &AudioBuffer,
    mut progress: impl FnMut(f32),
) -> Result<(), MediaError> {
    let mut stdin = child.stdin.take().expect("stdin piped");
    let bytes = interleave_f32le(buffer);
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        stdin.write_all(&bytes)?;
        // Explicit drop closes the pipe so ffmpeg sees EOF and starts flushing/finalizing.
        drop(stdin);
        Ok(())
    });

    let total_secs = buffer.frames() as f64 / f64::from(buffer.sample_rate().max(1));
    let mut stderr_tail = String::new();
    if let Some(stderr) = child.stderr.take() {
        for line in BufReader::new(stderr).lines() {
            let line = line?;
            if let Some(frac) = progress_fraction(&line, total_secs) {
                progress(frac);
            }
            push_tail(&mut stderr_tail, &line);
        }
    }

    let status = child.wait()?;
    writer
        .join()
        .map_err(|_| MediaError::SidecarFailed("stdin writer thread panicked".into()))??;

    if !status.success() {
        return Err(MediaError::SidecarFailed(format!(
            "ffmpeg exited with {status}: {}",
            stderr_tail.trim()
        )));
    }
    progress(1.0);
    Ok(())
}

/// Encode `buffer` to `out_path` per `spec`, locating the ffmpeg sidecar automatically.
/// Convenience wrapper over [`FfmpegSidecar::encode`] for callers that don't already hold a
/// located sidecar.
pub fn encode(buffer: &AudioBuffer, spec: &OutputSpec, out_path: &Path) -> Result<(), MediaError> {
    FfmpegSidecar::locate()?.encode(buffer, spec, out_path)
}

/// [`encode`] with a progress callback.
pub fn encode_with_progress(
    buffer: &AudioBuffer,
    spec: &OutputSpec,
    out_path: &Path,
    progress: impl FnMut(f32),
) -> Result<(), MediaError> {
    FfmpegSidecar::locate()?.encode_with_progress(buffer, spec, out_path, progress)
}

/// [`FfmpegSidecar::encode_multi`], locating the sidecar automatically.
pub fn encode_multi(
    buffer: &AudioBuffer,
    targets: &[(OutputSpec, PathBuf)],
) -> Result<(), MediaError> {
    FfmpegSidecar::locate()?.encode_multi(buffer, targets)
}

/// [`FfmpegSidecar::encode_m4b_audiobook`], locating the sidecar automatically.
pub fn encode_m4b_audiobook(
    buffer: &AudioBuffer,
    spec: &OutputSpec,
    chapters: &[Chapter],
    out_path: &Path,
    progress: impl FnMut(f32),
) -> Result<(), MediaError> {
    FfmpegSidecar::locate()?.encode_m4b_audiobook(buffer, spec, chapters, out_path, progress)
}
