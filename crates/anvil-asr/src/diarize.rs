//! Speaker diarization — *who spoke when* — via a **sherpa-onnx sidecar process**.
//!
//! ## Why a sidecar and not `sherpa-rs` / `ort`
//! ADR-004 specifies the pipeline (*pyannote segmentation → speaker embeddings → clustering*)
//! but not how it is hosted. ANVIL runs it as an external
//! `sherpa-onnx-offline-speaker-diarization` child process, exactly as it runs `whisper-cli`
//! ([`crate::sidecar`]) and ffmpeg ([`anvil_media`]). The alternatives were weighed and
//! rejected:
//!
//! - **`sherpa-rs`** links sherpa-onnx into the binary. That drags a C++/ONNX-Runtime link
//!   step (and, with `download-binaries`, a *build-time* download) into every crate that
//!   depends on `anvil-asr` — which today is `anvil-cut`, `anvil-cli` and `apps/desktop`.
//!   A diarization feature has no business changing how the CLI links.
//! - **`ort` directly** means hand-writing pyannote powerset decoding, the Kaldi-style
//!   filterbank front-end for the embedding model, and agglomerative clustering. That is a
//!   large surface of subtle numerical code to get wrong, and every bug in it shows up as
//!   silently worse DER rather than a crash.
//! - **The sidecar** reuses sherpa-onnx's own tested C++ pipeline, keeps this crate at four
//!   pure-Rust dependencies, and cannot break anybody else's build. It is the same bargain
//!   the ASR lane already made with whisper.cpp.
//!
//! ## Airplane-mode (ADR-005 engine invariant)
//! Nothing here touches the network. The sidecar binary must already be present (`ANVIL_DIARIZE`,
//! bundled next to the app, or on `PATH`) and both ONNX models must already be on disk (see
//! [`crate::model`], which carries their SHA-256s for a one-time, up-front, user-initiated
//! download).
//!
//! ## Audio expectations
//! Unlike whisper.cpp, the sherpa-onnx sidecar **does not resample**: it requires 16 kHz mono
//! 16-bit PCM WAV. So this module inspects the WAV header first and, when the input is
//! anything else (48 kHz stereo, MP3, an mp4's audio track, …), transcodes it to a temporary
//! 16 kHz mono WAV with the ffmpeg sidecar. If ffmpeg cannot be found *and* the input is not
//! already in the right shape, you get [`AsrError::AudioUnsupported`] rather than a confusing
//! failure from sherpa.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::AsrError;
use crate::model::{default_diarization_model, DiarModelKind};
use crate::Transcript;

/// The sample rate the sherpa-onnx diarization models are trained for. Anything else must be
/// resampled before the sidecar sees it.
const REQUIRED_SAMPLE_RATE: u32 = 16_000;

/// A word that overlaps no speaker turn is snapped to the nearest turn within this many
/// seconds — whisper and the diarizer disagree at the edges by a few tens of milliseconds,
/// and the diarizer trims leading/trailing breath that whisper still calls part of a word.
/// Beyond this gap the word is genuinely outside anyone's turn (music, an intro sting, a long
/// pause) and stays `None` rather than being attributed to somebody who was not talking.
const SPEAKER_SNAP_SECS: f64 = 2.0;

/// One speaker in a diarized recording.
///
/// `id` is dense (`0, 1, 2, …`) and ordered by **who spoke first** — `Speaker { id: 0 }` is
/// whoever opens the recording, which for a podcast is nearly always the host. `label` starts
/// as `"Speaker 1"`, `"Speaker 2"`, … and is meant to be renamed by the user ("Rob", "Guest");
/// nothing in this crate keys off the label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Speaker {
    /// Dense speaker id, `0`-based. Referenced by [`SpeakerSegment::speaker`] and
    /// [`Word::speaker`].
    pub id: u32,
    /// Display name, defaulting to `"Speaker N"` (1-based, so id `0` is `"Speaker 1"`).
    pub label: String,
}

/// A contiguous stretch of audio attributed to one speaker, in **seconds**.
///
/// Segments are sorted by `start`. They may overlap: the segmentation model is genuinely
/// capable of saying two people were talking at once, and ANVIL does not flatten that away.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeakerSegment {
    /// The [`Speaker::id`] talking.
    pub speaker: u32,
    /// Start time in seconds.
    pub start: f64,
    /// End time in seconds.
    pub end: f64,
}

/// The result of diarizing one file: the cast, and their turns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Diarization {
    /// Every speaker found, `id`-ordered.
    pub speakers: Vec<Speaker>,
    /// Every speaker turn, `start`-ordered.
    pub segments: Vec<SpeakerSegment>,
}

