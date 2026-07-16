//! Clip Studio render engine (M4, feature P19 — see handoff/04 §Clip Studio).
//!
//! Turns *a time range of the mastered audio + the whisper word timestamps* into a shareable
//! social MP4 with burned-in karaoke captions:
//!
//! ```text
//! AudioBuffer (mastered) ──slice[start,end]──┐
//!                                            ├─► ffmpeg sidecar ─► clip.mp4 (H.264 + AAC)
//! &[ClipWord] ──► ASS script (karaoke) ──────┘
//! ```
//!
//! ## How the captions are burned in
//! Captions are generated as an **ASS subtitle script** ([`caption_script`]) and burned into the
//! video by libass through ffmpeg's `ass` filter. Word-level highlighting is **not** done with
//! `\k` karaoke tags: `\k` renders a *progressive fill* (secondary → primary colour as the
//! syllable is sung), which is not the "the word being spoken is emphasized" look the feature
//! asks for, and its timing is only inspectable by re-rendering. Instead we emit **one Dialogue
//! event per word**: the event carries the whole caption line, with the active word wrapped in an
//! inline colour override (`{\c&H..&}word{\r}`), and it starts exactly at that word's timestamp
//! and ends at the next word's. That makes the highlight schedule a plain, greppable list of
//! timestamps (see `tests/clip.rs`) and it renders identically on every libass version.
//!
//! Only the colour changes on the active word — never the weight or the scale — so the line never
//! reflows or jitters as the highlight moves across it.
//!
//! **Timing accuracy.** ASS timestamps are centisecond-resolution (`H:MM:SS.CC`). We round each
//! word timestamp to the nearest centisecond, so the worst-case caption error is 5 ms — well
//! inside the ±1 frame (33.3 ms at [`CLIP_FPS`]) that 04's acceptance criterion allows.
//!
//! The title (if any) is a Dialogue event too, in its own top-anchored style, spanning the whole
//! clip — so libass is this module's *only* text-rendering dependency (no `drawtext`, no
//! freetype-in-the-filtergraph).
//!
//! ## Licensing (handoff/07 §2 — this is a hard constraint, not a preference)
//! ffmpeg is never linked; it is run as an external **LGPL** sidecar process, exactly like every
//! other encode path in this crate. The H.264 encoder is chosen at runtime from
//! [`LGPL_H264_ENCODERS`] — **OS encoders first** (Media Foundation on Windows, VideoToolbox on
//! macOS), then OpenH264 (Cisco, BSD), then the LGPL-safe hardware encoders. GPL-linked encoders
//! (x264/x265) are in [`GPL_VIDEO_ENCODERS`] and are **never** selected; if a caller tries to
//! force one via `ANVIL_H264_ENCODER`, [`FfmpegSidecar::h264_encoder`] refuses.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::encode::run_encode_child;
use crate::error::MediaError;
use crate::sidecar::FfmpegSidecar;
use crate::AudioBuffer;

/// Frame rate of every rendered clip. 30 fps is the social-platform baseline; ±1 frame (the 04
/// caption-accuracy budget) is 33.3 ms at this rate.
pub const CLIP_FPS: u32 = 30;

/// H.264 encoders that are safe in an LGPL-only ffmpeg build, in preference order (handoff/07 §2:
/// "OS encoders (Media Foundation H.264 on Win, VideoToolbox on Mac); if unavailable, fall back to
/// OpenH264 (Cisco, BSD); never bundle GPL x264").
pub const LGPL_H264_ENCODERS: &[&str] = &[
    // OS encoders — primary route, zero patent-licence surface for us.
    "h264_mf",
    "h264_videotoolbox",
    // Cisco OpenH264 (BSD) — the documented software fallback.
    "libopenh264",
    // Vendor hardware encoders: also LGPL-safe (no GPL component linked into ffmpeg).
    "h264_nvenc",
    "h264_qsv",
    "h264_amf",
];

/// Video encoders that force ffmpeg into `--enable-gpl`. ANVIL is MIT and ships an LGPL sidecar,
/// so these are never selected and are rejected if forced. See [`is_gpl_video_encoder`].
pub const GPL_VIDEO_ENCODERS: &[&str] = &["libx264", "libx264rgb", "libx265", "libxvid"];

/// Default clip background colour (near-black, the app's canvas).
const DEFAULT_BG_COLOR: &str = "#101014";
/// Default waveform trace colour.
const DEFAULT_WAVE_COLOR: &str = "#4CC9F0";

/// Caption line-breaking: at most this many words per line…
const MAX_LINE_WORDS: usize = 6;
/// …and at most this many characters, whichever hits first.
const MAX_LINE_CHARS: usize = 32;
/// A silent gap longer than this between two words starts a new caption line.
const LINE_BREAK_GAP: f64 = 0.7;
/// The last word of a line holds on screen this long after it ends (clamped to the next line).
const LINE_HOLD: f64 = 0.15;
/// Floor on a cue's on-screen time, so a degenerate zero-length word still renders.
const MIN_CUE_SECS: f64 = 0.02;

// ---------------------------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------------------------

/// Output shape of a clip. Every aspect renders at a 1080-px short edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aspect {
    /// 1:1 — 1080×1080 (feed posts).
    #[default]
    Square,
    /// 9:16 — 1080×1920 (Reels/Shorts/TikTok).
    Vertical,
    /// 16:9 — 1920×1080 (YouTube/X).
    Wide,
}

impl Aspect {
    /// Pixel canvas for this aspect: `(width, height)`.
    pub fn dimensions(self) -> (u32, u32) {
        match self {
            Aspect::Square => (1080, 1080),
            Aspect::Vertical => (1080, 1920),
            Aspect::Wide => (1920, 1080),
        }
    }

