//! Crossgate / bleed control (03 §6) — the hard one, and the reason this lane exists.
//!
//! Two people, two mics. When A talks, A's voice also arrives at B's mic: a **delayed,
//! attenuated, reverberant copy** of A. Summed into the mix, that spill smears the stereo
//! image and doubles every consonant. The job is to duck B while it is only carrying A's
//! bleed — **and to never, ever touch B when B is actually speaking**, including the very
//! first syllable of B's interruption.
//!
//! # How bleed is told apart from B's own voice
//!
//! Bleed is not "quiet audio on B's mic" — it is *A's signal, again*. So we do not ask "is B
//! loud?", we ask **"how much of B's frame is explained by A?"**
//!
//! For each 100 ms frame we take the normalized cross-correlation between B's frame and A's
//! frame at the bleed lag, ρ = ⟨b, a_L⟩ / (‖b‖·‖a_L‖). ρ is the correlation coefficient, so
//! **ρ² is the fraction of B's energy that A's signal accounts for**, and the leftover
//!
//! ```text
//!     residual = ‖b‖²·(1 − ρ²)          ← B's *own* content, with A projected out
//! ```
//!
//! is what B's mic heard that A cannot explain. That residual is the VAD the veto runs on:
//!
//! - **Pure bleed** (B silent, A talking): the frame *is* a scaled copy of A, ρ → 1, the
//!   residual collapses to B's own noise floor. Nothing of B's is in there → duck.
//! - **B talking** (with or without A underneath): B's voice is uncorrelated with A's, so it
//!   survives the projection whole. The residual jumps 20–30 dB above B's floor → **veto**.
//!   Double-talk is the case that kills naive gates, and it falls out of the same number.
//!
//! Two cheap conditions guard the decision: A must actually be speaking (VAD-A) and must
//! **dominate** (bleed is attenuated: A's mic is louder than B's by `min_dominance_db`),
//! because a coherent-but-equal pair is two mics on one voice, not spill.
//!
//! # Never chopping the first syllable
//!
//! A gate that decides frame-by-frame is always late: by the time the residual has risen, the
//! onset it belongs to is already half attenuated. So the veto is **dilated in time** —
//! backwards by `lookahead_ms` (the gain is fully open *before* B's onset arrives) and
//! forwards by `veto_hold_ms` (we do not re-duck in the gaps between B's words). The 100 ms
//! analysis window helps too: a window starting up to 100 ms before an onset already contains
//! it. Ducking is offline here, so the lookahead is free — and the release ramp is bounded by
//! it, so the gate cannot still be closing when B's first syllable lands.

use serde::{Deserialize, Serialize};

use crate::align::gcc_phat;
use crate::vad::{
    db_to_lin, dilate_backward, dilate_forward, frame_levels_db, ms_to_frames, noise_floor_db,
    quiet_frame_floor_db, speech_mask,
};

/// Crossgate tuning (03 §6).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CrossgateConfig {
    /// Deepest duck applied to a bleed-only frame, dB (03 §6: "up to −15 dB").
    pub max_reduction_db: f32,
    /// Coherence window, ms (03 §6: "normalized cross-correlation per 100 ms").
    pub window_ms: f32,
    /// Decision hop, ms. Finer than the window so the gate can move inside a word.
    pub hop_ms: f32,
    /// ρ below this is not bleed — no duck. Above it, the duck fades in with ρ.
    pub coherence_threshold: f32,
    /// A's mic must be at least this much louder than B's for B's frame to be *spill*.
    pub min_dominance_db: f32,
    /// The residual (B's own content) must clear B's noise floor by this much to veto. Set
    /// above a reverberant bleed tail (which lands in the residual too) but below real speech.
    pub veto_margin_db: f32,
    /// …or the residual must be this loud *relative to B's frame* (dB). −6 dB = "a quarter of
    /// B's energy is unexplained". Either test vetoes: the bias is deliberately toward not
    /// ducking, because leaving bleed in is a blemish and chopping a syllable is a bug.
    pub veto_residual_ratio_db: f32,
    /// …but the ratio test only counts if the residual is *audible at all*: this far above B's
    /// noise floor. Without this, any frame where the correlation is merely mediocre (a word's
    /// attack ramp, say) reads as "75% unexplained" when the unexplained part is nothing but
    /// B's own hiss, and the gate would let the spill through for no reason.
    pub veto_floor_margin_db: f32,
    /// Once B has spoken, hold the veto this long so the gate does not re-close between words.
    pub veto_hold_ms: f32,
    /// The gate must be fully open this long before B's onset (and the release is bounded by
    /// it, so it always is).
    pub lookahead_ms: f32,
    /// Fade-down time to the full duck, ms.
    pub attack_ms: f32,
    /// Fade-up time back to unity, ms.
    pub release_ms: f32,
    /// A frame is "A speaking" this far above A's noise floor (VAD-A).
    pub speech_margin_db: f32,
    /// The acoustic path from A to B's mic is a fixed delay: search it once, up to this far.
    pub max_bleed_lag_ms: f32,
    /// How much material to use when estimating that fixed bleed lag, seconds.
    pub bleed_lag_window_secs: f64,
    /// Below this GCC-PHAT confidence, the pair has no bleed path at all — crossgate skipped.
    pub min_bleed_confidence: f32,
}