impl Diarization {
    /// Total speech time attributed to `speaker`, in seconds (overlapping turns are counted
    /// once each — this is a talk-time figure, not a union of the timeline).
    pub fn speaking_time(&self, speaker: u32) -> f64 {
        self.segments
            .iter()
            .filter(|s| s.speaker == speaker)
            .map(|s| (s.end - s.start).max(0.0))
            .sum()
    }
}

/// Knobs for a single [`diarize`] call.
#[derive(Debug, Clone)]
pub struct DiarizeOptions {
    /// How many speakers to find. `None` (the default) auto-detects by cutting the clustering
    /// dendrogram at [`cluster_threshold`](Self::cluster_threshold); `Some(n)` forces exactly
    /// `n`. Pass `Some(2)` for a host+guest interview — knowing the count is the single
    /// biggest accuracy win available.
    pub num_speakers: Option<usize>,
    /// Explicit segmentation `.onnx`. `None` resolves from `ANVIL_DIARIZE_SEG_MODEL`, then the
    /// catalog default installed in the models dir.
    pub segmentation_model: Option<PathBuf>,
    /// Explicit speaker-embedding `.onnx`. `None` resolves from `ANVIL_DIARIZE_EMB_MODEL`, then
    /// the catalog default installed in the models dir.
    pub embedding_model: Option<PathBuf>,
    /// Cosine-distance cut for auto speaker counting. Ignored when
    /// [`num_speakers`](Self::num_speakers) is `Some`. Smaller → more speakers.
    pub cluster_threshold: f32,
    /// Speaker turns shorter than this are discarded, in seconds.
    pub min_speech_secs: f32,
    /// Two turns by the same speaker separated by less than this are merged, in seconds.
    pub min_silence_secs: f32,
    /// Threads for both ONNX sessions. `None` leaves sherpa's default (1).
    pub threads: Option<usize>,
}

impl Default for DiarizeOptions {
    fn default() -> Self {
        Self {
            num_speakers: None,
            segmentation_model: None,
            embedding_model: None,
            // sherpa-onnx's own defaults, which we verified on the synthetic fixture: auto
            // mode at 0.5 recovers the same 3 speakers as forcing `num_speakers = Some(3)`.
            cluster_threshold: 0.5,
            min_speech_secs: 0.3,
            min_silence_secs: 0.5,
            threads: None,
        }
    }
}

/// A located `sherpa-onnx-offline-speaker-diarization` binary, reusable across calls.
#[derive(Debug, Clone)]
pub struct DiarizeSidecar {
    binary: PathBuf,
}

impl DiarizeSidecar {
    /// Locate the sidecar without touching the network. Search order:
    /// 1. `ANVIL_DIARIZE` environment variable (explicit path),
    /// 2. a bundled sidecar next to the current executable (`…`, `sidecar/…`, `sherpa/…`, and —
    ///    for a macOS `.app` — `../Resources/sherpa/bin/…`; the mac sherpa bundle keeps its
    ///    `bin/` + `lib/` sibling structure, so the exe lives under a `bin/` subdir there,
    ///    unlike the flat Windows layout),
    /// 3. the binary on `PATH`.
    pub fn locate() -> Result<Self, AsrError> {
        for candidate in Self::candidates() {
            if candidate.is_file() {
                return Self::from_path(candidate);
            }
        }
        if let Some(found) = search_path(&Self::exe_name()) {
            return Self::from_path(found);
        }
        Err(AsrError::SidecarNotFound(
            "no bundled sherpa-onnx-offline-speaker-diarization, ANVIL_DIARIZE unset, and it is \
             not on PATH (airplane-mode: ANVIL never auto-downloads it)"
                .into(),
        ))
    }

    /// Wrap an explicit sidecar path.
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, AsrError> {
        let binary = path.into();
        if !binary.is_file() {
            return Err(AsrError::SidecarNotFound(binary.display().to_string()));
        }
        Ok(Self { binary })
    }

