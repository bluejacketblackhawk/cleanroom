//! Per-track chains, the multitrack stage, and the mixdown (03 §3, §6, §4.9, §4.10).
//!
//! The chain order the spec asks for is
//!
//! ```text
//!   per track: DC/HPF → de-hum → repair → denoise → breath → de-ess → AutoEQ → leveler
//!   then:      [multitrack: crossgate / ducking / mix]
//!   then:      master bus: two-pass loudness normalize → true-peak limiter
//! ```
//!
//! so the loudness math happens **once, on the sum** — not per track. That is why this module
//! runs the §4 modules itself rather than calling [`anvil_dsp::Chain::render`] (which is the
//! single-file path and ends with its own normalize + limiter): a limiter on every track,
//! followed by another on the bus, would squash each voice against a ceiling it never
//! reaches in the mix. The auto-decision ([`anvil_dsp::auto_configure`]) is reused verbatim,
//! so a speech track in a multitrack session gets exactly the decisions it would get alone.

use anvil_dsp::chain::measure_loudness;
use anvil_dsp::{
    analyze_buffer, auto_configure, AutoEq, BreathControl, ChainConfig, DcHpf, DcHpfConfig, DeClip,
    DeCrackle, DeEsser, DeHum, Denoiser, HealthFinding, HpfMode, LimiterConfig, LoudnessSnapshot,
    MouthDeClick, Processor, TruePeakLimiter,
};
use anvil_media::{decode_to_buffer, AudioBuffer};
use anvil_project::{Preset, PROJECT_SCHEMA_VERSION};
use serde::{Deserialize, Serialize};

use crate::align::{align_buffers, apply_offset, repair_drift, Alignment};
use crate::crossgate::{crossgate, CrossgateReport};
use crate::duck::{duck_gain, DuckReport};
use crate::error::MultitrackError;
use crate::track::{MultitrackOptions, Track, TrackKind};
use crate::vad::{db_to_lin, frame_levels_db, mono, noise_floor_db, speech_mask};

/// What happened to one track on its way into the mix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackReport {
    /// Track name.
    pub name: String,
    /// Speech or music.
    pub kind: TrackKind,
    /// Measured lag behind the reference track, seconds (0 for the reference).
    pub offset_secs: f64,
    /// Measured clock drift, ppm.
    pub drift_ppm: f64,
    /// Whether the drift was actually repaired by resampling (≤ 50 ppm and confident).
    pub drift_repaired: bool,
    /// Alignment confidence, 0..1.
    pub alignment_confidence: f32,
    /// The user's gain offset, dB.
    pub gain_db: f32,
    /// Whether the track reached the mixdown (solo/mute).
    pub audible: bool,
    /// Crossgate outcome (speech tracks only).
    pub crossgate: Option<CrossgateReport>,
    /// Ducking outcome (music tracks only).
    pub duck: Option<DuckReport>,
}

/// The multitrack report (contract: serde, snake_case).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MixReport {
    /// One entry per input track, in input order.
    pub tracks: Vec<TrackReport>,
    /// Loudness of the raw sum, before the master bus.
    pub before: LoudnessSnapshot,
    /// Loudness of the delivered mix.
    pub after: LoudnessSnapshot,
    /// Integrated loudness target, LUFS.
    pub target_lufs: f32,
    /// True-peak ceiling, dBTP.
    pub true_peak_ceiling_dbtp: f32,
    /// Static gain the master bus applied to hit the target, dB.
    pub master_gain_db: f64,
    /// Plain-language findings (the multitrack half of the Health Card).
    pub health_card: Vec<HealthFinding>,
    /// Chain version this mix was produced with.
    pub chain_version: u32,
}

/// The mixdown: audio + the alignment it was built on + the report.
#[derive(Debug, Clone)]
pub struct MixResult {
    /// The mastered mix (planar f32 @ 48 kHz).
    pub audio: AudioBuffer,
    /// The measured alignment of the input tracks.
    pub alignment: Alignment,
    /// What happened.
    pub report: MixReport,
}