impl Default for CrossgateConfig {
    fn default() -> Self {
        Self {
            max_reduction_db: -15.0,
            window_ms: 100.0,
            hop_ms: 25.0,
            coherence_threshold: 0.6,
            min_dominance_db: 6.0,
            veto_margin_db: 18.0,
            veto_residual_ratio_db: -6.0,
            veto_floor_margin_db: 6.0,
            veto_hold_ms: 250.0,
            lookahead_ms: 150.0,
            attack_ms: 30.0,
            release_ms: 120.0,
            speech_margin_db: 12.0,
            max_bleed_lag_ms: 250.0,
            bleed_lag_window_secs: 60.0,
            min_bleed_confidence: 0.2,
        }
    }
}

/// What the crossgate did to one track (mix report).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossgateReport {
    /// The track whose bleed dominated this one (the source most often ducked against).
    pub source: Option<String>,
    /// Fraction of frames where the gate was ducking at all.
    pub active_ratio: f32,
    /// Fraction of frames where B's own speech vetoed a duck.
    pub veto_ratio: f32,
    /// Deepest gain reduction actually applied, dB (≤ 0).
    pub max_reduction_db: f32,
    /// Mean gain reduction over ducking frames, dB (≤ 0).
    pub mean_reduction_db: f32,
    /// Bleed path delay found by GCC-PHAT, ms (per source track, in input order).
    pub bleed_lag_ms: Vec<f32>,
}

/// The per-sample gain the crossgate wants applied to track B, plus its report.
#[derive(Debug, Clone)]
pub struct CrossgateResult {
    /// Linear gain, one per sample of B.
    pub gain: Vec<f32>,
    /// What happened.
    pub report: CrossgateReport,
}

/// Dot product and energies of `b[b0..b0+len]` against `a[a0..a0+len]`, zero-padded.
fn dot_energies(b: &[f32], a: &[f32], b0: usize, a0: i64, len: usize) -> (f64, f64, f64) {
    let (mut dot, mut ea, mut eb) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..len {
        let bi = b0 + i;
        let bv = if bi < b.len() { b[bi] as f64 } else { 0.0 };
        let ai = a0 + i as i64;
        let av = if ai >= 0 && (ai as usize) < a.len() {
            a[ai as usize] as f64
        } else {
            0.0
        };
        dot += av * bv;
        ea += av * av;
        eb += bv * bv;
    }
    (dot, ea, eb)
}

/// The fixed A→B bleed path delay, in samples, and how much we believe in it.
///
/// The path is a property of the room, not of the moment, so it is estimated once over a
/// chunk of material rather than searched per frame (which would be both slower and less
/// stable). Tracks reaching here are already aligned, so this lag is the *acoustic* delay —
/// small — and its GCC-PHAT confidence doubles as "is there any bleed on this pair at all".
fn bleed_lag(a: &[f32], b: &[f32], sample_rate: u32, cfg: &CrossgateConfig) -> (i64, f32) {
    let sr = sample_rate as f64;
    let n = a.len().min(b.len());
    if n == 0 {
        return (0, 0.0);
    }
    let win = ((cfg.bleed_lag_window_secs * sr) as usize).clamp(1, n);
    let max_shift = ((cfg.max_bleed_lag_ms as f64 * 1e-3 * sr) as usize).max(1);
    let lag = gcc_phat(&a[..win], &b[..win], max_shift);
    (lag.samples.round() as i64, lag.confidence)
}