    /// Path of the resolved binary.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    fn exe_name() -> String {
        format!(
            "sherpa-onnx-offline-speaker-diarization{}",
            std::env::consts::EXE_SUFFIX
        )
    }

    fn candidates() -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(explicit) = std::env::var_os("ANVIL_DIARIZE") {
            out.push(PathBuf::from(explicit));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let name = Self::exe_name();
                out.push(dir.join(&name));
                out.push(dir.join("sidecar").join(&name));
                out.push(dir.join("sherpa").join(&name));
                // macOS `.app` layout: `bundle.resources` land in `Contents/Resources/`, and the
                // mac sherpa bundle preserves its `bin/` + `lib/` siblings (the binary's rpath is
                // `@loader_path/../lib`), so the exe is at `../Resources/sherpa/bin/<exe>` — note
                // the `bin/` subdir, encoded here for mac. Added unconditionally (the workspace
                // confines `#[cfg]` to anvil-core::platform): this path does not exist in a
                // Windows/flat install, so Windows resolution stays byte-identical.
                out.push(dir.join("../Resources/sherpa/bin").join(&name));
            }
        }
        out
    }

    /// Diarize `audio`, returning who spoke when.
    ///
    /// Resamples to 16 kHz mono via the ffmpeg sidecar if needed (see the module docs), runs
    /// sherpa-onnx, and normalises its output into a [`Diarization`] with dense, first-speaker-
    /// first ids.
    pub fn diarize(&self, audio: &Path, opts: &DiarizeOptions) -> Result<Diarization, AsrError> {
        if !audio.is_file() {
            return Err(AsrError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("audio file not found: {}", audio.display()),
            )));
        }
        let segmentation = resolve_diar_model(
            opts.segmentation_model.as_deref(),
            "ANVIL_DIARIZE_SEG_MODEL",
            DiarModelKind::Segmentation,
        )?;
        let embedding = resolve_diar_model(
            opts.embedding_model.as_deref(),
            "ANVIL_DIARIZE_EMB_MODEL",
            DiarModelKind::Embedding,
        )?;

        let prepared = prepare_audio(audio)?;
        let stdout = self.run(prepared.path(), &segmentation, &embedding, opts)?;
        parse_diarization_output(&stdout)
    }

    fn run(
        &self,
        audio: &Path,
        segmentation: &Path,
        embedding: &Path,
        opts: &DiarizeOptions,
    ) -> Result<String, AsrError> {
        tracing::debug!(
            binary = %self.binary.display(),
            segmentation = %segmentation.display(),
            embedding = %embedding.display(),
            audio = %audio.display(),
            num_speakers = ?opts.num_speakers,
            "running sherpa-onnx speaker diarization"
        );

        let mut cmd = Command::new(&self.binary);
        cmd.arg(format!(
            "--segmentation.pyannote-model={}",
            segmentation.display()
        ))
        .arg(format!("--embedding.model={}", embedding.display()))
        .arg(format!("--min-duration-on={}", opts.min_speech_secs))
        .arg(format!("--min-duration-off={}", opts.min_silence_secs));

        // num-clusters wins when set; sherpa ignores the threshold in that case, but we only
        // pass one of the two so the invocation reads honestly in a log.
        match opts.num_speakers {
            Some(n) if n > 0 => {
                cmd.arg(format!("--clustering.num-clusters={n}"));
            }
            _ => {
                cmd.arg(format!(
                    "--clustering.cluster-threshold={}",
                    opts.cluster_threshold
                ));
            }
        }
        if let Some(threads) = opts.threads {
            cmd.arg(format!("--segmentation.num-threads={threads}"))
                .arg(format!("--embedding.num-threads={threads}"));
        }
        cmd.arg(audio)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // sherpa streams a `progress NN%` ticker to stderr; we do not want it interleaved
            // into the results we parse, and we keep it only to quote back on failure.
            .stderr(Stdio::piped());

        let output = cmd.output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AsrError::SidecarFailed(format!(
                "sherpa-onnx-offline-speaker-diarization exited with {}: {}",
                output.status,
                tail(&stderr, 800)
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Locate the sidecar and diarize `audio` in one call. Convenience wrapper over
/// [`DiarizeSidecar::locate`] + [`DiarizeSidecar::diarize`].
pub fn diarize(audio: &Path, opts: &DiarizeOptions) -> Result<Diarization, AsrError> {
    DiarizeSidecar::locate()?.diarize(audio, opts)
}

/// Stamp every [`Word`] in `transcript` with the speaker who said it, and every [`Segment`]
/// with its dominant speaker.
///
/// A word is attributed to the speaker turn it **overlaps most**. A word that overlaps no turn
/// at all — whisper and the diarizer disagree by tens of milliseconds at turn edges, and the
/// diarizer drops sub-`min_speech_secs` blips entirely — snaps to the nearest turn within
/// [`SPEAKER_SNAP_SECS`]; past that it stays `None`, because it is genuinely outside anybody's
/// speech (an intro sting, a music bed, a long silence) and guessing there would be worse than
/// admitting ignorance.
///
/// A segment takes whichever speaker holds the most **word-time** inside it, so a sentence
/// whose last word bleeds into the next person's turn still belongs to the person who said
/// most of it. Ties break toward the lower speaker id, which keeps the function deterministic
/// (ADR-003).
///
/// Idempotent, and safe to call with an empty [`Diarization`] (everything becomes `None`).
pub fn assign_speakers(transcript: &mut Transcript, diar: &Diarization) {
    for word in &mut transcript.words {
        word.speaker = speaker_for_span(word.start, word.end, &diar.segments);
    }

    // Snapshot the (start, end, speaker) triples so the segment pass can borrow them while
    // `transcript.segments` is mutably borrowed.
    let words: Vec<(f64, f64, Option<u32>)> = transcript
        .words
        .iter()
        .map(|w| (w.start, w.end, w.speaker))
        .collect();

    for segment in &mut transcript.segments {
        segment.speaker = dominant_speaker(segment.start, segment.end, &words);
    }
}

/// The speaker turn that `[start, end]` overlaps most, else the nearest turn within
/// [`SPEAKER_SNAP_SECS`], else `None`.
fn speaker_for_span(start: f64, end: f64, segments: &[SpeakerSegment]) -> Option<u32> {
    let mut best_overlap = 0.0f64;
    let mut best_overlap_speaker: Option<u32> = None;
    let mut best_distance = f64::INFINITY;
    let mut nearest_speaker: Option<u32> = None;

    for seg in segments {
        let overlap = end.min(seg.end) - start.max(seg.start);
        if overlap > best_overlap {
            best_overlap = overlap;
            best_overlap_speaker = Some(seg.speaker);
        }

        // Gap between the word and this turn; 0 when they touch or overlap. This is what lets
        // a zero-length word sitting inside a turn still find its speaker.
        let distance = if end < seg.start {
            seg.start - end
        } else if start > seg.end {
            start - seg.end
        } else {
            0.0
        };
        if distance < best_distance {
            best_distance = distance;
            nearest_speaker = Some(seg.speaker);
        }
    }

    if best_overlap_speaker.is_some() {
        return best_overlap_speaker;
    }
    if best_distance <= SPEAKER_SNAP_SECS {
        return nearest_speaker;
    }
    None
}

/// The speaker holding the most word-time inside `[start, end]`. Ties break to the lower id.
fn dominant_speaker(start: f64, end: f64, words: &[(f64, f64, Option<u32>)]) -> Option<u32> {
    // Speaker ids are dense and small (a podcast is not a stadium), so a flat Vec keyed by id
    // beats a HashMap and, unlike a HashMap, iterates in id order — which is what makes the
    // tie-break deterministic.
    let mut weight: Vec<f64> = Vec::new();
    for &(w_start, w_end, speaker) in words {
        let Some(id) = speaker else { continue };
        if w_end < start || w_start > end {
            continue;
        }
        // Weight by the part of the word inside the segment, but never zero: a zero-length
        // word still votes, just faintly.
        let inside = (w_end.min(end) - w_start.max(start)).max(0.0);
        let idx = id as usize;
        if weight.len() <= idx {
            weight.resize(idx + 1, 0.0);
        }
        weight[idx] += inside.max(1e-3);
    }

    let mut best: Option<(u32, f64)> = None;
    for (id, &w) in weight.iter().enumerate() {
        if w > 0.0 && best.is_none_or(|(_, best_w)| w > best_w) {
            best = Some((id as u32, w));
        }
    }
    best.map(|(id, _)| id)
}

/// Parse the sidecar's stdout into a [`Diarization`].
///
/// Runs with **no** sherpa process — pure string → struct — so it is fully unit-testable
/// against captured output. sherpa prints a config dump, a `Started` line, then one turn per
/// line:
///
/// ```text
/// 0.031 -- 1.499 speaker_00
/// 9.869 -- 18.458 speaker_01
/// ```
///
/// Two things this normalises. First, sherpa's `speaker_NN` numbers are **cluster indices**,
/// which are sparse — auto mode on our own fixture emits `speaker_00`, `speaker_01`,
/// `speaker_04` for three speakers. Callers get dense `0..n` ids instead. Second, ids are
/// re-assigned in **order of first speech**, so `Speaker 1` is whoever talks first rather than
/// whichever cluster index the dendrogram happened to hand out.
pub fn parse_diarization_output(stdout: &str) -> Result<Diarization, AsrError> {
    let mut raw: Vec<(f64, f64, String)> = Vec::new();

    for line in stdout.lines() {
        // Windows sherpa terminates lines with \r\n; `lines()` leaves the \r behind.
        let line = line.trim();
        let Some((start, end, label)) = parse_turn_line(line) else {
            continue;
        };
        if end < start {
            return Err(AsrError::DiarizeParse(format!(
                "turn ends before it starts: {line:?}"
            )));
        }
        raw.push((start, end, label));
    }

    if raw.is_empty() {
        // Not an error state we can distinguish from "this file is 40 minutes of silence", so
        // treat a run that found nobody as an empty diarization rather than a failure. A run
        // that *failed* returns non-zero and never reaches here.
        return Ok(Diarization::default());
    }

    // Sort by start (sherpa mostly emits in order, but overlapping turns can come out
    // interleaved), then assign dense ids in order of first appearance.
    raw.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));

    let mut order: Vec<String> = Vec::new();
    let mut segments = Vec::with_capacity(raw.len());
    for (start, end, label) in raw {
        let id = match order.iter().position(|l| *l == label) {
            Some(i) => i,
            None => {
                order.push(label);
                order.len() - 1
            }
        };
        segments.push(SpeakerSegment {
            speaker: id as u32,
            start,
            end,
        });
    }

    let speakers = (0..order.len())
        .map(|i| Speaker {
            id: i as u32,
            label: format!("Speaker {}", i + 1),
        })
        .collect();

    Ok(Diarization { speakers, segments })
}

/// `"0.031 -- 1.499 speaker_00"` → `(0.031, 1.499, "speaker_00")`. Anything else → `None`, so
/// sherpa's config dump and `Started` banner are skipped without a brittle prefix match.
fn parse_turn_line(line: &str) -> Option<(f64, f64, String)> {
    let mut fields = line.split_whitespace();
    let start: f64 = fields.next()?.parse().ok()?;
    if fields.next()? != "--" {
        return None;
    }
    let end: f64 = fields.next()?.parse().ok()?;
    let label = fields.next()?;
    if fields.next().is_some() {
        return None; // a trailing field means this is not a turn line
    }
    Some((start, end, label.to_string()))
}

// --- model + audio plumbing ---------------------------------------------------------------

/// Resolve one diarization model: explicit path wins, then `env_key`, then the catalog default
/// installed in the models dir. Never downloads.
fn resolve_diar_model(
    explicit: Option<&Path>,
    env_key: &str,
    kind: DiarModelKind,
) -> Result<PathBuf, AsrError> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        return Err(AsrError::ModelNotFound(path.display().to_string()));
    }
    if let Some(env) = std::env::var_os(env_key) {
        let path = PathBuf::from(env);
        if path.is_file() {
            return Ok(path);
        }
        return Err(AsrError::ModelNotFound(path.display().to_string()));
    }
    default_diarization_model(kind).ok_or_else(|| {
        AsrError::ModelNotFound(format!(
            "no {kind:?} model given, {env_key} unset, and none installed in the models dir"
        ))
    })
}