fn info(title: &str, detail: String) -> HealthFinding {
    HealthFinding {
        severity: "info".into(),
        title: title.into(),
        detail,
        fix: None,
    }
}

fn warn(title: &str, detail: String, fix: Option<&str>) -> HealthFinding {
    HealthFinding {
        severity: "warn".into(),
        title: title.into(),
        detail,
        fix: fix.map(str::to_string),
    }
}

/// Decode every track, then mix (03 §6).
pub fn mix(tracks: &[Track], opts: &MultitrackOptions) -> Result<MixResult, MultitrackError> {
    if tracks.is_empty() {
        return Err(MultitrackError::NoTracks);
    }
    let mut buffers = Vec::with_capacity(tracks.len());
    for t in tracks {
        let buf = decode_to_buffer(&t.path).map_err(|source| MultitrackError::Decode {
            name: t.name.clone(),
            source,
        })?;
        if buf.is_empty() {
            return Err(MultitrackError::EmptyTrack(t.name.clone()));
        }
        buffers.push(buf);
    }
    mix_buffers(tracks, &buffers, opts)
}

/// Mix already-decoded tracks (the app has them in memory after analysis; the tests build
/// them synthetically).
pub fn mix_buffers(
    tracks: &[Track],
    buffers: &[AudioBuffer],
    opts: &MultitrackOptions,
) -> Result<MixResult, MultitrackError> {
    if tracks.is_empty() {
        return Err(MultitrackError::NoTracks);
    }
    if tracks.len() != buffers.len() {
        return Err(MultitrackError::BufferCountMismatch {
            tracks: tracks.len(),
            buffers: buffers.len(),
        });
    }
    for (t, b) in tracks.iter().zip(buffers) {
        if b.is_empty() {
            return Err(MultitrackError::EmptyTrack(t.name.clone()));
        }
    }
    let any_solo = tracks.iter().any(|t| t.solo);
    let audible: Vec<bool> = tracks
        .iter()
        .map(|t| !t.mute && (!any_solo || t.solo))
        .collect();
    if !audible.iter().any(|&a| a) {
        return Err(MultitrackError::NothingAudible);
    }

    let sample_rate = buffers[0].sample_rate();
    let mut health: Vec<HealthFinding> = Vec::new();

    // --- 1. Alignment (§6) -----------------------------------------------------------------
    let alignment = if opts.align && tracks.len() > 1 {
        align_buffers(buffers, &opts.align_config)
    } else {
        Alignment::identity(tracks.len())
    };

    let mut drift_repaired = vec![false; tracks.len()];
    let mut aligned: Vec<AudioBuffer> = Vec::with_capacity(tracks.len());
    for (i, buf) in buffers.iter().enumerate() {
        let conf = alignment.confidence[i];
        let trusted = i == alignment.reference || conf >= opts.align_config.min_confidence;
        if !trusted {
            health.push(warn(
                "Alignment not confident",
                format!(
                    "\"{}\" does not correlate with \"{}\" (confidence {:.2}) — left where it is.",
                    tracks[i].name, tracks[alignment.reference].name, conf
                ),
                Some("Check the tracks really are the same session."),
            ));
            aligned.push(buf.clone());
            continue;
        }

        let ppm = alignment.drift_ppm[i];
        let mut b = buf.clone();
        if opts.drift_repair
            && ppm.abs() >= opts.align_config.min_repair_ppm
            && ppm.abs() <= opts.align_config.max_drift_ppm
        {
            b = repair_drift(&b, ppm);
            drift_repaired[i] = true;
            health.push(info(
                "Clock drift repaired",
                format!(
                    "\"{}\" drifts {ppm:+.1} ppm against \"{}\" — resampled to match.",
                    tracks[i].name, tracks[alignment.reference].name
                ),
            ));
        } else if ppm.abs() > opts.align_config.max_drift_ppm {
            health.push(warn(
                "Clock drift too large to repair",
                format!(
                    "\"{}\" drifts {ppm:+.1} ppm — beyond the {:.0} ppm resample limit.",
                    tracks[i].name, opts.align_config.max_drift_ppm
                ),
                Some("Re-export the track from the source recorder."),
            ));
        }

        let offset = (alignment.offsets_secs[i] * sample_rate as f64).round() as i64;
        if offset != 0 {
            b = apply_offset(&b, offset);
            health.push(info(
                "Track aligned",
                format!(
                    "\"{}\" was {:+.3} s off \"{}\" — shifted into place (confidence {:.2}).",
                    tracks[i].name,
                    alignment.offsets_secs[i],
                    tracks[alignment.reference].name,
                    conf
                ),
            ));
        }
        aligned.push(b);
    }

    // --- 2. Per-track chains (§6: "each speech track runs the §4 chain; music: light") ------
    let processed: Vec<AudioBuffer> = aligned
        .iter()
        .zip(tracks)
        .map(|(buf, t)| {
            if !opts.per_track_chain {
                return buf.clone();
            }
            match t.kind {
                TrackKind::Speech => speech_chain(buf, opts),
                TrackKind::Music => music_chain(buf),
            }
        })
        .collect();

    // Per-track user gain, applied after the chain (a mixing-desk fader, not a chain param).
    let processed: Vec<AudioBuffer> = processed
        .iter()
        .zip(tracks)
        .map(|(buf, t)| {
            if t.gain_db == 0.0 {
                return buf.clone();
            }
            let g = db_to_lin(t.gain_db);
            let channels: Vec<Vec<f32>> = buf
                .planar()
                .iter()
                .map(|c| c.iter().map(|&s| s * g).collect())
                .collect();
            AudioBuffer::from_planar(channels, buf.sample_rate())
        })
        .collect();

    // Mono analysis views for the multitrack stage. Muted tracks stay in here on purpose:
    // a muted mic's bleed is still sitting in everybody else's track, and the bed still has
    // to duck under a speaker whose fader happens to be down.
    let monos: Vec<Vec<f32>> = processed.iter().map(mono).collect();

    // --- 3. Crossgate (§6) ------------------------------------------------------------------
    let cg_cfg = opts.resolved_crossgate();
    let mut gains: Vec<Option<Vec<f32>>> = vec![None; tracks.len()];
    let mut crossgate_reports: Vec<Option<CrossgateReport>> = vec![None; tracks.len()];
    if opts.crossgate {
        for (i, t) in tracks.iter().enumerate() {
            if t.kind != TrackKind::Speech {
                continue;
            }
            let sources: Vec<(&str, &[f32])> = tracks
                .iter()
                .enumerate()
                .filter(|(j, s)| *j != i && s.kind == TrackKind::Speech)
                .map(|(j, s)| (s.name.as_str(), monos[j].as_slice()))
                .collect();
            if sources.is_empty() {
                continue;
            }
            let res = crossgate(&monos[i], &sources, sample_rate, &cg_cfg);
            if res.report.active_ratio > 0.0 {
                health.push(info(
                    "Bleed ducked",
                    format!(
                        "\"{}\" carries {} on {:.0}% of frames — ducked up to {:.0} dB \
                         ({:.0}% of frames vetoed as its own speech).",
                        t.name,
                        res.report.source.as_deref().unwrap_or("another speaker"),
                        res.report.active_ratio * 100.0,
                        -res.report.max_reduction_db,
                        res.report.veto_ratio * 100.0,
                    ),
                ));
            }
            crossgate_reports[i] = Some(res.report);
            gains[i] = Some(res.gain);
        }
    }

    // --- 4. Ducking (§6) --------------------------------------------------------------------
    let duck_cfg = opts.resolved_duck();
    let mut duck_reports: Vec<Option<DuckReport>> = vec![None; tracks.len()];
    let has_music = tracks.iter().any(|t| t.kind == TrackKind::Music);
    let has_speech = tracks.iter().any(|t| t.kind == TrackKind::Speech);
    if has_music && has_speech {
        let hop = (duck_cfg.hop_ms as f64 * 1e-3 * sample_rate as f64).round() as usize;
        let win = (duck_cfg.window_ms as f64 * 1e-3 * sample_rate as f64).round() as usize;
        let speech = union_speech_mask(
            &monos,
            tracks,
            win.max(1),
            hop.max(1),
            duck_cfg.speech_margin_db,
        );
        for (i, t) in tracks.iter().enumerate() {
            if t.kind != TrackKind::Music {
                continue;
            }
            let frames = processed[i].frames();
            let (gain, report) = duck_gain(&speech, frames, hop.max(1), sample_rate, &duck_cfg);
            if report.active_ratio > 0.0 {
                health.push(info(
                    "Music ducked under speech",
                    format!(
                        "\"{}\" ducks {:.0} dB while anyone is talking ({:.0}% of the track), \
                         200 ms fade-down / 800 ms fade-up with a 300 ms hold.",
                        t.name,
                        -report.duck_db,
                        report.active_ratio * 100.0
                    ),
                ));
            }
            duck_reports[i] = Some(report);
            gains[i] = Some(gain);
        }
    }

    // --- 5. Mixdown -------------------------------------------------------------------------
    let out_ch = opts.output_channels.clamp(1, 2);
    let frames = processed
        .iter()
        .zip(&audible)
        .filter(|(_, &a)| a)
        .map(|(b, _)| b.frames())
        .max()
        .unwrap_or(0);
    let mut sum = vec![vec![0.0f32; frames]; out_ch];
    for (i, buf) in processed.iter().enumerate() {
        if !audible[i] {
            continue;
        }
        let gain = gains[i].as_deref();
        for (c, out) in sum.iter_mut().enumerate() {
            let src = buf.channel(c.min(buf.channel_count() - 1));
            for (n, slot) in out.iter_mut().enumerate().take(src.len()) {
                let g = gain.and_then(|g| g.get(n)).copied().unwrap_or(1.0);
                *slot += src[n] * g;
            }
        }
    }
    let mixed = AudioBuffer::from_planar(sum, sample_rate);

    // --- 6. Master bus (§4.9 two-pass loudness + §4.10 true-peak limiter) --------------------
    let before = measure_loudness(&mixed);
    let limiter = LimiterConfig {
        ceiling_dbtp: opts.true_peak_ceiling_dbtp,
        ..Default::default()
    };
    let target = opts.target_lufs as f64;
    let apply = |gain_db: f64| -> AudioBuffer {
        let mut b = mixed.clone();
        if gain_db.is_finite() && gain_db != 0.0 {
            let g = db_to_lin(gain_db as f32);
            for ch in b.planar_mut() {
                for s in ch.iter_mut() {
                    *s *= g;
                }
            }
        }
        TruePeakLimiter::with_rate(limiter, sample_rate).process(&mut b);
        b
    };
    let l0 = before.integrated_lufs;
    let mut master_gain_db = if l0.is_finite() && l0 > -70.0 {
        target - l0
    } else {
        0.0
    };
    let mut out = apply(master_gain_db);
    let l1 = measure_loudness(&out).integrated_lufs;
    // Verify, and take one correction iteration when the limiter ate into loud program (§4.9).
    if l1.is_finite() && l1 > -70.0 && (target - l1).abs() > 0.5 {
        master_gain_db += target - l1;
        out = apply(master_gain_db);
    }
    let after = measure_loudness(&out);
    health.push(info(
        "Mix normalized",
        format!(
            "Sum of {} track(s) normalized {:.1} → {:.1} LUFS, true-peak limited to {:.1} dBTP.",
            audible.iter().filter(|&&a| a).count(),
            before.integrated_lufs,
            after.integrated_lufs,
            opts.true_peak_ceiling_dbtp
        ),
    ));

    let track_reports = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| TrackReport {
            name: t.name.clone(),
            kind: t.kind,
            offset_secs: alignment.offsets_secs[i],
            drift_ppm: alignment.drift_ppm[i],
            drift_repaired: drift_repaired[i],
            alignment_confidence: alignment.confidence[i],
            gain_db: t.gain_db,
            audible: audible[i],
            crossgate: crossgate_reports[i].clone(),
            duck: duck_reports[i].clone(),
        })
        .collect();

    Ok(MixResult {
        audio: out,
        alignment,
        report: MixReport {
            tracks: track_reports,
            before,
            after,
            target_lufs: opts.target_lufs,
            true_peak_ceiling_dbtp: opts.true_peak_ceiling_dbtp,
            master_gain_db,
            health_card: health,
            chain_version: anvil_core::CHAIN_VERSION,
        },
    })
}