    /// Human label (`"1:1"`, `"9:16"`, `"16:9"`).
    pub fn ratio_label(self) -> &'static str {
        match self {
            Aspect::Square => "1:1",
            Aspect::Vertical => "9:16",
            Aspect::Wide => "16:9",
        }
    }

    /// Video bitrate for this canvas, in bits/s.
    fn video_bitrate(self) -> u32 {
        match self {
            Aspect::Wide => 8_000_000,
            _ => 6_000_000,
        }
    }
}

/// One of the three caption templates (04 §Clip Studio: "caption style (3 templates)").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptionStyle {
    /// Heavy white type, thick black outline, spoken word in yellow. The loud social default.
    #[default]
    Bold,
    /// Light type, hairline outline, spoken word in cyan. Quiet and editorial.
    Minimal,
    /// White type on an opaque dark slab, spoken word in green. Legible over any background.
    Boxed,
}

/// What sits behind the captions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Background {
    /// A flat colour (`#RRGGBB`).
    Solid { color: String },
    /// The clip's own audio, drawn as a waveform trace over a flat colour.
    Waveform { color: String, wave_color: String },
    /// The episode's cover art, filled to the canvas and dimmed so captions stay legible.
    CoverArt { path: PathBuf },
}

impl Default for Background {
    fn default() -> Self {
        Background::solid(DEFAULT_BG_COLOR)
    }
}

impl Background {
    /// A flat-colour background, e.g. `Background::solid("#101014")`.
    pub fn solid(color: impl Into<String>) -> Self {
        Background::Solid {
            color: color.into(),
        }
    }

    /// A waveform background with the default palette.
    pub fn waveform() -> Self {
        Background::Waveform {
            color: DEFAULT_BG_COLOR.to_string(),
            wave_color: DEFAULT_WAVE_COLOR.to_string(),
        }
    }

    /// A cover-art background. The image is scaled+cropped to fill the canvas and dimmed.
    pub fn cover_art(path: impl Into<PathBuf>) -> Self {
        Background::CoverArt { path: path.into() }
    }
}

/// One transcript word with its timestamps, in **seconds on the episode's timeline** (the same
/// shape as `anvil_asr::Word` — this crate takes the plain slice rather than depending on the ASR
/// crate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipWord {
    pub text: String,
    pub start: f64,
    pub end: f64,
}

impl ClipWord {
    pub fn new(text: impl Into<String>, start: f64, end: f64) -> Self {
        Self {
            text: text.into(),
            start,
            end,
        }
    }
}

/// Everything the clip editor collects before it hits Render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipSpec {
    /// Clip in-point, in seconds on the episode's timeline.
    pub start: f64,
    /// Clip out-point, in seconds on the episode's timeline.
    pub end: f64,
    #[serde(default)]
    pub aspect: Aspect,
    #[serde(default)]
    pub caption_style: CaptionStyle,
    #[serde(default)]
    pub background: Background,
    /// Optional title card, pinned to the top of the frame for the whole clip.
    #[serde(default)]
    pub title: Option<String>,
}

impl ClipSpec {
    /// A clip of `[start, end]` with the default aspect/style/background and no title.
    pub fn new(start: f64, end: f64) -> Self {
        Self {
            start,
            end,
            aspect: Aspect::default(),
            caption_style: CaptionStyle::default(),
            background: Background::default(),
            title: None,
        }
    }

    #[must_use]
    pub fn with_aspect(mut self, aspect: Aspect) -> Self {
        self.aspect = aspect;
        self
    }

    #[must_use]
    pub fn with_caption_style(mut self, style: CaptionStyle) -> Self {
        self.caption_style = style;
        self
    }

    #[must_use]
    pub fn with_background(mut self, background: Background) -> Self {
        self.background = background;
        self
    }

    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Requested clip length in seconds (`end - start`, never negative).
    pub fn duration(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }
}

/// One karaoke state of one caption line: the whole line, plus which word is lit right now.
/// Times are **seconds from the start of the clip**, not the episode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptionCue {
    pub start: f64,
    pub end: f64,
    /// Every word on the caption line, in order.
    pub line: Vec<String>,
    /// Index into [`CaptionCue::line`] of the word being spoken.
    pub active: usize,
}

impl CaptionCue {
    /// The word being spoken during this cue.
    pub fn active_word(&self) -> &str {
        self.line
            .get(self.active)
            .map(String::as_str)
            .unwrap_or_default()
    }
}

/// Whether `name` is a GPL-linked video encoder ANVIL must never use (handoff/07 §2).
pub fn is_gpl_video_encoder(name: &str) -> bool {
    GPL_VIDEO_ENCODERS
        .iter()
        .any(|gpl| gpl.eq_ignore_ascii_case(name.trim()))
}

/// The karaoke cue list for `words` under `spec` — the caption schedule, with no ffmpeg involved.
/// Words outside `[spec.start, spec.end]` are dropped; words straddling an edge are clipped.
/// Cue times are rebased to the start of the clip.
pub fn caption_cues(words: &[ClipWord], spec: &ClipSpec) -> Vec<CaptionCue> {
    cues_for_duration(words, spec.start, spec.duration())
}

/// The ASS subtitle script the sidecar burns in — the exact bytes handed to libass. Deterministic
/// and ffmpeg-free, so caption timing can be asserted without rendering a frame.
pub fn caption_script(words: &[ClipWord], spec: &ClipSpec) -> String {
    build_script(words, spec, spec.duration())
}

