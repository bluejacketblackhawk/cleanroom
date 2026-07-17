//! Clip Studio (04 §Clip Studio, M4, feature P19): select a range of the currently open
//! file → aspect + caption style + background + title → render an MP4.
//!
//! This module is a **thin wrapper over `anvil_media::render_clip`**. It does not compose a
//! filtergraph itself: it maps the UI's request onto an [`anvil_media::ClipSpec`], hands over
//! the audio and the whisper word timestamps, and forwards progress. That matters for two
//! reasons the app must not get wrong on its own:
//!
//! - **Licensing.** The engine picks an **LGPL-safe** H.264 encoder (`h264_mf` /
//!   `h264_videotoolbox` / `libopenh264`, …) and keeps `libx264`/`libx265` on a denylist that
//!   not even an env var can override (07 §video-encode: the shipped ffmpeg is LGPL-only, and
//!   Cleanroom is MIT). An app-local filtergraph hardcoding `-c:v libx264` would quietly make the
//!   product's video path GPL.
//! - **Caption fidelity.** The engine burns **per-word karaoke** captions in as ASS, timed to
//!   the word (measured 0-frame error), rather than showing a whole segment at a time.
//!
//! [`ClipRenderResult::seam_notes`] still surfaces honest downgrades (no transcript yet, or a
//! missing cover-art image) instead of silently swallowing them.

use std::path::PathBuf;