/// Union of the per-track speech VADs ("any speech VAD" — 03 §6).
fn union_speech_mask(
    monos: &[Vec<f32>],
    tracks: &[Track],
    win: usize,
    hop: usize,
    margin_db: f32,
) -> Vec<bool> {
    let n_frames = monos
        .iter()
        .zip(tracks)
        .filter(|(_, t)| t.kind == TrackKind::Speech)
        .map(|(m, _)| m.len().div_ceil(hop))
        .max()
        .unwrap_or(0);
    let mut mask = vec![false; n_frames];
    for (m, _) in monos
        .iter()
        .zip(tracks)
        .filter(|(_, t)| t.kind == TrackKind::Speech)
    {
        let levels = frame_levels_db(m, win, hop);
        let floor = noise_floor_db(&levels);
        for (f, active) in speech_mask(&levels, floor, margin_db)
            .into_iter()
            .enumerate()
        {
            if active && f < mask.len() {
                mask[f] = true;
            }
        }
    }
    mask
}

/// The §4 speech chain, minus the loudness/limiter/dither tail (that runs once, on the bus).
fn speech_chain(buf: &AudioBuffer, opts: &MultitrackOptions) -> AudioBuffer {
    let report = analyze_buffer(buf);
    let preset = Preset {
        schema_version: PROJECT_SCHEMA_VERSION,
        name: "multitrack".into(),
        tier: opts.tier,
        target_lufs: opts.target_lufs,
        true_peak_ceiling_dbtp: opts.true_peak_ceiling_dbtp,
    };
    let mut config = auto_configure(&report, &preset, opts.tier);
    if !opts.denoise {
        config.denoise = None;
    }
    front_chain(buf, &config)
}