/// Compute the crossgate gain for track `b` against every other speech track in `sources`.
///
/// `b` and each source are **mono, post-chain, already aligned** views of the tracks.
pub fn crossgate(
    b: &[f32],
    sources: &[(&str, &[f32])],
    sample_rate: u32,
    cfg: &CrossgateConfig,
) -> CrossgateResult {
    let n = b.len();
    let empty = CrossgateResult {
        gain: vec![1.0; n],
        report: CrossgateReport {
            source: None,
            active_ratio: 0.0,
            veto_ratio: 0.0,
            max_reduction_db: 0.0,
            mean_reduction_db: 0.0,
            bleed_lag_ms: Vec::new(),
        },
    };
    if n == 0 || sources.is_empty() || cfg.max_reduction_db >= 0.0 {
        return empty;
    }

    let win = (cfg.window_ms as f64 * 1e-3 * sample_rate as f64).round() as usize;
    let hop = (cfg.hop_ms as f64 * 1e-3 * sample_rate as f64).round() as usize;
    if win == 0 || hop == 0 {
        return empty;
    }

    // --- Per-frame levels, floors, and VAD-A ---------------------------------------------
    let b_levels = frame_levels_db(b, win, hop);
    let n_frames = b_levels.len();
    let mut a_levels: Vec<Vec<f32>> = Vec::with_capacity(sources.len());
    let mut a_active: Vec<Vec<bool>> = Vec::with_capacity(sources.len());
    let mut lags: Vec<i64> = Vec::with_capacity(sources.len());
    let mut lag_conf: Vec<f32> = Vec::with_capacity(sources.len());
    for (_, a) in sources {
        let lv = frame_levels_db(a, win, hop);
        let floor = noise_floor_db(&lv);
        a_active.push(speech_mask(&lv, floor, cfg.speech_margin_db));
        a_levels.push(lv);
        let (lag, conf) = bleed_lag(a, b, sample_rate, cfg);
        lags.push(lag);
        lag_conf.push(conf);
    }

    // B's own noise floor, measured only where nobody else is talking — see `quiet_frame_floor_db`.
    let others_active: Vec<bool> = (0..n_frames)
        .map(|f| a_active.iter().any(|m| m.get(f).copied().unwrap_or(false)))
        .collect();
    let b_floor = quiet_frame_floor_db(&b_levels, &others_active);

    // Lag refinement steps: the room's direct path drifts a little with head movement.
    let lag_step = (0.001 * sample_rate as f64).round() as i64;

    // --- Per-frame decision ---------------------------------------------------------------
    let mut target_db = vec![0.0f32; n_frames];
    let mut veto = vec![false; n_frames];
    let mut source_hits = vec![0usize; sources.len()];

    for f in 0..n_frames {
        let b0 = f * hop;
        let b_db = b_levels[f];
        // The last few windows hang off the end of the track. Correlating a truncated B
        // against a full-length A would read as "B has content A cannot explain" — which is
        // the veto condition — so shorten the window to what actually exists instead.
        let len = win.min(n - b0);
        if len * 4 < win {
            continue;
        }

        // Pick the source that best explains this frame of B, among those actually speaking.
        // Modelling one bleed source per frame is exact for a double-ender and a good
        // approximation beyond it (see the joint-projection TODO in the crate docs).
        let mut best: Option<(usize, f32, f64, f64)> = None; // (src, rho, eb, residual_energy)
        for (s, (_, a)) in sources.iter().enumerate() {
            if !a_active[s].get(f).copied().unwrap_or(false) {
                continue;
            }
            if lag_conf[s] < cfg.min_bleed_confidence {
                continue; // no bleed path on this pair at all
            }
            let mut best_rho = 0.0f32;
            let mut best_eb = 0.0f64;
            let mut best_res = 0.0f64;
            for step in [-lag_step, 0, lag_step] {
                let a0 = b0 as i64 - (lags[s] + step);
                let (dot, ea, eb) = dot_energies(b, a, b0, a0, len);
                if ea <= 1e-12 || eb <= 1e-12 {
                    continue;
                }
                let rho = (dot / (ea.sqrt() * eb.sqrt())).abs().min(1.0) as f32;
                if rho > best_rho {
                    best_rho = rho;
                    best_eb = eb;
                    // Energy of B once the best scalar multiple of A is projected out.
                    best_res = eb * (1.0 - (rho as f64) * (rho as f64));
                }
            }
            if best_rho > best.map(|x| x.1).unwrap_or(0.0) {
                best = Some((s, best_rho, best_eb, best_res));
            }
        }

        let Some((s, rho, eb, residual)) = best else {
            continue; // nobody else is talking → nothing to gate
        };
        if eb <= 1e-12 {
            continue;
        }

        // The VAD-B veto, on the *residual* rather than on B's raw level: this is what tells
        // "B is a copy of A" from "B has a voice of its own".
        let residual_db = 10.0 * ((residual / len as f64) as f32 + 1e-12).log10();
        let residual_ratio_db = 10.0 * ((residual / eb) as f32 + 1e-12).log10();
        let own_speech = residual_db > b_floor + cfg.veto_margin_db
            || (residual_db > b_floor + cfg.veto_floor_margin_db
                && residual_ratio_db > cfg.veto_residual_ratio_db);
        if own_speech {
            veto[f] = true;
            continue;
        }

        // Spill is attenuated: if B is as loud as A, it is not spill, it is a second mic on
        // the same voice (or B shouting) — leave it alone.
        let a_db = a_levels[s][f];
        if a_db - b_db < cfg.min_dominance_db || rho < cfg.coherence_threshold {
            continue;
        }

        let scale =
            ((rho - cfg.coherence_threshold) / (1.0 - cfg.coherence_threshold)).clamp(0.0, 1.0);
        target_db[f] = cfg.max_reduction_db * scale;
        source_hits[s] += 1;
    }

    // --- Time dilation: open early, stay open ---------------------------------------------
    let hold_frames = ms_to_frames(cfg.veto_hold_ms, hop, sample_rate);
    let look_frames = ms_to_frames(cfg.lookahead_ms, hop, sample_rate);
    let veto_ratio = if n_frames == 0 {
        0.0
    } else {
        veto.iter().filter(|&&v| v).count() as f32 / n_frames as f32
    };
    let veto = dilate_forward(&veto, hold_frames);
    let veto = dilate_backward(&veto, look_frames);
    for (f, &v) in veto.iter().enumerate() {
        if v {
            target_db[f] = 0.0;
        }
    }
    // …and the duck itself gets the same lookahead: a frame ducks only if every frame within
    // the lookahead also wants to duck. (Max over the window = "if anything ahead says open,
    // be open now".)
    let mut smoothed = target_db.clone();
    for f in 0..n_frames {
        let end = (f + look_frames + 1).min(n_frames);
        let mut m = target_db[f];
        for &t in &target_db[f..end] {
            if t > m {
                m = t;
            }
        }
        smoothed[f] = m;
    }

    // --- Ramps: dB-linear slew, attack down / release up -----------------------------------
    let depth = -cfg.max_reduction_db; // positive dB span
    let attack_per_sample = if cfg.attack_ms > 0.0 {
        depth / (cfg.attack_ms * 1e-3 * sample_rate as f32)
    } else {
        depth
    };
    let release_per_sample = if cfg.release_ms > 0.0 {
        depth / (cfg.release_ms * 1e-3 * sample_rate as f32)
    } else {
        depth
    };

    let mut gain = vec![1.0f32; n];
    let mut cur = 0.0f32; // current gain in dB
    let mut active_frames = 0usize;
    let mut reduction_sum = 0.0f32;
    let mut max_reduction = 0.0f32;
    for (i, g) in gain.iter_mut().enumerate() {
        let f = (i / hop).min(n_frames.saturating_sub(1));
        let want = smoothed[f];
        if want < cur {
            cur = (cur - attack_per_sample).max(want);
        } else if want > cur {
            cur = (cur + release_per_sample).min(want);
        }
        *g = db_to_lin(cur);
        if cur < max_reduction {
            max_reduction = cur;
        }
    }
    for &t in &smoothed {
        if t < 0.0 {
            active_frames += 1;
            reduction_sum += t;
        }
    }

    let source = source_hits
        .iter()
        .enumerate()
        .max_by_key(|(_, &h)| h)
        .filter(|(_, &h)| h > 0)
        .map(|(s, _)| sources[s].0.to_string());

    CrossgateResult {
        gain,
        report: CrossgateReport {
            source,
            active_ratio: if n_frames == 0 {
                0.0
            } else {
                active_frames as f32 / n_frames as f32
            },
            veto_ratio,
            max_reduction_db: max_reduction,
            mean_reduction_db: if active_frames == 0 {
                0.0
            } else {
                reduction_sum / active_frames as f32
            },
            bleed_lag_ms: lags
                .iter()
                .map(|&l| l as f32 * 1000.0 / sample_rate as f32)
                .collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfix::{add, at, delayed, noise, speechy, Speech, SR};

    /// dB of the gain curve over a time range.
    fn gain_db(gain: &[f32], from: f64, to: f64) -> Vec<f32> {
        gain[at(from)..at(to).min(gain.len())]
            .iter()
            .map(|&g| 20.0 * (g.max(1e-6)).log10())
            .collect()
    }
    fn min_db(gain: &[f32], from: f64, to: f64) -> f32 {
        gain_db(gain, from, to)
            .into_iter()
            .fold(f32::INFINITY, f32::min)
    }
    fn max_db(gain: &[f32], from: f64, to: f64) -> f32 {
        gain_db(gain, from, to)
            .into_iter()
            .fold(f32::NEG_INFINITY, f32::max)
    }

    /// The host's mic, and a guest mic carrying nothing but the host's spill.
    fn bleed_only(secs: f64) -> (Vec<f32>, Vec<f32>) {
        let a = speechy(secs, 3);
        let mut b = delayed(&a, 480, 0.1); // 10 ms path, −20 dB
        add(&mut b, &noise(secs, 0.0006, 11));
        (a, b)
    }

    /// Test (a), crossgate half — failure mode 1: the bleed is left in. It must be ducked.
    #[test]
    fn ducks_a_pure_bleed_track() {
        let (a, b) = bleed_only(6.0);
        let cfg = CrossgateConfig::default();
        // Measured: −14.96 dB deepest duck, 56.7% of frames ducking, 0% vetoed, path 10.0 ms.
        let res = crossgate(&b, &[("Host", &a)], SR, &cfg);
        assert!(
            min_db(&res.gain, 1.0, 5.0) <= -14.0,
            "bleed left in: deepest duck was only {:.1} dB",
            min_db(&res.gain, 1.0, 5.0)
        );
        assert!(
            res.report.active_ratio > 0.3,
            "the gate barely engaged ({:.0}% of frames)",
            res.report.active_ratio * 100.0
        );
        assert_eq!(
            res.report.veto_ratio, 0.0,
            "there is no B speech to veto on"
        );
        assert_eq!(res.report.source.as_deref(), Some("Host"));
        // The path delay it found is the one we built in (10 ms).
        assert!((res.report.bleed_lag_ms[0] - 10.0).abs() < 1.0);
    }

    /// Test (b) — failure mode 2: the gate chops B's first syllable. B starts talking at
    /// 3.0 s over the top of A; the gate must already be fully open when that syllable lands,
    /// and must stay open for as long as B is speaking.
    #[test]
    fn never_gates_bs_own_speech_onset() {
        let secs = 7.0;
        let a = speechy(secs, 3);
        let mut b = delayed(&a, 480, 0.1);
        add(&mut b, &noise(secs, 0.0006, 11));
        let own = Speech {
            start: 3.0,
            seed: 21,
            f0: 190.0,
            ..Default::default()
        }
        .render(secs);
        add(&mut b, &own);

        let cfg = CrossgateConfig::default();
        let res = crossgate(&b, &[("Host", &a)], SR, &cfg);

        // Measured: −14.95 dB through the spill, exactly 0.000 dB across B's onset *and* across
        // B's whole turn (the gate does not so much as breathe on it), 29.6% of frames vetoed.
        //
        // Before B speaks, the spill is ducked (otherwise this test proves nothing).
        assert!(
            min_db(&res.gain, 1.0, 2.5) <= -14.0,
            "bleed not ducked before the interruption ({:.1} dB)",
            min_db(&res.gain, 1.0, 2.5)
        );
        // The onset itself: fully open when B's first syllable arrives, and through it.
        assert!(
            min_db(&res.gain, 2.95, 3.30) >= -0.5,
            "B's first syllable was gated by {:.1} dB",
            min_db(&res.gain, 2.95, 3.30)
        );
        // Every later word onset too (B's words run 0.45 s on / 0.25 s off from 3.0 s).
        for k in 1..4 {
            let onset = 3.0 + 0.7 * k as f64;
            assert!(
                min_db(&res.gain, onset, onset + 0.1) >= -0.5,
                "B's word at {onset:.1} s was gated by {:.1} dB",
                min_db(&res.gain, onset, onset + 0.1)
            );
        }
        // And across B's whole turn the gate stays open — the veto hold bridges B's own gaps.
        assert!(
            min_db(&res.gain, 3.0, 6.5) >= -1.0,
            "the gate closed inside B's turn ({:.1} dB)",
            min_db(&res.gain, 3.0, 6.5)
        );
        assert!(res.report.veto_ratio > 0.2, "B's own speech should veto");
    }

    /// Two people on two mics with no spill between them: nothing to duck. (A gate that fires
    /// here would attenuate a perfectly good voice for no reason.)
    #[test]
    fn leaves_uncorrelated_tracks_alone() {
        let a = speechy(5.0, 3);
        let b = speechy(5.0, 31);
        let res = crossgate(&b, &[("Host", &a)], SR, &CrossgateConfig::default());
        assert!(
            min_db(&res.gain, 0.0, 5.0) >= -0.5,
            "an uncorrelated track was ducked by {:.1} dB",
            min_db(&res.gain, 0.0, 5.0)
        );
        assert_eq!(res.report.active_ratio, 0.0);
    }

    /// A quiet copy of A on B's mic is spill; an equally loud copy is two mics on one voice.
    /// The dominance test is what separates them.
    #[test]
    fn does_not_duck_a_track_that_is_as_loud_as_the_source() {
        let a = speechy(5.0, 3);
        let mut b = delayed(&a, 480, 0.95); // same voice, same level → not spill
        add(&mut b, &noise(5.0, 0.0006, 11));
        let res = crossgate(&b, &[("Host", &a)], SR, &CrossgateConfig::default());
        assert!(
            min_db(&res.gain, 0.5, 4.5) >= -0.5,
            "ducked a non-dominated track by {:.1} dB",
            min_db(&res.gain, 0.5, 4.5)
        );
    }

    /// The duck depth is the parameter, and it is honoured.
    #[test]
    fn duck_depth_follows_the_parameter() {
        let (a, b) = bleed_only(5.0);
        for depth in [-6.0f32, -15.0, -24.0] {
            let cfg = CrossgateConfig {
                max_reduction_db: depth,
                ..Default::default()
            };
            let res = crossgate(&b, &[("Host", &a)], SR, &cfg);
            let deepest = min_db(&res.gain, 1.0, 4.5);
            assert!(
                deepest <= depth + 1.5 && deepest >= depth - 0.1,
                "asked for {depth} dB, got {deepest:.1} dB"
            );
        }
    }

    /// The gate must never *boost*.
    #[test]
    fn gain_never_exceeds_unity() {
        let (a, b) = bleed_only(4.0);
        let res = crossgate(&b, &[("Host", &a)], SR, &CrossgateConfig::default());
        assert!(res.gain.iter().all(|&g| g <= 1.0 + 1e-6));
        assert!(max_db(&res.gain, 0.0, 4.0) <= 0.01);
    }
}