/// Audio staged for the sidecar. Owns the temp file when a transcode was needed, and deletes
/// it on drop so a long batch does not leave a trail of 16 kHz WAVs in `%TEMP%`.
enum PreparedAudio {
    /// The input was already 16 kHz mono 16-bit PCM WAV; hand it straight to sherpa.
    AsIs(PathBuf),
    /// The input needed transcoding; this temp file is ours to delete.
    Temp(PathBuf),
}

impl PreparedAudio {
    fn path(&self) -> &Path {
        match self {
            PreparedAudio::AsIs(p) | PreparedAudio::Temp(p) => p,
        }
    }
}

impl Drop for PreparedAudio {
    fn drop(&mut self) {
        if let PreparedAudio::Temp(path) = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Give sherpa what it insists on: 16 kHz mono 16-bit PCM WAV.
fn prepare_audio(audio: &Path) -> Result<PreparedAudio, AsrError> {
    if is_16k_mono_pcm_wav(audio) {
        return Ok(PreparedAudio::AsIs(audio.to_path_buf()));
    }

    let ffmpeg = locate_ffmpeg().ok_or_else(|| {
        AsrError::AudioUnsupported(format!(
            "{} is not 16 kHz mono 16-bit PCM WAV, and ffmpeg (ANVIL_FFMPEG / bundled / PATH) \
             was not found to convert it. The diarization sidecar does not resample.",
            audio.display()
        ))
    })?;

    let out = temp_path("wav");
    tracing::debug!(
        audio = %audio.display(),
        out = %out.display(),
        "transcoding to 16 kHz mono for the diarization sidecar"
    );

    let output = Command::new(&ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(audio)
        .args(["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"])
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if !output.status.success() {
        let _ = std::fs::remove_file(&out);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AsrError::AudioUnsupported(format!(
            "ffmpeg could not convert {} to 16 kHz mono WAV ({}): {}",
            audio.display(),
            output.status,
            tail(&stderr, 400)
        )));
    }
    Ok(PreparedAudio::Temp(out))
}

/// `true` iff `path` is a RIFF/WAVE file whose `fmt ` chunk says 16 kHz, 1 channel, 16-bit
/// uncompressed PCM. Walks the chunk list rather than assuming a 44-byte header, because a WAV
/// is perfectly entitled to carry `LIST`/`fact` chunks before `fmt `.
fn is_16k_mono_pcm_wav(path: &Path) -> bool {
    let Ok(bytes) = read_prefix(path, 4096) else {
        return false;
    };
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return false;
    }

    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            if body + 16 > bytes.len() {
                return false;
            }
            let format = u16::from_le_bytes([bytes[body], bytes[body + 1]]);
            let channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
            let sample_rate = u32::from_le_bytes([
                bytes[body + 4],
                bytes[body + 5],
                bytes[body + 6],
                bytes[body + 7],
            ]);
            let bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
            return format == 1
                && channels == 1
                && sample_rate == REQUIRED_SAMPLE_RATE
                && bits == 16;
        }
        // Chunks are word-aligned; an odd size carries a pad byte.
        pos = body.saturating_add(size).saturating_add(size % 2);
    }
    false
}