/// Run the front (pre-loudness) half of the §3 chain.
fn front_chain(buf: &AudioBuffer, config: &ChainConfig) -> AudioBuffer {
    let sr = buf.sample_rate();
    let mut b = buf.clone();
    let ch = b.channel_count();

    DcHpf::new(ch, sr, config.dc_hpf).process(&mut b);
    if let Some(cfg) = config.dehum {
        DeHum::new(ch, sr, cfg).process(&mut b);
    }
    if let Some(cfg) = config.declip {
        DeClip::new(sr, cfg).process(&mut b);
    }
    if let Some(cfg) = config.declick {
        MouthDeClick::new(sr, cfg).process(&mut b);
    }
    if let Some(cfg) = config.decrackle {
        DeCrackle::new(sr, cfg).process(&mut b);
    }
    if let Some(cfg) = config.denoise {
        // Tier-select the engine (Fast = RNNoise, Standard/Studio = DeepFilterNet3, 03 §4.4).
        // Without this every tier silently denoises identically on a multitrack mix. Falls back
        // to `Denoiser::new` (DFN3 with an RNNoise safety net) if the tier can't initialize.
        let tier = match config.tier {
            anvil_project::Tier::Fast => anvil_dsp::DenoiseTier::Fast,
            anvil_project::Tier::Standard => anvil_dsp::DenoiseTier::Standard,
            anvil_project::Tier::Studio => anvil_dsp::DenoiseTier::Studio,
        };
        Denoiser::try_with_tier(ch, cfg, tier)
            .unwrap_or_else(|_| Denoiser::new(ch, cfg))
            .process(&mut b);
    }
    if let Some(cfg) = config.breath {
        BreathControl::new(sr, cfg).process(&mut b);
    }
    if let Some(cfg) = config.deess {
        DeEsser::new(sr, cfg).process(&mut b);
    }
    if let Some(cfg) = config.autoeq {
        AutoEq::new(sr, cfg).process(&mut b);
    }
    // The leveler is the per-speaker balance (§6: "per-speaker leveling"): every mic arrives
    // at the bus already sitting on the same speech loudness, which is the thing a window-based
    // AGC on the *sum* can never do — it cannot tell a quiet guest from a soft passage.
    anvil_dsp::Leveler::new(sr, config.leveler).process(&mut b);
    b
}

