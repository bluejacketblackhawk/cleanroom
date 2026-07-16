//! Music/SFX ducking under speech (03 §6).
//!
//! > "Music tracks duck under any speech VAD by `duck_db` (default −12, range −6..−24),
//! > lookahead 200 ms fade-down, 800 ms fade-up, hold 300 ms (no chatter between words)."
//!
//! The three time constants are the whole feature, and each one is there for a reason:
//!
//! - **Lookahead 200 ms.** The music must already be down *when* the first word lands, not
//!   200 ms later. The speech mask is dilated backwards by the fade-down time, so the ramp
//!   starts early and finishes exactly on the onset.
//! - **Hold 300 ms.** Speech has gaps — between words, between clauses, at every plosive. A
//!   gate that follows the VAD literally pumps the bed up and down inside a sentence. The
//!   hold bridges every gap shorter than 300 ms, which is most of them.
//! - **Fade-up 800 ms.** When the talking really is over, the bed comes back slowly enough
//!   that nobody notices it moving.
//!
//! The mask is a union over every speech track ("any speech VAD"), so a guest interrupting
//! keeps the bed down.

use serde::{Deserialize, Serialize};

use crate::vad::{db_to_lin, dilate_backward, dilate_forward, ms_to_frames};

/// Ducking tuning (03 §6). Defaults are the spec's numbers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DuckConfig {
    /// Duck depth in dB (03 §6: default −12, range −6..−24).
    pub duck_db: f32,
    /// Fade-down time, ms — and the lookahead, which must equal it so the bed is fully down
    /// exactly when speech starts.
    pub lookahead_ms: f32,
    /// Fade-up time, ms.
    pub fade_up_ms: f32,
    /// Hold the duck this long after speech stops (bridges the gaps between words).
    pub hold_ms: f32,
    /// A frame counts as speech this far above the track's noise floor.
    pub speech_margin_db: f32,
    /// VAD hangover, ms — smooths the mask before hold/lookahead are applied.
    pub hangover_ms: f32,
    /// VAD analysis hop, ms.
    pub hop_ms: f32,
    /// VAD analysis window, ms.
    pub window_ms: f32,
}

impl Default for DuckConfig {
    fn default() -> Self {
        Self {
            duck_db: -12.0,
            lookahead_ms: 200.0,
            fade_up_ms: 800.0,
            hold_ms: 300.0,
            speech_margin_db: 12.0,
            hangover_ms: 100.0,
            hop_ms: 10.0,
            window_ms: 20.0,
        }
    }
}

/// What the ducker did to one music track (mix report).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DuckReport {
    /// Depth applied, dB.
    pub duck_db: f32,
    /// Fraction of the track's samples spent ducked (gain ≥ 1 dB down).
    pub active_ratio: f32,
    /// Deepest reduction actually reached, dB.
    pub max_reduction_db: f32,
}