/// Refuse an inverted/empty `[spec.start, spec.end]`, or one with no audio inside it, without
/// touching ffmpeg. Pure Rust (slicing + arithmetic only — no process spawn, no filesystem
/// probe), so callers can run it *before* [`FfmpegSidecar::locate`]: a bad range is refused even
/// on a machine with no ffmpeg installed at all (see `tests/clip.rs`'s
/// `an_inverted_or_empty_range_is_refused_before_ffmpeg_runs`).
fn validate_clip_range(audio: &AudioBuffer, spec: &ClipSpec) -> Result<AudioBuffer, MediaError> {
    if spec.duration() <= 0.0 {
        return Err(MediaError::InvalidClip(format!(
            "clip end ({}) must be after clip start ({})",
            spec.end, spec.start
        )));
    }

    let clip_audio = slice_audio(audio, spec.start, spec.end);
    if clip_audio.frames() == 0 {
        return Err(MediaError::InvalidClip(format!(
            "no audio in [{}, {}] — the buffer holds {:.3} s",
            spec.start,
            spec.end,
            audio.frames() as f64 / f64::from(audio.sample_rate().max(1))
        )));
    }
    Ok(clip_audio)
}

/// Render `[spec.start, spec.end]` of `audio`, captioned with `words`, to an MP4 at `out`.
/// Validates the range before doing anything else, then locates the ffmpeg sidecar
/// automatically; see [`FfmpegSidecar::render_clip`].
pub fn render_clip(
    audio: &AudioBuffer,
    words: &[ClipWord],
    spec: &ClipSpec,
    out: &Path,
) -> Result<(), MediaError> {
    validate_clip_range(audio, spec)?;
    FfmpegSidecar::locate()?.render_clip(audio, words, spec, out)
}

/// [`render_clip`] with a completion-fraction callback in `[0, 1]`.
pub fn render_clip_with_progress(
    audio: &AudioBuffer,
    words: &[ClipWord],
    spec: &ClipSpec,
    out: &Path,
    progress: impl FnMut(f32),
) -> Result<(), MediaError> {
    validate_clip_range(audio, spec)?;
    FfmpegSidecar::locate()?.render_clip_with_progress(audio, words, spec, out, progress)
}

impl FfmpegSidecar {
    /// Render a captioned clip to `out` (an `.mp4`). See [`FfmpegSidecar::render_clip_with_progress`].
    pub fn render_clip(
        &self,
        audio: &AudioBuffer,
        words: &[ClipWord],
        spec: &ClipSpec,
        out: &Path,
    ) -> Result<(), MediaError> {
        self.render_clip_with_progress(audio, words, spec, out, |_| {})
    }

    /// Render a captioned clip to `out`, reporting a completion fraction in `[0, 1]`.
    ///
    /// The audio is sliced from the (already mastered) `audio` buffer in Rust and piped to ffmpeg
    /// as raw `f32le` — the same wire format every other encode path in this crate uses — so the
    /// clip carries the mastered audio verbatim, never a re-decode of the source file. If the
    /// buffer runs out before `spec.end`, the clip is shortened to the audio that exists (video
    /// and audio always end together).
    pub fn render_clip_with_progress(
        &self,
        audio: &AudioBuffer,
        words: &[ClipWord],
        spec: &ClipSpec,
        out: &Path,
        progress: impl FnMut(f32),
    ) -> Result<(), MediaError> {
        // Re-validated here (not just by the free functions above) so a caller that already
        // holds a located `FfmpegSidecar` and calls this method directly still gets the range
        // check before `spawn_clip_render` touches the filesystem or ffmpeg.
        let clip_audio = validate_clip_range(audio, spec)?;
        let duration = clip_audio.frames() as f64 / f64::from(clip_audio.sample_rate().max(1));

        // The .ass path goes into the filtergraph, where `:` and `\` are escape characters — and
        // every Windows path has a drive colon. Rather than double-escaping, we write the script
        // into the temp dir and run ffmpeg *with the temp dir as its working directory*, so the
        // filtergraph only ever sees a bare, punctuation-free filename.
        let dir = std::env::temp_dir();
        let name = format!(
            "anvil_clip_{}_{}.ass",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        std::fs::write(dir.join(&name), build_script(words, spec, duration))?;

        let result = self.spawn_clip_render(&clip_audio, spec, out, &dir, &name, duration);
        let result = result.and_then(|child| run_encode_child(child, &clip_audio, progress));
        let _ = std::fs::remove_file(dir.join(&name));
        result
    }

    /// Build and spawn the clip's ffmpeg child. Input 0 is the background video source, input 1 is
    /// the mastered PCM on stdin; the filtergraph composites them and burns in `ass_name`.
    fn spawn_clip_render(
        &self,
        clip_audio: &AudioBuffer,
        spec: &ClipSpec,
        out: &Path,
        work_dir: &Path,
        ass_name: &str,
        duration: f64,
    ) -> Result<std::process::Child, MediaError> {
        let encoder = self.h264_encoder()?;
        let (width, height) = spec.aspect.dimensions();
        let fps = CLIP_FPS.to_string();
        let dur = format!("{duration:.3}");
        // `out` (and any cover art) must be absolute: the child's cwd is the temp dir, not ours.
        let out_abs = absolutize(out)?;

        let mut cmd = Command::new(self.binary());
        cmd.current_dir(work_dir)
            .args(["-y", "-nostdin", "-hide_banner", "-loglevel", "error"]);

        // Input 0 — background.
        match &spec.background {
            Background::Solid { color } | Background::Waveform { color, .. } => {
                let c = ffmpeg_color(color)?;
                cmd.args(["-f", "lavfi", "-i"])
                    .arg(format!("color=c={c}:s={width}x{height}:r={fps}:d={dur}"));
            }
            Background::CoverArt { path } => {
                let art = absolutize(path)?;
                if !art.is_file() {
                    return Err(MediaError::InvalidClip(format!(
                        "cover art not found: {}",
                        art.display()
                    )));
                }
                cmd.args(["-loop", "1", "-framerate", &fps])
                    .arg("-i")
                    .arg(art);
            }
        }

        // Input 1 — the mastered clip audio, raw planar-interleaved f32.
        cmd.args(["-f", "f32le"])
            .args(["-ar", &clip_audio.sample_rate().to_string()])
            .args(["-ac", &clip_audio.channel_count().max(1).to_string()])
            .arg("-i")
            .arg("pipe:0");

        let (graph, audio_label) = filtergraph(spec, ass_name)?;
        cmd.args(["-filter_complex", &graph])
            .args(["-map", "[v]", "-map", &audio_label]);

        cmd.args(["-c:v", &encoder])
            .args(["-b:v", &spec.aspect.video_bitrate().to_string()])
            .args(["-pix_fmt", "yuv420p"])
            .args(["-r", &fps])
            .args(["-t", &dur])
            .args(["-c:a", "aac", "-b:a", "160k"])
            .args(["-movflags", "+faststart"])
            .args(["-progress", "pipe:2"])
            .arg(&out_abs)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        cmd.spawn().map_err(MediaError::from)
    }

    /// Pick the H.264 encoder to render with: the first entry of [`LGPL_H264_ENCODERS`] this
    /// ffmpeg build both *advertises* and can actually *run* (advertised ≠ usable — a build can
    /// list `h264_nvenc` on a machine with no NVIDIA GPU), so each candidate gets a one-frame
    /// smoke encode before we commit a whole render to it.
    ///
    /// `ANVIL_H264_ENCODER` overrides the search, but never past the licence bar: a GPL-linked
    /// encoder ([`is_gpl_video_encoder`]) is refused outright.
    pub fn h264_encoder(&self) -> Result<String, MediaError> {
        if let Some(forced) = std::env::var("ANVIL_H264_ENCODER")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            if is_gpl_video_encoder(&forced) {
                return Err(MediaError::InvalidClip(format!(
                    "refusing to encode with `{forced}`: it is GPL-linked, and ANVIL only ever \
                     drives an LGPL ffmpeg sidecar (handoff/07 §2)"
                )));
            }
            return Ok(forced);
        }

        let listing = self.encoder_listing()?;
        let mut advertised_but_broken = Vec::new();
        for candidate in LGPL_H264_ENCODERS {
            if !listing_advertises(&listing, candidate) {
                continue;
            }
            if self.encoder_smoke_test(candidate) {
                tracing::debug!(encoder = candidate, "clip render: selected H.264 encoder");
                return Ok((*candidate).to_string());
            }
            advertised_but_broken.push(*candidate);
        }

        Err(MediaError::SidecarFailed(format!(
            "no usable LGPL-safe H.264 encoder in this ffmpeg build (wanted one of {:?}; \
             advertised but failed to run: {:?}). ANVIL never falls back to GPL x264 — \
             see handoff/07 §2",
            LGPL_H264_ENCODERS, advertised_but_broken
        )))
    }