use anvil_media::{
    render_clip_with_progress, Aspect, Background, CaptionStyle, ClipSpec, ClipWord,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::transcript::TranscriptState;
use crate::AudioState;

/// Fallback background colour when the UI sends none (or sends nonsense).
const DEFAULT_BG_COLOR: &str = "#101014";
/// Waveform trace colour — the app's one accent.
const DEFAULT_WAVE_COLOR: &str = "#34d399";

// ---- wire types -------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ClipRange {
    pub start_secs: f64,
    pub end_secs: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClipBackground {
    /// "waveform" | "color" | "cover_art".
    pub kind: String,
    /// Hex colour (`#rrggbb`) — the solid fill, and the waveform's trace colour.
    pub color: Option<String>,
    /// Image file for "cover_art".
    pub cover_art_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClipRenderRequest {
    pub range: ClipRange,
    /// "1:1" | "9:16" | "16:9".
    pub aspect: String,
    /// "clean" | "bold" | "minimal".
    pub caption_style: String,
    pub captions_enabled: bool,
    pub background: ClipBackground,
    pub title: String,
    pub out_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClipRenderResult {
    pub ok: bool,
    pub path: String,
    pub message: Option<String>,
    /// Honest fidelity disclosures. Empty when nothing was downgraded.
    pub seam_notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ClipProgressEvent {
    fraction: f32,
}

// ---- request -> engine mapping -----------------------------------------------------------

fn map_aspect(aspect: &str) -> Aspect {
    match aspect {
        "1:1" => Aspect::Square,
        "9:16" => Aspect::Vertical,
        _ => Aspect::Wide, // "16:9" and anything unrecognized
    }
}

/// The UI ships three template names; map them onto the engine's three styles.
fn map_caption_style(style: &str) -> CaptionStyle {
    match style {
        "bold" => CaptionStyle::Bold,
        "minimal" => CaptionStyle::Minimal,
        _ => CaptionStyle::Boxed, // "clean" (the default template) and anything unrecognized
    }
}

fn is_hex_color(s: &str) -> bool {
    let s = s.trim_start_matches('#');
    s.len() == 6 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Map the wire background onto the engine's, pushing a seam note when cover art is requested
/// but unusable (a missing image is a fallback, not a crash and not a silent success).
fn map_background(bg: &ClipBackground, seam_notes: &mut Vec<String>) -> Background {
    let color = bg
        .color
        .as_deref()
        .filter(|c| is_hex_color(c))
        .unwrap_or(DEFAULT_BG_COLOR)
        .to_string();

    match bg.kind.as_str() {
        "color" => Background::Solid { color },
        "cover_art" => {
            let path = bg.cover_art_path.as_deref().unwrap_or("").trim();
            if path.is_empty() || !std::path::Path::new(path).is_file() {
                seam_notes.push(
                    "No cover art image was found — used a plain colour background instead."
                        .to_string(),
                );
                Background::Solid { color }
            } else {
                Background::CoverArt {
                    path: PathBuf::from(path),
                }
            }
        }
        _ => Background::Waveform {
            color,
            wave_color: DEFAULT_WAVE_COLOR.to_string(),
        },
    }
}

// ---- command ------------------------------------------------------------------------------

/// Render the selected range of the currently open file (mastered take preferred) to an MP4.
/// Never reports `ok: true` unless the engine actually wrote the file.
#[tauri::command]
pub fn clip_studio_render(
    request: ClipRenderRequest,
    app: AppHandle,
    state: State<'_, AudioState>,
    tstate: State<'_, TranscriptState>,
) -> ClipRenderResult {
    match render(&request, &app, &state, &tstate) {
        Ok(result) => result,
        Err(message) => ClipRenderResult {
            ok: false,
            path: request.out_path.clone(),
            message: Some(message),
            seam_notes: Vec::new(),
        },
    }
}

fn render(
    request: &ClipRenderRequest,
    app: &AppHandle,
    state: &AudioState,
    tstate: &TranscriptState,
) -> Result<ClipRenderResult, String> {
    let mut seam_notes = Vec::new();

    // Prefer the mastered take — a clip should sound like the finished episode.
    let source = {
        let processed = state
            .processed
            .read()
            .map_err(|_| "audio lock poisoned")?
            .clone();
        match processed {
            Some(buf) => buf,
            None => state
                .original
                .read()
                .map_err(|_| "audio lock poisoned")?
                .clone()
                .ok_or_else(|| "open a file before rendering a clip".to_string())?,
        }
    };

    if !(request.range.start_secs.is_finite() && request.range.end_secs.is_finite())
        || request.range.end_secs <= request.range.start_secs
    {
        return Err("select a valid range (end after start) before rendering".to_string());
    }

    // Word-level timestamps drive the karaoke highlight. The engine clips them to the range.
    let words: Vec<ClipWord> = if request.captions_enabled {
        match tstate.snapshot() {
            Some(t) if !t.words.is_empty() => t
                .words
                .iter()
                .map(|w| ClipWord::new(w.text.clone(), w.start, w.end))
                .collect(),
            _ => {
                seam_notes.push(
                    "No transcript yet, so the clip has no captions — transcribe the file first, \
                     then render again."
                        .to_string(),
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let mut spec = ClipSpec::new(request.range.start_secs, request.range.end_secs)
        .with_aspect(map_aspect(&request.aspect))
        .with_caption_style(map_caption_style(&request.caption_style))
        .with_background(map_background(&request.background, &mut seam_notes));
    if !request.title.trim().is_empty() {
        spec = spec.with_title(request.title.clone());
    }

    let out_path = PathBuf::from(&request.out_path);
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }

    // Render to a sibling temp file and only rename onto `out_path` once ffmpeg finishes
    // without error (05 §M5.F crash recovery) — a crash or forced quit mid-render never
    // leaves a truncated MP4 sitting at the path the user chose.
    let tmp_path = crate::export::temp_sibling(&out_path);
    let app_handle = app.clone();
    let render_result =
        render_clip_with_progress(&source, &words, &spec, &tmp_path, move |fraction| {
            let _ = app_handle.emit("clip://progress", ClipProgressEvent { fraction });
        });
    if let Err(e) = render_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.to_string());
    }
    if let Err(e) = std::fs::rename(&tmp_path, &out_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "rendered the clip but could not finalize it at {}: {e}",
            out_path.display()
        ));
    }

    Ok(ClipRenderResult {
        ok: true,
        path: out_path.to_string_lossy().into_owned(),
        message: None,
        seam_notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_maps_the_three_supported_ratios() {
        assert_eq!(map_aspect("1:1"), Aspect::Square);
        assert_eq!(map_aspect("9:16"), Aspect::Vertical);
        assert_eq!(map_aspect("16:9"), Aspect::Wide);
        // Unknown values default sanely rather than erroring.
        assert_eq!(map_aspect("weird"), Aspect::Wide);
    }

    #[test]
    fn caption_styles_map_to_the_three_engine_templates() {
        assert_eq!(map_caption_style("bold"), CaptionStyle::Bold);
        assert_eq!(map_caption_style("minimal"), CaptionStyle::Minimal);
        assert_eq!(map_caption_style("clean"), CaptionStyle::Boxed);
    }

    #[test]
    fn is_hex_color_validates_six_digit_hex() {
        assert!(is_hex_color("#101014"));
        assert!(is_hex_color("2dd4bf"));
        assert!(!is_hex_color("#12"));
        assert!(!is_hex_color("not-a-color"));
    }

    #[test]
    fn background_color_uses_the_requested_hex() {
        let mut notes = Vec::new();
        let bg = ClipBackground {
            kind: "color".into(),
            color: Some("#ff00aa".into()),
            cover_art_path: None,
        };
        match map_background(&bg, &mut notes) {
            Background::Solid { color } => assert_eq!(color, "#ff00aa"),
            other => panic!("expected a solid background, got {other:?}"),
        }
        assert!(notes.is_empty());
    }

    #[test]
    fn background_rejects_a_nonsense_colour_rather_than_passing_it_to_ffmpeg() {
        let mut notes = Vec::new();
        let bg = ClipBackground {
            kind: "color".into(),
            color: Some("dropTable".into()),
            cover_art_path: None,
        };
        match map_background(&bg, &mut notes) {
            Background::Solid { color } => assert_eq!(color, DEFAULT_BG_COLOR),
            other => panic!("expected a solid background, got {other:?}"),
        }
    }

    #[test]
    fn cover_art_falls_back_to_colour_when_the_image_is_missing() {
        let mut notes = Vec::new();
        let bg = ClipBackground {
            kind: "cover_art".into(),
            color: None,
            cover_art_path: Some("Z:\\does\\not\\exist.jpg".into()),
        };
        match map_background(&bg, &mut notes) {
            Background::Solid { .. } => {}
            other => panic!("expected the colour fallback, got {other:?}"),
        }
        assert_eq!(
            notes.len(),
            1,
            "the downgrade must be disclosed, not silent"
        );
    }
}