/// Per-sample ducking gain for a music track of `frames` samples, given a per-frame speech
/// mask at `hop` samples.
///
/// Returns the linear gain and the report.
pub fn duck_gain(
    speech: &[bool],
    frames: usize,
    hop: usize,
    sample_rate: u32,
    cfg: &DuckConfig,
) -> (Vec<f32>, DuckReport) {
    let depth = cfg.duck_db.min(0.0);
    if frames == 0 || speech.is_empty() || hop == 0 || depth >= 0.0 {
        return (
            vec![1.0; frames],
            DuckReport {
                duck_db: depth,
                active_ratio: 0.0,
                max_reduction_db: 0.0,
            },
        );
    }

    // Hangover → hold (forward) → lookahead (backward). All three are dilations, so the order
    // among them does not matter; what matters is that the bed is already down at the onset
    // and does not come back up inside a sentence.
    let mask = dilate_forward(speech, ms_to_frames(cfg.hangover_ms, hop, sample_rate));
    let mask = dilate_forward(&mask, ms_to_frames(cfg.hold_ms, hop, sample_rate));
    let mask = dilate_backward(&mask, ms_to_frames(cfg.lookahead_ms, hop, sample_rate));

    // dB-linear ramps: the fade-down spans exactly `lookahead_ms`, the fade-up `fade_up_ms`.
    let span = -depth;
    let down_per_sample = if cfg.lookahead_ms > 0.0 {
        span / (cfg.lookahead_ms * 1e-3 * sample_rate as f32)
    } else {
        span
    };
    let up_per_sample = if cfg.fade_up_ms > 0.0 {
        span / (cfg.fade_up_ms * 1e-3 * sample_rate as f32)
    } else {
        span
    };

    let mut gain = vec![1.0f32; frames];
    let mut cur = 0.0f32;
    let mut ducked = 0usize;
    let mut max_reduction = 0.0f32;
    for (i, g) in gain.iter_mut().enumerate() {
        let f = (i / hop).min(mask.len() - 1);
        let want = if mask[f] { depth } else { 0.0 };
        if want < cur {
            cur = (cur - down_per_sample).max(want);
        } else if want > cur {
            cur = (cur + up_per_sample).min(want);
        }
        *g = db_to_lin(cur);
        if cur <= -1.0 {
            ducked += 1;
        }
        if cur < max_reduction {
            max_reduction = cur;
        }
    }

    (
        gain,
        DuckReport {
            duck_db: depth,
            active_ratio: ducked as f32 / frames as f32,
            max_reduction_db: max_reduction,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfix::{at, music, Speech, SR};
    use crate::vad::{frame_levels_db, noise_floor_db, speech_mask};

    fn db(gain: &[f32], from: f64, to: f64) -> Vec<f32> {
        gain[at(from)..at(to).min(gain.len())]
            .iter()
            .map(|&g| 20.0 * g.max(1e-6).log10())
            .collect()
    }
    fn min_db(gain: &[f32], from: f64, to: f64) -> f32 {
        db(gain, from, to).into_iter().fold(f32::INFINITY, f32::min)
    }
    fn max_db(gain: &[f32], from: f64, to: f64) -> f32 {
        db(gain, from, to)
            .into_iter()
            .fold(f32::NEG_INFINITY, f32::max)
    }

    /// Test (d): the bed ducks under speech, holds through the gaps between words (no
    /// chatter), and comes back up once the talking really stops.
    #[test]
    fn music_ducks_under_speech_and_recovers_without_chattering() {
        let secs = 8.0;
        // −12 dB, 200 ms down, 800 ms up, 300 ms hold.
        let cfg = DuckConfig::default();
        // Speech from 1.0 s to 4.0 s: 450 ms words with 250 ms gaps. Then the mic keeps running
        // (noise floor, no words) — which is what a real mic does, and what the VAD's percentile
        // floor has to be estimated against.
        let speech_track = Speech {
            start: 1.0,
            end: 4.0,
            word: 0.45,
            gap: 0.25,
            ..Default::default()
        }
        .render(secs);
        let bed = music(secs, 0.2);

        let hop = at(cfg.hop_ms as f64 * 1e-3);
        let win = at(cfg.window_ms as f64 * 1e-3);
        let levels = frame_levels_db(&speech_track, win, hop);
        let mask = speech_mask(&levels, noise_floor_db(&levels), cfg.speech_margin_db);

        let (gain, report) = duck_gain(&mask, bed.len(), hop, SR, &cfg);

        // Before anyone speaks: bed at full level.
        assert!(
            min_db(&gain, 0.0, 0.75) >= -0.1,
            "the bed was ducked before anybody spoke ({:.1} dB)",
            min_db(&gain, 0.0, 0.75)
        );
        // The fade-down is a 200 ms *lookahead*: fully down by the first word, not after it.
        assert!(
            min_db(&gain, 1.0, 1.02) <= -11.5,
            "the bed was still up at the speech onset ({:.1} dB)",
            min_db(&gain, 1.0, 1.02)
        );
        // No chatter: across the whole speaking stretch — words *and* the 250 ms gaps between
        // them — the bed never comes back up. This is the assertion the 300 ms hold exists for.
        assert!(
            max_db(&gain, 1.0, 3.9) <= -11.0,
            "the bed chattered between words (rose to {:.1} dB)",
            max_db(&gain, 1.0, 3.9)
        );
        // And it recovers: mask ends ~4.0 s + 400 ms of hangover/hold + 800 ms fade-up.
        assert!(
            max_db(&gain, 6.0, 8.0) >= -0.1,
            "the bed never came back up ({:.1} dB)",
            max_db(&gain, 6.0, 8.0)
        );
        assert!((report.max_reduction_db - cfg.duck_db).abs() < 0.1);
        assert!(report.active_ratio > 0.3);
    }

    /// The duck depth is the parameter (03 §6: default −12, range −6..−24).
    #[test]
    fn duck_depth_follows_the_parameter() {
        let mask = vec![true; 400];
        for depth in [-6.0f32, -12.0, -24.0] {
            let cfg = DuckConfig {
                duck_db: depth,
                ..Default::default()
            };
            let (gain, report) = duck_gain(&mask, at(4.0), at(0.01), SR, &cfg);
            assert!((report.max_reduction_db - depth).abs() < 0.05);
            assert!((min_db(&gain, 1.0, 3.9) - depth).abs() < 0.05);
        }
    }

    /// No speech anywhere ⇒ the bed is untouched.
    #[test]
    fn silence_leaves_the_bed_alone() {
        let mask = vec![false; 400];
        let (gain, report) = duck_gain(&mask, at(4.0), at(0.01), SR, &DuckConfig::default());
        assert!(gain.iter().all(|&g| (g - 1.0).abs() < 1e-6));
        assert_eq!(report.active_ratio, 0.0);
    }
}
