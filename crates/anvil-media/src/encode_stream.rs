//! Block-at-a-time streaming encoder for the M5 streaming master.
//!
//! [`crate::encode`] takes a whole [`AudioBuffer`] and pipes it to ffmpeg in one shot — fine for
//! short clips, but a 3-hour master must never hold the whole output in RAM (06 §4). This is the
//! same ffmpeg-sidecar encode driven **incrementally**: the caller pushes mastered blocks as the
//! streaming master produces them and each is interleaved to `f32le` and written to ffmpeg's
//! stdin straight away. It reuses the exact codec/bitrate/mux argument logic
//! ([`crate::encode::apply_output_spec`]) and stderr handling of the tested whole-buffer path;
//! only the stdin feeding is different (per-block on the calling thread instead of one buffer on
//! a worker thread). Additive — nothing in the existing encode path changes.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread::JoinHandle;

use crate::encode::{apply_output_spec, OutputSpec};
use crate::error::MediaError;
use crate::sidecar::{interleave_f32le, push_tail, FfmpegSidecar};
use crate::AudioBuffer;

/// A streaming ffmpeg encoder. Spawns the child lazily on the first block (so `-ac`/`-ar` come
/// from the real audio), then writes each block's interleaved `f32le` to stdin as it arrives.
pub struct StreamEncoder {
    spec: OutputSpec,
    out_path: PathBuf,
    binary: PathBuf,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stderr_thread: Option<JoinHandle<String>>,
}

impl StreamEncoder {
    /// Locate the pinned ffmpeg sidecar and prepare (but do not yet spawn) the encoder for
    /// `spec` → `out_path`.
    pub fn new(spec: OutputSpec, out_path: &Path) -> Result<Self, MediaError> {
        let sidecar = FfmpegSidecar::locate()?;
        Ok(Self {
            spec,
            out_path: out_path.to_path_buf(),
            binary: sidecar.binary().to_path_buf(),
            child: None,
            stdin: None,
            stderr_thread: None,
        })
    }

    fn ensure_started(&mut self, channels: usize, sample_rate: u32) -> Result<(), MediaError> {
        if self.child.is_some() {
            return Ok(());
        }
        let mut cmd = Command::new(&self.binary);
        cmd.args(["-y", "-nostdin", "-hide_banner", "-loglevel", "error"])
            .args(["-f", "f32le"])
            .args(["-ar", &sample_rate.to_string()])
            .args(["-ac", &channels.max(1).to_string()])
            .arg("-i")
            .arg("pipe:0");
        apply_output_spec(&mut cmd, &self.spec);
        cmd.args(["-progress", "pipe:2"])
            .arg(&self.out_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(MediaError::from)?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stderr = child.stderr.take();
        // Drain stderr on a worker thread so a full stderr pipe can never deadlock the stdin
        // writes (mirrors the whole-buffer `run_encode_child`).
        let handle = std::thread::spawn(move || {
            let mut tail = String::new();
            if let Some(stderr) = stderr {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    push_tail(&mut tail, &line);
                }
            }
            tail
        });
        self.stdin = Some(stdin);
        self.child = Some(child);
        self.stderr_thread = Some(handle);
        Ok(())
    }

    /// Feed one mastered block to the encoder. Empty blocks are ignored.
    pub fn write_block(&mut self, block: &AudioBuffer) -> Result<(), MediaError> {
        if block.frames() == 0 {
            return Ok(());
        }
        self.ensure_started(block.channel_count().max(1), block.sample_rate())?;
        let bytes = interleave_f32le(block);
        self.stdin
            .as_mut()
            .expect("stdin present after ensure_started")
            .write_all(&bytes)
            .map_err(MediaError::from)
    }

    /// Close stdin, wait for ffmpeg to finalize the container, and surface any failure.
    pub fn finish(mut self) -> Result<(), MediaError> {
        // Closing stdin signals EOF so ffmpeg flushes and writes the trailer.
        drop(self.stdin.take());
        let tail = self
            .stderr_thread
            .take()
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        if let Some(mut child) = self.child.take() {
            let status = child.wait().map_err(MediaError::from)?;
            if !status.success() {
                return Err(MediaError::SidecarFailed(format!(
                    "ffmpeg exited with {status}: {}",
                    tail.trim()
                )));
            }
        }
        Ok(())
    }
}