/// The music/SFX light chain (03 §6: "music tracks: light chain (HPF/loudness prep only)").
///
/// No denoise (it would eat the bed), no leveler (music dynamics are intentional), no de-ess,
/// no breath control. Just the 40 Hz high-pass that keeps rumble out of the sum's headroom.
fn music_chain(buf: &AudioBuffer) -> AudioBuffer {
    let mut b = buf.clone();
    let cfg = DcHpfConfig {
        mode: HpfMode::Auto,
        cutoff_hz: 40.0,
        dc_block: true,
    };
    DcHpf::new(b.channel_count(), buf.sample_rate(), cfg).process(&mut b);
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfix::{add, buffer, delayed, music, noise, speechy, temp_dir, write_wav, SR};
    use anvil_dsp::limiter::true_peak_dbtp;

    /// A realistic little session: the host talks throughout; the guest listens for the first
    /// half (so their mic carries nothing but the host's spill) and answers in the second
    /// (so the gate has to get out of the way); the guest's recorder started 150 ms late; and
    /// a music bed runs under all of it.
    fn session(secs: f64) -> (Vec<Track>, Vec<AudioBuffer>) {
        let host = speechy(secs, 3);
        let mut guest = crate::testfix::Speech {
            start: secs / 2.0,
            seed: 21,
            f0: 190.0,
            ..Default::default()
        }
        .render(secs);
        add(&mut guest, &delayed(&host, 480, 0.1)); // spill: 10 ms across the room, −20 dB
        add(&mut guest, &noise(secs, 0.0006, 11));
        let guest = delayed(&guest, 7200, 1.0); // the guest's recorder started 150 ms late
        let bed = music(secs, 0.15);

        let tracks = vec![
            Track::speech("host.wav", "Host"),
            Track::speech("guest.wav", "Guest"),
            Track::music("bed.wav", "Bed"),
        ];
        let buffers = vec![buffer(host), buffer(guest), buffer(bed)];
        (tracks, buffers)
    }

    /// Options that keep the tests fast and deterministic: the AI denoiser is a big ONNX model
    /// and this is not the lane that tests it. Everything else in the chain runs.
    fn opts() -> MultitrackOptions {
        MultitrackOptions {
            denoise: false,
            ..Default::default()
        }
    }

    /// Test (e): the mixdown hits the loudness target and stays under the true-peak ceiling.
    #[test]
    fn mixdown_hits_the_loudness_target_and_ceiling() {
        let (tracks, buffers) = session(10.0);
        let o = opts();
        let res = mix_buffers(&tracks, &buffers, &o).expect("mix");

        assert!(
            (res.report.after.integrated_lufs - o.target_lufs as f64).abs() <= 0.5,
            "integrated {:.2} LUFS is off the {:.0} LUFS target",
            res.report.after.integrated_lufs,
            o.target_lufs
        );
        let tp = true_peak_dbtp(&res.audio);
        assert!(
            tp <= o.true_peak_ceiling_dbtp as f64 + 0.01,
            "true peak {tp:.2} dBTP breached the {:.1} dBTP ceiling",
            o.true_peak_ceiling_dbtp
        );
        assert_eq!(res.audio.channel_count(), 2);
        assert_eq!(res.report.tracks.len(), 3);
    }

    /// The whole path, end to end: alignment finds the guest's late start, the crossgate ducks
    /// the host's spill off the guest's mic, and the bed ducks under both of them.
    #[test]
    fn mix_aligns_crossgates_and_ducks() {
        let (tracks, buffers) = session(10.0);
        let res = mix_buffers(&tracks, &buffers, &opts()).expect("mix");

        // The guest's recorder started 150 ms late, and the host's voice took another 10 ms to
        // cross the room into the guest's mic. Alignment locks onto the *shared content* — the
        // only thing both tracks contain — so the offset it reports is the sum: 160 ms. That is
        // the right answer: it is the lag that makes the host's voice line up on both tracks.
        let guest = &res.report.tracks[1];
        assert!(
            (guest.offset_secs - 0.160).abs() < 0.002,
            "guest offset {:.4} s, expected 0.160",
            guest.offset_secs
        );
        assert!(guest.alignment_confidence > 0.4);

        let cg = guest.crossgate.as_ref().expect("guest is crossgated");
        assert_eq!(cg.source.as_deref(), Some("Host"));
        assert!(
            cg.active_ratio > 0.1,
            "the crossgate never engaged on the guest's spill"
        );
        assert!(cg.max_reduction_db <= -10.0);
        assert!(
            cg.veto_ratio > 0.1,
            "the guest's own speech should have vetoed the gate somewhere"
        );

        let duck = res.report.tracks[2].duck.as_ref().expect("bed is ducked");
        assert!((duck.duck_db + 12.0).abs() < 0.01);
        assert!(duck.active_ratio > 0.3);

        // The host is the reference and nothing about it moves.
        assert_eq!(res.report.tracks[0].offset_secs, 0.0);
        assert!(res
            .report
            .health_card
            .iter()
            .any(|f| f.title == "Track aligned"));
    }

    /// Solo/mute are mixdown decisions, not analysis ones: a muted mic still bleeds into the
    /// others, so it must still drive the crossgate and the duck.
    #[test]
    fn solo_and_mute_control_what_reaches_the_bus() {
        let (mut tracks, buffers) = session(6.0);
        tracks[2].mute = true; // bed out
        let res = mix_buffers(&tracks, &buffers, &opts()).expect("mix");
        assert!(!res.report.tracks[2].audible);
        assert!(res.report.tracks[0].audible && res.report.tracks[1].audible);

        let (mut tracks, buffers) = session(6.0);
        tracks[0].solo = true;
        let res = mix_buffers(&tracks, &buffers, &opts()).expect("mix");
        assert!(res.report.tracks[0].audible);
        assert!(!res.report.tracks[1].audible, "solo mutes the rest");
        assert!(!res.report.tracks[2].audible);
        // …and the muted guest's mic is still what the host would have been crossgated against.
        assert!(res.report.tracks[1].crossgate.is_some());

        let (mut tracks, buffers) = session(3.0);
        for t in &mut tracks {
            t.mute = true;
        }
        assert!(matches!(
            mix_buffers(&tracks, &buffers, &opts()),
            Err(MultitrackError::NothingAudible)
        ));
    }

    /// Per-track gain offsets are faders: −6 dB on a track takes 6 dB out of its contribution.
    #[test]
    fn per_track_gain_offsets_apply() {
        let host = speechy(4.0, 3);
        let tracks = vec![Track::speech("a.wav", "A")];
        let buffers = vec![buffer(host.clone())];
        let base = MultitrackOptions {
            denoise: false,
            per_track_chain: false,
            align: false,
            crossgate: false,
            target_lufs: -16.0,
            ..Default::default()
        };
        let a = mix_buffers(&tracks, &buffers, &base).expect("mix");

        let mut quiet = tracks.clone();
        quiet[0].gain_db = -6.0;
        let b = mix_buffers(&quiet, &buffers, &base).expect("mix");
        // The master bus normalizes both back to target, so the *reported* pre-master loudness
        // is where the fader shows up.
        assert!(
            (a.report.before.integrated_lufs - b.report.before.integrated_lufs - 6.0).abs() < 0.2,
            "a −6 dB fader moved the sum by {:.2} dB",
            a.report.before.integrated_lufs - b.report.before.integrated_lufs
        );
    }

    /// ADR-003: same input, same options, bit-identical mix.
    #[test]
    fn the_mix_is_deterministic() {
        let (tracks, buffers) = session(4.0);
        let o = opts();
        let a = mix_buffers(&tracks, &buffers, &o).expect("mix");
        let b = mix_buffers(&tracks, &buffers, &o).expect("mix");
        assert_eq!(a.audio, b.audio, "two mixes of the same session must match");
        assert_eq!(a.report, b.report);
    }

    /// The file path: real WAVs on disk, decoded through `anvil-media`.
    #[test]
    fn mix_from_files_on_disk() {
        let dir = temp_dir("mix-files");
        let host = speechy(4.0, 3);
        let mut guest = delayed(&host, 2400, 0.12);
        add(&mut guest, &noise(4.0, 0.0006, 11));
        let tracks = vec![
            Track::speech(write_wav(&dir, "host.wav", &host), "Host"),
            Track::speech(write_wav(&dir, "guest.wav", &guest), "Guest"),
        ];
        let res = mix(&tracks, &opts()).expect("mix from files");
        assert_eq!(res.audio.sample_rate(), SR);
        assert!(
            (res.alignment.offsets_secs[1] - 0.05).abs() < 0.002,
            "offset {:.4} s, expected 0.050",
            res.alignment.offsets_secs[1]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 03 §8: degenerate inputs are errors, not panics.
    #[test]
    fn degenerate_inputs_are_graceful() {
        assert!(matches!(
            mix_buffers(&[], &[], &opts()),
            Err(MultitrackError::NoTracks)
        ));
        let tracks = vec![Track::speech("a.wav", "A")];
        assert!(matches!(
            mix_buffers(&tracks, &[buffer(Vec::new())], &opts()),
            Err(MultitrackError::EmptyTrack(_))
        ));
        assert!(matches!(
            mix_buffers(&tracks, &[], &opts()),
            Err(MultitrackError::BufferCountMismatch { .. })
        ));
    }

    /// The report is the contract (serde, snake_case).
    #[test]
    fn report_keys_match_the_contract() {
        let (tracks, buffers) = session(4.0);
        let res = mix_buffers(&tracks, &buffers, &opts()).expect("mix");
        let v = serde_json::to_value(&res.report).unwrap();
        for key in [
            "tracks",
            "before",
            "after",
            "target_lufs",
            "true_peak_ceiling_dbtp",
            "master_gain_db",
            "health_card",
            "chain_version",
        ] {
            assert!(v.get(key).is_some(), "MixReport missing {key}");
        }
        let t = &v["tracks"][1];
        for key in [
            "name",
            "kind",
            "offset_secs",
            "drift_ppm",
            "drift_repaired",
            "alignment_confidence",
            "gain_db",
            "audible",
            "crossgate",
            "duck",
        ] {
            assert!(t.get(key).is_some(), "TrackReport missing {key}");
        }
        assert_eq!(t["kind"], "speech");

        let al = serde_json::to_value(&res.alignment).unwrap();
        for key in ["offsets_secs", "drift_ppm", "confidence"] {
            assert!(al.get(key).is_some(), "Alignment missing {key}");
        }
    }
}