    /// `ffmpeg -encoders` stdout.
    fn encoder_listing(&self) -> Result<String, MediaError> {
        let output = Command::new(self.binary())
            .args(["-hide_banner", "-encoders"])
            .stdin(Stdio::null())
            .output()?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Encode one tiny synthetic frame with `encoder` and throw it away. `true` = it works here.
    fn encoder_smoke_test(&self, encoder: &str) -> bool {
        let status = Command::new(self.binary())
            .args(["-y", "-nostdin", "-hide_banner", "-loglevel", "error"])
            .args(["-f", "lavfi", "-i", "color=c=black:s=64x64:r=10:d=0.1"])
            .args(["-c:v", encoder, "-pix_fmt", "yuv420p"])
            .args(["-f", "null", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        matches!(status, Ok(s) if s.success())
    }
}

// ---------------------------------------------------------------------------------------------
// Filtergraph
// ---------------------------------------------------------------------------------------------

/// The `-filter_complex` string and the audio map label for `spec`. Video always lands on `[v]`;
/// the waveform background has to tee the audio through `asplit`, so its audio label differs.
fn filtergraph(spec: &ClipSpec, ass_name: &str) -> Result<(String, String), MediaError> {
    let (w, h) = spec.aspect.dimensions();
    // `ass_name` is a filename we generated (ASCII, no `:`/`\`/`'`), and ffmpeg runs with the
    // script's directory as its cwd — so no filtergraph escaping is needed here.
    let ass = format!("ass=filename={ass_name}");

    let graph = match &spec.background {
        Background::Solid { .. } => format!("[0:v]{ass}[v]"),
        Background::Waveform { wave_color, .. } => {
            let wave = ffmpeg_color(wave_color)?;
            // A band across the middle of the canvas, height rounded down to an even number
            // (yuv420p chroma subsampling needs even dimensions).
            let band = ((h / 3) & !1).max(2);
            format!(
                "[1:a]asplit=2[aout][awav];\
                 [awav]showwaves=s={w}x{band}:mode=cline:rate={fps}:colors={wave},format=rgba[wave];\
                 [0:v][wave]overlay=(W-w)/2:(H-h)/2[bgv];\
                 [bgv]{ass}[v]",
                fps = CLIP_FPS
            )
        }
        Background::CoverArt { .. } => format!(
            "[0:v]scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h},setsar=1,\
             drawbox=x=0:y=0:w={w}:h={h}:color=0x000000@0.45:t=fill[bgv];\
             [bgv]{ass}[v]"
        ),
    };

    let audio_label = match spec.background {
        Background::Waveform { .. } => "[aout]",
        _ => "1:a",
    };
    Ok((graph, audio_label.to_string()))
}

// ---------------------------------------------------------------------------------------------
// Caption schedule → ASS
// ---------------------------------------------------------------------------------------------

/// Build the cue list for a clip of `duration` seconds starting at `clip_start` on the episode's
/// timeline. Split out from [`caption_cues`] because a render may shorten `duration` when the
/// audio buffer ends early.
fn cues_for_duration(words: &[ClipWord], clip_start: f64, duration: f64) -> Vec<CaptionCue> {
    if duration <= 0.0 {
        return Vec::new();
    }
    let clip_end = clip_start + duration;

    // Clip to the range and rebase onto the clip's own timeline.
    let mut in_range: Vec<(String, f64, f64)> = words
        .iter()
        .filter(|w| w.end > clip_start && w.start < clip_end && !w.text.trim().is_empty())
        .map(|w| {
            let start = (w.start - clip_start).clamp(0.0, duration);
            let end = (w.end - clip_start).clamp(start, duration);
            (sanitize(&w.text), start, end)
        })
        .collect();
    in_range.sort_by(|a, b| a.1.total_cmp(&b.1));

    // Group into caption lines: too many words, too many characters, or too long a pause.
    let mut lines: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut chars = 0usize;
    for (i, word) in in_range.iter().enumerate() {
        let len = word.0.chars().count();
        if let Some(&prev) = current.last() {
            let gap = word.1 - in_range[prev].2;
            if current.len() >= MAX_LINE_WORDS
                || chars + 1 + len > MAX_LINE_CHARS
                || gap > LINE_BREAK_GAP
            {
                lines.push(std::mem::take(&mut current));
                chars = 0;
            }
        }
        chars += if current.is_empty() { len } else { len + 1 };
        current.push(i);
    }
    if !current.is_empty() {
        lines.push(current);
    }

    // One cue per word: the whole line, with that word lit, held until the next word starts.
    let mut cues = Vec::with_capacity(in_range.len());
    for (li, line) in lines.iter().enumerate() {
        let text: Vec<String> = line.iter().map(|&i| in_range[i].0.clone()).collect();
        let next_line_start = lines
            .get(li + 1)
            .and_then(|next| next.first())
            .map_or(duration, |&i| in_range[i].1);
        let last_end = in_range[*line.last().expect("lines are never empty")].2;
        let line_end = (last_end + LINE_HOLD).min(next_line_start).max(last_end);

        for (k, &i) in line.iter().enumerate() {
            let start = in_range[i].1;
            let end = match line.get(k + 1) {
                Some(&next) => in_range[next].1,
                None => line_end,
            };
            cues.push(CaptionCue {
                start,
                end: end.max(start + MIN_CUE_SECS),
                line: text.clone(),
                active: k,
            });
        }
    }
    cues
}

/// Colour/typography of one caption template. Sizes are fractions of the canvas's short edge so a
/// 9:16 and a 16:9 clip get visually identical type.
struct Template {
    font: &'static str,
    size_ratio: f64,
    bold: bool,
    primary_rgb: u32,
    highlight_rgb: u32,
    outline_rgb: u32,
    /// ASS alpha for the outline/box: `0x00` opaque, `0xFF` transparent.
    outline_alpha: u8,
    /// 1 = outline + drop shadow, 3 = opaque slab behind the text.
    border_style: u8,
    outline_ratio: f64,
    shadow_ratio: f64,
}

impl CaptionStyle {
    fn template(self) -> Template {
        match self {
            CaptionStyle::Bold => Template {
                font: "Arial",
                size_ratio: 0.058,
                bold: true,
                primary_rgb: 0xFFFFFF,
                highlight_rgb: 0xF2E14C,
                outline_rgb: 0x000000,
                outline_alpha: 0x00,
                border_style: 1,
                outline_ratio: 0.10,
                shadow_ratio: 0.0,
            },
            CaptionStyle::Minimal => Template {
                font: "Arial",
                size_ratio: 0.048,
                bold: false,
                primary_rgb: 0xE8E8EC,
                highlight_rgb: 0x4CC9F0,
                outline_rgb: 0x000000,
                outline_alpha: 0x60,
                border_style: 1,
                outline_ratio: 0.045,
                shadow_ratio: 0.03,
            },
            CaptionStyle::Boxed => Template {
                font: "Arial",
                size_ratio: 0.050,
                bold: true,
                primary_rgb: 0xFFFFFF,
                highlight_rgb: 0x3DDC97,
                outline_rgb: 0x101014,
                outline_alpha: 0x28,
                border_style: 3,
                outline_ratio: 0.22,
                shadow_ratio: 0.0,
            },
        }
    }
}

/// The full ASS script: `[Script Info]` sized to the canvas, a `Caption` + `Title` style from the
/// chosen template, then one Dialogue event per karaoke cue (plus the title, if any).
fn build_script(words: &[ClipWord], spec: &ClipSpec, duration: f64) -> String {
    let (w, h) = spec.aspect.dimensions();
    let t = spec.caption_style.template();
    let short = f64::from(w.min(h));

    let font_size = (short * t.size_ratio).round() as i64;
    let outline = (font_size as f64 * t.outline_ratio).round() as i64;
    let shadow = (font_size as f64 * t.shadow_ratio).round() as i64;
    let margin_h = (f64::from(w) * 0.07).round() as i64;
    let margin_v = (f64::from(h) * 0.10).round() as i64;
    let title_size = (short * 0.033).round() as i64;
    let title_outline = (title_size as f64 * 0.08).round() as i64;
    let title_margin_v = (f64::from(h) * 0.06).round() as i64;
    let bold = if t.bold { -1 } else { 0 };

    let primary = ass_color(t.primary_rgb, 0x00);
    let highlight = ass_color(t.highlight_rgb, 0x00);
    let outline_col = ass_color(t.outline_rgb, t.outline_alpha);
    let shadow_col = ass_color(0x000000, 0x64);
    let white = ass_color(0xFFFFFF, 0x00);
    let black = ass_color(0x000000, 0x00);

    let mut s = String::with_capacity(2048);
    s.push_str("[Script Info]\n");
    s.push_str("; Generated by ANVIL Clip Studio — do not edit, it is rewritten every render.\n");
    s.push_str("ScriptType: v4.00+\n");
    s.push_str(&format!("PlayResX: {w}\nPlayResY: {h}\n"));
    s.push_str("WrapStyle: 2\nScaledBorderAndShadow: yes\nYCbCr Matrix: None\n\n");

    s.push_str("[V4+ Styles]\n");
    s.push_str(
        "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, \
         BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, \
         BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n",
    );
    // Alignment 2 = bottom-centre (captions), 8 = top-centre (title).
    s.push_str(&format!(
        "Style: Caption,{font},{font_size},{primary},{highlight},{outline_col},{shadow_col},\
         {bold},0,0,0,100,100,0,0,{border},{outline},{shadow},2,{margin_h},{margin_h},{margin_v},1\n",
        font = t.font,
        border = t.border_style,
    ));
    s.push_str(&format!(
        "Style: Title,{font},{title_size},{white},{white},{black},{shadow_col},\
         -1,0,0,0,100,100,0,0,1,{title_outline},0,8,{margin_h},{margin_h},{title_margin_v},1\n\n",
        font = t.font,
    ));

    s.push_str("[Events]\n");
    s.push_str("Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");

    if let Some(title) = spec.title.as_ref().map(|t| sanitize(t)) {
        if !title.is_empty() {
            s.push_str(&format!(
                "Dialogue: 0,{},{},Title,,0,0,0,,{title}\n",
                ass_time(0.0),
                ass_time(duration)
            ));
        }
    }

    for cue in cues_for_duration(words, spec.start, duration) {
        // Only the colour changes on the active word: no weight/scale change means no reflow.
        let text = cue
            .line
            .iter()
            .enumerate()
            .map(|(i, word)| {
                if i == cue.active {
                    format!("{{\\c{highlight}&}}{word}{{\\r}}")
                } else {
                    word.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        s.push_str(&format!(
            "Dialogue: 0,{},{},Caption,,0,0,0,,{text}\n",
            ass_time(cue.start),
            ass_time(cue.end)
        ));
    }
    s
}

/// `H:MM:SS.CC` — ASS's centisecond timestamp. Rounded to nearest, so the worst-case error is
/// 5 ms, versus the 33.3 ms (±1 frame at [`CLIP_FPS`]) the acceptance criterion allows.
fn ass_time(secs: f64) -> String {
    let cs = (secs.max(0.0) * 100.0).round() as u64;
    format!(
        "{}:{:02}:{:02}.{:02}",
        cs / 360_000,
        (cs / 6_000) % 60,
        (cs / 100) % 60,
        cs % 100
    )
}

/// ASS colours are `&HAABBGGRR` — alpha first, then *reversed* RGB. `alpha` is 0 = opaque.
fn ass_color(rgb: u32, alpha: u8) -> String {
    let (r, g, b) = ((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);
    format!("&H{alpha:02X}{b:02X}{g:02X}{r:02X}")
}

/// Make `text` safe to drop into an ASS Dialogue's Text field: `{}` open/close override blocks and
/// `\` starts an override code, so neither may appear literally. Whitespace is collapsed (a word
/// must stay one word — the karaoke join depends on it). Commas are fine: Text is the last field.
fn sanitize(text: &str) -> String {
    let swapped: String = text
        .chars()
        .map(|c| match c {
            '{' => '(',
            '}' => ')',
            '\\' => '/',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    swapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Cut `[start, end]` (seconds) out of `audio`, at sample resolution. Out-of-range edges clamp.
fn slice_audio(audio: &AudioBuffer, start: f64, end: f64) -> AudioBuffer {
    let rate = audio.sample_rate();
    let rate_f = f64::from(rate.max(1));
    let frames = audio.frames();
    let from = ((start.max(0.0) * rate_f).round() as usize).min(frames);
    let to = ((end.max(0.0) * rate_f).round() as usize).clamp(from, frames);
    let planar = (0..audio.channel_count())
        .map(|c| audio.channel(c)[from..to].to_vec())
        .collect();
    AudioBuffer::from_planar(planar, rate)
}

/// `#RRGGBB` / `0xRRGGBB` / `RRGGBB` → ffmpeg's `0xRRGGBB` colour literal.
fn ffmpeg_color(value: &str) -> Result<String, MediaError> {
    Ok(format!("0x{:06X}", parse_hex_rgb(value)?))
}

fn parse_hex_rgb(value: &str) -> Result<u32, MediaError> {
    let trimmed = value.trim();
    let digits = trimmed
        .strip_prefix('#')
        .or_else(|| trimmed.strip_prefix("0x"))
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(MediaError::InvalidClip(format!(
            "colour `{value}` is not #RRGGBB"
        )));
    }
    u32::from_str_radix(digits, 16)
        .map_err(|e| MediaError::InvalidClip(format!("colour `{value}`: {e}")))
}

/// Absolute form of `path` — the render child runs with the temp dir as its cwd, so any relative
/// path a caller hands us would otherwise resolve against the wrong directory.
fn absolutize(path: &Path) -> Result<PathBuf, MediaError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Is `encoder` listed in `ffmpeg -encoders` output? Lines look like
/// ` V....D h264_mf              H264 via MediaFoundation (codec h264)`.
fn listing_advertises(listing: &str, encoder: &str) -> bool {
    listing
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(encoder))
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// One video frame, in seconds — the ±1 frame caption budget from 04's acceptance criteria.
    const FRAME_SECS: f64 = 1.0 / CLIP_FPS as f64;

    fn words() -> Vec<ClipWord> {
        vec![
            ClipWord::new("the", 10.00, 10.20),
            ClipWord::new("whole", 10.20, 10.55),
            ClipWord::new("point", 10.55, 10.90),
            ClipWord::new("is", 10.90, 11.05),
            ClipWord::new("local", 11.05, 11.60),
        ]
    }

    fn spec() -> ClipSpec {
        ClipSpec::new(10.0, 13.0)
    }

    #[test]
    fn cue_starts_match_word_starts_within_one_frame() {
        let words = words();
        let cues = caption_cues(&words, &spec());
        assert_eq!(cues.len(), words.len());
        for (cue, word) in cues.iter().zip(&words) {
            let expected = word.start - 10.0;
            assert!(
                (cue.start - expected).abs() < FRAME_SECS,
                "cue for {:?} started at {} s, word at {} s",
                word.text,
                cue.start,
                expected
            );
            assert_eq!(cue.active_word(), word.text);
        }
    }

    #[test]
    fn ass_timestamps_land_within_one_frame_of_the_word() {
        let words = words();
        let script = caption_script(&words, &spec());
        let starts: Vec<f64> = script
            .lines()
            .filter(|l| l.starts_with("Dialogue:") && l.contains(",Caption,"))
            .map(|l| parse_ass_time(l.split(',').nth(1).unwrap()))
            .collect();
        assert_eq!(starts.len(), words.len());
        for (rendered, word) in starts.iter().zip(&words) {
            let expected = word.start - 10.0;
            assert!(
                (rendered - expected).abs() <= FRAME_SECS,
                "ASS start {rendered} vs word start {expected} (budget {FRAME_SECS})"
            );
        }
    }

    /// Round-trip helper: `H:MM:SS.CC` → seconds.
    fn parse_ass_time(t: &str) -> f64 {
        let mut parts = t.split(':');
        let h: f64 = parts.next().unwrap().parse().unwrap();
        let m: f64 = parts.next().unwrap().parse().unwrap();
        let s: f64 = parts.next().unwrap().parse().unwrap();
        h * 3600.0 + m * 60.0 + s
    }

    #[test]
    fn one_dialogue_per_word_lights_exactly_one_word() {
        let script = caption_script(&words(), &spec().with_title("Episode 12"));
        let captions: Vec<&str> = script
            .lines()
            .filter(|l| l.starts_with("Dialogue:") && l.contains(",Caption,"))
            .collect();
        assert_eq!(captions.len(), 5);
        for line in &captions {
            assert_eq!(line.matches("{\\c&H").count(), 1, "exactly one lit word");
            assert_eq!(line.matches("{\\r}").count(), 1, "and it resets after");
        }
        // The third cue lights "point", and nothing else.
        assert!(captions[2].ends_with("the whole {\\c&H004CE1F2&}point{\\r} is local"));
        assert!(script.contains("Dialogue: 0,0:00:00.00,0:00:03.00,Title,,0,0,0,,Episode 12"));
    }

    #[test]
    fn cues_are_contiguous_and_hold_after_the_last_word() {
        let cues = caption_cues(&words(), &spec());
        for pair in cues.windows(2) {
            assert!(
                (pair[0].end - pair[1].start).abs() < 1e-9,
                "cue {:?} should end where the next begins",
                pair[0].active_word()
            );
        }
        let last = cues.last().unwrap();
        assert!((last.end - (1.60 + LINE_HOLD)).abs() < 1e-9, "{}", last.end);
    }

    #[test]
    fn words_outside_the_range_are_dropped_and_edges_clipped() {
        let words = vec![
            ClipWord::new("before", 8.0, 9.5),
            ClipWord::new("straddling", 9.8, 10.4),
            ClipWord::new("inside", 11.0, 11.5),
            ClipWord::new("after", 13.5, 14.0),
        ];
        let cues = caption_cues(&words, &spec());
        let lit: Vec<&str> = cues.iter().map(CaptionCue::active_word).collect();
        assert_eq!(lit, ["straddling", "inside"]);
        assert_eq!(
            cues[0].start, 0.0,
            "the straddling word clips to the in-point"
        );
    }

    #[test]
    fn long_passages_break_into_lines_on_length_and_on_pauses() {
        let mut words: Vec<ClipWord> = (0..8)
            .map(|i| {
                ClipWord::new(
                    format!("word{i}"),
                    10.0 + f64::from(i) * 0.2,
                    10.0 + f64::from(i) * 0.2 + 0.15,
                )
            })
            .collect();
        // A 1.2 s pause before the last word must force a fresh line.
        words.push(ClipWord::new("after-the-pause", 13.0, 13.4));
        let spec = ClipSpec::new(10.0, 20.0);
        let cues = caption_cues(&words, &spec);
        assert_eq!(cues.len(), 9, "one cue per word, whatever the line breaks");
        assert!(cues.len() > cues[0].line.len(), "it did break into lines");
        for cue in &cues {
            assert!(cue.line.len() <= MAX_LINE_WORDS, "{:?}", cue.line);
            assert!(
                cue.line.join(" ").chars().count() <= MAX_LINE_CHARS,
                "{:?}",
                cue.line
            );
        }
        assert_eq!(
            cues.last().unwrap().line,
            ["after-the-pause"],
            "the 1.2 s pause starts a new line"
        );
    }

    #[test]
    fn braces_and_backslashes_cannot_escape_into_override_codes() {
        let words = vec![ClipWord::new("{\\pos(0,0)}evil", 10.0, 10.5)];
        let script = caption_script(&words, &spec());
        assert!(script.contains("(/pos(0,0))evil"));
        assert_eq!(
            script.matches("{\\c&H").count(),
            1,
            "the only override block is the one we wrote"
        );
    }

    #[test]
    fn styles_scale_to_the_canvas_and_all_three_templates_differ() {
        for aspect in [Aspect::Square, Aspect::Vertical, Aspect::Wide] {
            let (w, h) = aspect.dimensions();
            let script = caption_script(&[], &ClipSpec::new(0.0, 1.0).with_aspect(aspect));
            assert!(script.contains(&format!("PlayResX: {w}\nPlayResY: {h}")));
        }
        let highlights: Vec<String> = [
            CaptionStyle::Bold,
            CaptionStyle::Minimal,
            CaptionStyle::Boxed,
        ]
        .iter()
        .map(|s| ass_color(s.template().highlight_rgb, 0))
        .collect();
        assert_eq!(highlights.len(), 3);
        assert!(
            highlights
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                == 3,
            "the three templates must not look the same"
        );
        // Boxed is the only slab style (BorderStyle 3).
        assert_eq!(CaptionStyle::Boxed.template().border_style, 3);
    }

    #[test]
    fn ass_colours_are_alpha_then_reversed_rgb() {
        assert_eq!(ass_color(0xF2E14C, 0x00), "&H004CE1F2");
        assert_eq!(ass_color(0x000000, 0x64), "&H64000000");
    }

    #[test]
    fn ass_time_is_centisecond_rounded() {
        assert_eq!(ass_time(0.0), "0:00:00.00");
        assert_eq!(ass_time(1.005), "0:00:01.00"); // nearest cs; error ≤ 5 ms
        assert_eq!(ass_time(3661.239), "1:01:01.24");
    }

    #[test]
    fn every_background_builds_a_graph_that_burns_in_the_script() {
        let cases = [
            (Background::solid("#101014"), "1:a"),
            (Background::waveform(), "[aout]"),
            (Background::cover_art("art.png"), "1:a"),
        ];
        for (background, expected_audio) in cases {
            let spec = ClipSpec::new(0.0, 5.0).with_background(background);
            let (graph, audio) = filtergraph(&spec, "clip.ass").expect("graph");
            assert!(graph.contains("ass=filename=clip.ass"), "{graph}");
            assert!(graph.ends_with("[v]"), "{graph}");
            assert_eq!(audio, expected_audio);
        }
        let wave = filtergraph(
            &ClipSpec::new(0.0, 5.0).with_background(Background::waveform()),
            "clip.ass",
        )
        .unwrap()
        .0;
        assert!(wave.contains("showwaves=s=1080x360"), "{wave}");
    }

    #[test]
    fn a_bad_colour_is_rejected_before_ffmpeg_ever_runs() {
        assert!(ffmpeg_color("#zzzzzz").is_err());
        assert!(ffmpeg_color("#12345").is_err());
        assert_eq!(ffmpeg_color("#4CC9F0").unwrap(), "0x4CC9F0");
        assert_eq!(ffmpeg_color("0x4cc9f0").unwrap(), "0x4CC9F0");
    }

    #[test]
    fn gpl_encoders_are_named_and_refused() {
        assert!(is_gpl_video_encoder("libx264"));
        assert!(is_gpl_video_encoder("LIBX265"));
        assert!(!is_gpl_video_encoder("h264_mf"));
        assert!(!is_gpl_video_encoder("libopenh264"));
        for encoder in LGPL_H264_ENCODERS {
            assert!(
                !is_gpl_video_encoder(encoder),
                "{encoder} must be LGPL-safe to be in the preference list"
            );
        }
    }

    #[test]
    fn encoder_listing_is_matched_on_the_name_column() {
        let listing = " V....D libx264              libx264 H.264 (codec h264)\n \
                        V....D h264_mf              H264 via MediaFoundation (codec h264)\n";
        assert!(listing_advertises(listing, "h264_mf"));
        assert!(!listing_advertises(listing, "libopenh264"));
        // "h264" appears inside descriptions everywhere — only the name column counts.
        assert!(!listing_advertises(listing, "h264"));
    }

    #[test]
    fn slicing_cuts_the_audio_at_sample_resolution() {
        let audio = AudioBuffer::from_planar(vec![(0..48_000).map(|i| i as f32).collect()], 48_000);
        let clip = slice_audio(&audio, 0.25, 0.75);
        assert_eq!(clip.frames(), 24_000);
        assert_eq!(clip.channel(0)[0], 12_000.0);
        // Past the end clamps rather than panicking.
        assert_eq!(slice_audio(&audio, 0.9, 5.0).frames(), 4_800);
        assert_eq!(slice_audio(&audio, 5.0, 6.0).frames(), 0);
    }

    #[test]
    fn spec_round_trips_through_snake_case_json() {
        let spec = ClipSpec::new(12.5, 42.0)
            .with_aspect(Aspect::Vertical)
            .with_caption_style(CaptionStyle::Boxed)
            .with_background(Background::waveform())
            .with_title("Ep. 12 — the local bit");
        let json = serde_json::to_string(&spec).expect("serialize");
        assert!(json.contains(r#""aspect":"vertical""#), "{json}");
        assert!(json.contains(r#""caption_style":"boxed""#), "{json}");
        assert!(json.contains(r#""type":"waveform""#), "{json}");
        assert_eq!(serde_json::from_str::<ClipSpec>(&json).unwrap(), spec);

        // Everything but the range is optional.
        let minimal: ClipSpec = serde_json::from_str(r#"{"start":0,"end":30}"#).unwrap();
        assert_eq!(minimal, ClipSpec::new(0.0, 30.0));
    }

    #[test]
    fn aspects_carry_their_ratios() {
        assert_eq!(Aspect::Square.dimensions(), (1080, 1080));
        assert_eq!(Aspect::Vertical.dimensions(), (1080, 1920));
        assert_eq!(Aspect::Wide.dimensions(), (1920, 1080));
        assert_eq!(Aspect::Vertical.ratio_label(), "9:16");
    }
}