/// First `max` bytes of a file (the WAV header lives well inside 4 KiB — we are not going to
/// read a 3-hour episode into memory to find out its sample rate).
fn read_prefix(path: &Path, max: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; max];
    let mut filled = 0usize;
    while filled < max {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Find ffmpeg the same way [`anvil_media`] does, without depending on it: `ANVIL_FFMPEG`, then
/// bundled next to the executable, then `PATH`.
fn locate_ffmpeg() -> Option<PathBuf> {
    let name = format!("ffmpeg{}", std::env::consts::EXE_SUFFIX);
    if let Some(explicit) = std::env::var_os("ANVIL_FFMPEG") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for candidate in [
                dir.join(&name),
                dir.join("sidecar").join(&name),
                dir.join("ffmpeg").join(&name),
            ] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    search_path(&name)
}

/// Find `name` on `PATH`.
fn search_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// A unique temp path with the given extension. Unique per process *and* per call, so
/// concurrent diarize jobs in a batch never collide.
fn temp_path(extension: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "anvil-diar-{}-{nanos}.{extension}",
        std::process::id()
    ));
    path
}

/// Last `max` chars of `s`, trimmed — keeps a sidecar stderr tail bounded.
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
    use crate::{Segment, Word};

    /// Real stdout captured from `sherpa-onnx-offline-speaker-diarization` v1.13.4 on the
    /// synthetic 3-speaker fixture, in **auto** mode (`--clustering.cluster-threshold=0.5`).
    /// Note the cluster indices: `speaker_00`, `speaker_01`, `speaker_04`. They are sparse,
    /// which is exactly the normalisation this parser exists to do.
    const SAMPLE_STDOUT: &str = "\
OfflineSpeakerDiarizationConfig(segmentation=..., clustering=FastClusteringConfig(num_clusters=-1, threshold=0.5), min_duration_on=0.3, min_duration_off=0.5)\r
Started\r
0.031 -- 8.637 speaker_00\r
9.869 -- 18.458 speaker_01\r
19.657 -- 27.925 speaker_04\r
29.140 -- 37.088 speaker_00\r
38.236 -- 47.399 speaker_01\r
";

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "expected ~{b}, got {a}");
    }

    #[test]
    fn parses_turns_and_densifies_sparse_cluster_ids() {
        let d = parse_diarization_output(SAMPLE_STDOUT).expect("parse");

        // Three speakers, densely numbered, despite sherpa emitting cluster index 4.
        assert_eq!(d.speakers.len(), 3);
        assert_eq!(
            d.speakers[0],
            Speaker {
                id: 0,
                label: "Speaker 1".into()
            }
        );
        assert_eq!(
            d.speakers[1],
            Speaker {
                id: 1,
                label: "Speaker 2".into()
            }
        );
        assert_eq!(
            d.speakers[2],
            Speaker {
                id: 2,
                label: "Speaker 3".into()
            }
        );

        assert_eq!(d.segments.len(), 5);
        // Ids follow order of first speech: speaker_04 (third to talk) becomes id 2.
        let ids: Vec<u32> = d.segments.iter().map(|s| s.speaker).collect();
        assert_eq!(ids, [0, 1, 2, 0, 1]);
        approx(d.segments[0].start, 0.031);
        approx(d.segments[2].end, 27.925);
    }

    #[test]
    fn config_dump_and_banner_are_not_mistaken_for_turns() {
        let d = parse_diarization_output(SAMPLE_STDOUT).expect("parse");
        // 5 turn lines out of 7 total lines: the config dump and `Started` are skipped.
        assert_eq!(d.segments.len(), 5);
    }

    #[test]
    fn speaking_time_sums_a_speakers_turns() {
        let d = parse_diarization_output(SAMPLE_STDOUT).expect("parse");
        // speaker 0: (8.637 - 0.031) + (37.088 - 29.140)
        approx(d.speaking_time(0), 8.606 + 7.948);
        approx(d.speaking_time(2), 27.925 - 19.657);
    }

    #[test]
    fn no_turns_is_an_empty_diarization_not_an_error() {
        let d = parse_diarization_output("Started\n").expect("parse");
        assert!(d.speakers.is_empty());
        assert!(d.segments.is_empty());
    }

    #[test]
    fn turns_are_sorted_and_ids_follow_first_speech() {
        // Deliberately out of order: the later-starting turn is printed first.
        let out = "9.0 -- 12.0 speaker_07\n1.0 -- 4.0 speaker_03\n";
        let d = parse_diarization_output(out).expect("parse");
        approx(d.segments[0].start, 1.0);
        // speaker_03 talks first, so it takes id 0 even though speaker_07 was printed first.
        assert_eq!(d.segments[0].speaker, 0);
        assert_eq!(d.segments[1].speaker, 1);
    }

    #[test]
    fn backwards_turn_is_rejected() {
        let err = parse_diarization_output("5.0 -- 1.0 speaker_00\n")
            .expect_err("end before start must not be accepted");
        assert!(matches!(err, AsrError::DiarizeParse(_)), "{err:?}");
    }

    #[test]
    fn overlapping_turns_survive_parsing() {
        // Two people talking at once is a real thing the segmentation model reports, and we
        // must not silently drop or merge it.
        let out = "1.0 -- 5.0 speaker_00\n4.0 -- 8.0 speaker_01\n";
        let d = parse_diarization_output(out).expect("parse");
        assert_eq!(d.segments.len(), 2);
        assert!(d.segments[0].end > d.segments[1].start, "overlap preserved");
    }

    // --- assign_speakers ------------------------------------------------------------------

    fn word(text: &str, start: f64, end: f64) -> Word {
        Word {
            text: text.into(),
            start,
            end,
            confidence: 0.9,
            speaker: None,
        }
    }

    fn segment(text: &str, start: f64, end: f64) -> Segment {
        Segment {
            text: text.into(),
            start,
            end,
            speaker: None,
        }
    }

    fn two_speaker_diarization() -> Diarization {
        parse_diarization_output("0.0 -- 5.0 speaker_00\n6.0 -- 11.0 speaker_01\n").expect("parse")
    }

    #[test]
    fn words_take_the_speaker_they_overlap_most() {
        let mut t = Transcript {
            language: "en".into(),
            words: vec![
                word("hello", 0.2, 0.7), // squarely in speaker 0
                word("there", 4.8, 5.4), // straddles the edge, mostly in speaker 0
                word("hi", 6.5, 7.0),    // squarely in speaker 1
            ],
            segments: vec![],
        };
        assign_speakers(&mut t, &two_speaker_diarization());

        assert_eq!(t.words[0].speaker, Some(0));
        assert_eq!(
            t.words[1].speaker,
            Some(0),
            "0.2 s inside spk0 beats 0 s inside spk1"
        );
        assert_eq!(t.words[2].speaker, Some(1));
    }

    #[test]
    fn a_word_in_the_gap_snaps_to_the_nearest_turn() {
        let mut t = Transcript {
            language: "en".into(),
            // 5.4 s falls in the 1 s hole between the two turns, 0.4 s past speaker 0's end
            // and 0.6 s before speaker 1's start.
            words: vec![word("uh", 5.3, 5.5)],
            segments: vec![],
        };
        assign_speakers(&mut t, &two_speaker_diarization());
        assert_eq!(
            t.words[0].speaker,
            Some(0),
            "nearest turn wins in a short gap"
        );
    }

    #[test]
    fn a_word_far_from_every_turn_stays_unassigned() {
        let mut t = Transcript {
            language: "en".into(),
            // 40 s in, with nobody talking within SPEAKER_SNAP_SECS: this is a music bed, not
            // a person. Guessing here would be worse than admitting we do not know.
            words: vec![word("la", 40.0, 40.5)],
            segments: vec![],
        };
        assign_speakers(&mut t, &two_speaker_diarization());
        assert_eq!(t.words[0].speaker, None);
    }

    #[test]
    fn segments_take_their_dominant_speaker() {
        let mut t = Transcript {
            language: "en".into(),
            words: vec![
                word("one", 0.0, 1.0),   // spk 0
                word("two", 1.0, 2.0),   // spk 0
                word("three", 6.0, 7.0), // spk 1
            ],
            // A segment that spans the turn boundary: 2 s of speaker 0 against 1 s of speaker 1.
            segments: vec![segment("one two three", 0.0, 7.0)],
        };
        assign_speakers(&mut t, &two_speaker_diarization());
        assert_eq!(
            t.segments[0].speaker,
            Some(0),
            "2 s of spk0 outweighs 1 s of spk1"
        );
    }

    #[test]
    fn empty_diarization_clears_speakers_and_does_not_panic() {
        let mut t = Transcript {
            language: "en".into(),
            words: vec![word("hi", 0.0, 0.5)],
            segments: vec![segment("hi", 0.0, 0.5)],
        };
        t.words[0].speaker = Some(7);
        assign_speakers(&mut t, &Diarization::default());
        assert_eq!(t.words[0].speaker, None);
        assert_eq!(t.segments[0].speaker, None);
    }

    #[test]
    fn assign_speakers_is_idempotent() {
        let diar = two_speaker_diarization();
        let mut t = Transcript {
            language: "en".into(),
            words: vec![word("hello", 0.2, 0.7), word("hi", 6.5, 7.0)],
            segments: vec![segment("hello", 0.2, 0.7), segment("hi", 6.5, 7.0)],
        };
        assign_speakers(&mut t, &diar);
        let once = t.clone();
        assign_speakers(&mut t, &diar);
        assert_eq!(t, once);
    }

    #[test]
    fn zero_length_word_inside_a_turn_still_gets_its_speaker() {
        let mut t = Transcript {
            language: "en".into(),
            words: vec![word("-", 3.0, 3.0)],
            segments: vec![],
        };
        assign_speakers(&mut t, &two_speaker_diarization());
        assert_eq!(t.words[0].speaker, Some(0));
    }

    #[test]
    fn diarization_json_is_in_contract_shape() {
        let d = two_speaker_diarization();
        let json = serde_json::to_string(&d).expect("serialize");
        assert!(json.contains("\"speakers\""));
        assert!(json.contains("\"segments\""));
        assert!(json.contains("\"label\":\"Speaker 1\""));
        assert!(json.contains("\"speaker\":0"));
        let back: Diarization = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(d, back);
    }

    #[test]
    fn default_options_auto_detect_speakers() {
        let opts = DiarizeOptions::default();
        assert!(opts.num_speakers.is_none(), "auto by default");
        assert_eq!(opts.cluster_threshold, 0.5);
    }

    #[test]
    fn non_wav_input_is_not_mistaken_for_16k_mono() {
        let dir = std::env::temp_dir().join(format!("anvil-asr-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");

        let not_wav = dir.join("nope.wav");
        std::fs::write(
            &not_wav,
            b"ID3\x04\x00\x00\x00\x00\x00\x00 this is an mp3 really",
        )
        .expect("write");
        assert!(!is_16k_mono_pcm_wav(&not_wav));

        // A well-formed 48 kHz stereo header must also be rejected — that is the case that
        // would otherwise reach sherpa and fail confusingly.
        let stereo = dir.join("48k-stereo.wav");
        let mut hdr = Vec::new();
        hdr.extend_from_slice(b"RIFF");
        hdr.extend_from_slice(&36u32.to_le_bytes());
        hdr.extend_from_slice(b"WAVEfmt ");
        hdr.extend_from_slice(&16u32.to_le_bytes());
        hdr.extend_from_slice(&1u16.to_le_bytes()); // PCM
        hdr.extend_from_slice(&2u16.to_le_bytes()); // stereo
        hdr.extend_from_slice(&48_000u32.to_le_bytes());
        hdr.extend_from_slice(&192_000u32.to_le_bytes());
        hdr.extend_from_slice(&4u16.to_le_bytes());
        hdr.extend_from_slice(&16u16.to_le_bytes());
        hdr.extend_from_slice(b"data");
        hdr.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&stereo, &hdr).expect("write");
        assert!(!is_16k_mono_pcm_wav(&stereo));

        // The real thing: same header at 16 kHz mono.
        let good = dir.join("16k-mono.wav");
        let mut hdr = Vec::new();
        hdr.extend_from_slice(b"RIFF");
        hdr.extend_from_slice(&36u32.to_le_bytes());
        hdr.extend_from_slice(b"WAVEfmt ");
        hdr.extend_from_slice(&16u32.to_le_bytes());
        hdr.extend_from_slice(&1u16.to_le_bytes());
        hdr.extend_from_slice(&1u16.to_le_bytes());
        hdr.extend_from_slice(&16_000u32.to_le_bytes());
        hdr.extend_from_slice(&32_000u32.to_le_bytes());
        hdr.extend_from_slice(&2u16.to_le_bytes());
        hdr.extend_from_slice(&16u16.to_le_bytes());
        hdr.extend_from_slice(b"data");
        hdr.extend_from_slice(&8u32.to_le_bytes());
        hdr.extend_from_slice(&[0u8; 8]);
        std::fs::write(&good, &hdr).expect("write");
        assert!(is_16k_mono_pcm_wav(&good));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
