//! Chain orchestrator + auto-decision (03 §2, §3 M1 subset).
//!
//! [`auto_configure`] maps an [`AnalysisReport`] + preset + tier onto a [`ChainConfig`]
//! (which modules engage, and how) and a plain-language Health Card. [`Chain::render`] then
//! runs the modules in order —
//!
//! ```text
//! (downmix) → DC/HPF → de-hum → de-clip → de-click → de-crackle → denoise → breath → de-ess
//! → AutoEQ → per-speaker leveling → leveler → two-pass loudness → true-peak limiter → dither
//! ```
//!
//! with a **stage cache**: each front stage's output is memoized by (input hash + that
//! stage's config hash), so re-mastering with only a downstream module tweaked reuses the
//! upstream work and logs `cache hit: stage X reused`.

use anvil_asr::Diarization;
use anvil_media::AudioBuffer;
use anvil_project::{Preset, Tier};
use ebur128::{EbuR128, Mode};
use serde::{Deserialize, Serialize};

use crate::analysis::AnalysisReport;
use crate::autoeq::{AutoEq, AutoEqConfig, EqTarget};
use crate::breath::{BreathConfig, BreathControl};
use crate::declick::{DeCrackle, DeCrackleConfig, MouthDeClick, MouthDeClickConfig};
use crate::declip::{DeClip, DeClipConfig};
use crate::deess::{DeEsser, DeEsserConfig};
use crate::dehum::{DeHum, DeHumConfig};
use crate::dither::{Dither, DitherConfig};
use crate::error::DspError;
use crate::hash::{hash_buffer, hash_config};
use crate::hpf::{DcHpf, DcHpfConfig, HpfMode};
use crate::leveler::{Leveler, LevelerConfig};
use crate::limiter::{measure_true_peak, LimiterConfig, TruePeakLimiter};
use crate::speaker::{
    MedianSource as SpeakerMedianSource, SpeakerGain, SpeakerLeveler, SpeakerLevelingConfig,
    VoiceMemory,
};
use anvil_ai::{DenoiseConfig, DenoiseTier, Denoiser};

use crate::Processor;

/// A before/after loudness snapshot (the compliance-report columns).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LoudnessSnapshot {
    /// Integrated loudness, LUFS.
    pub integrated_lufs: f64,
    /// Maximum true peak, dBTP.
    pub true_peak_dbtp: f64,
    /// Loudness range, LU.
    pub loudness_range_lu: f64,
}

/// One module's line in the master report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleReport {
    /// Module name (e.g. "denoise").
    pub name: String,
    /// Whether the module actually did anything.
    pub engaged: bool,
    /// The module's strength knob, if it has one (denoise); `null` otherwise.
    pub strength: Option<f32>,
    /// Plain-language detail.
    pub detail: String,
}

/// A Health Card finding (03 §2: every decision surfaced in plain language).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthFinding {
    /// "info" or "warn".
    pub severity: String,
    /// Short title.
    pub title: String,
    /// What we found / did.
    pub detail: String,
    /// Suggested user action, if any.
    pub fix: Option<String>,
}

impl HealthFinding {
    fn info(title: &str, detail: String) -> Self {
        Self {
            severity: "info".into(),
            title: title.into(),
            detail,
            fix: None,
        }
    }
    fn warn(title: &str, detail: String, fix: Option<&str>) -> Self {
        Self {
            severity: "warn".into(),
            title: title.into(),
            detail,
            fix: fix.map(str::to_string),
        }
    }
}

/// The full master report (contract E). `analysis` is the pass-1 report; `before`/`after`
/// bracket the loudness change; `modules` and `health_card` explain what happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MasterReport {
    /// The analysis pass this master was configured from.
    pub analysis: AnalysisReport,
    /// Loudness before the chain (= analysis).
    pub before: LoudnessSnapshot,
    /// Loudness of the rendered output.
    pub after: LoudnessSnapshot,
    /// Preset name.
    pub preset: String,
    /// Tier ("fast" | "standard" | "studio").
    pub tier: String,
    /// Chain version.
    pub chain_version: u32,
    /// Per-module engagement + detail, in chain order.
    pub modules: Vec<ModuleReport>,
    /// Plain-language findings.
    pub health_card: Vec<HealthFinding>,
}

/// Resolved configuration for the whole chain (produced by [`auto_configure`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ChainConfig {
    /// Collapse to mono before processing (dual-mono source).
    pub downmix_mono: bool,
    /// DC/HPF settings.
    pub dc_hpf: DcHpfConfig,
    /// De-hum settings, or `None` when the analysis found no confirmed hum (§4.2).
    pub dehum: Option<DeHumConfig>,
    /// De-clip settings, or `None` when the analysis found ≤ 3 clipped regions — or when the
    /// file is music-majority, where de-clip never engages (§2, §4.3).
    pub declip: Option<DeClipConfig>,
    /// Mouth de-click settings, or `None` in music mode (§4.3).
    pub declick: Option<MouthDeClickConfig>,
    /// De-crackle settings, or `None` in music mode (§4.3).
    pub decrackle: Option<DeCrackleConfig>,
    /// Denoise settings, or `None` when the source is clean enough to skip it.
    pub denoise: Option<DenoiseConfig>,
    /// Breath-control settings, or `None` in music mode (§4.5).
    pub breath: Option<BreathConfig>,
    /// De-esser settings, or `None` when there is no sibilance to tame (§4.6).
    pub deess: Option<DeEsserConfig>,
    /// AutoEQ settings, or `None` for non-speech / music material (§4.7).
    pub autoeq: Option<AutoEqConfig>,
    /// Per-speaker leveling + Voice Memory (§4.8 per-speaker mode), or `None` when the file was
    /// not diarized — in which case the chain runs exactly the single-speaker path it always did.
    pub speaker: Option<SpeakerLevelingConfig>,
    /// Adaptive leveler settings.
    pub leveler: LevelerConfig,
    /// True-peak limiter settings.
    pub limiter: LimiterConfig,
    /// Dither settings (off for the float M1 master output).
    pub dither: DitherConfig,
    /// Integrated loudness target (LUFS) for the two-pass normalize.
    pub target_lufs: f32,
    /// Preset name (for the report).
    pub preset_name: String,
    /// Tier (for the report).
    pub tier: Tier,
    /// The Health Card assembled while deciding.
    pub health_card: Vec<HealthFinding>,
}

/// De-clip trigger (03 §2: "Clipping regions > 3 → de-clip on").
const CLIP_REGION_TRIGGER: u32 = 3;
/// A single clipped stretch longer than this is "clipped-throughout" territory (03 §8) and
/// earns a Health Card warning: the excursion is gone, the repair can only bound it.
const LONG_CLIP_SECS: f64 = 0.05;

/// Map SNR onto denoise strength: 35 dB → 0.3, 10 dB → 1.0, clamped (03 §2).
fn snr_to_strength(snr_db: f32) -> f32 {
    let t = (35.0 - snr_db) / (35.0 - 10.0);
    (0.3 + t * (1.0 - 0.3)).clamp(0.3, 1.0)
}

/// The auto-decision (03 §2) for an undiarized file. Deterministic and unit-tested; every
/// choice becomes a Health Card finding.
///
/// This is the single-speaker path, unchanged: no diarization ⇒ no per-speaker stage ⇒ the
/// window-based AGC in [`crate::leveler`] does all the leveling, exactly as before.
pub fn auto_configure(report: &AnalysisReport, preset: &Preset, tier: Tier) -> ChainConfig {
    auto_configure_with_diarization(report, preset, tier, None, None)
}

/// The auto-decision (03 §2) with **speakers**: when a [`Diarization`] is supplied, per-speaker
/// leveling engages (§4.8) and any [`VoiceMemory`] profiles are applied (§4.7).
///
/// `diarization: None` is byte-for-byte [`auto_configure`].
pub fn auto_configure_with_diarization(
    report: &AnalysisReport,
    preset: &Preset,
    tier: Tier,
    diarization: Option<&Diarization>,
    memory: Option<&VoiceMemory>,
) -> ChainConfig {
    let mut health = Vec::new();

    // --- Stereo / dual-mono ---------------------------------------------------------------
    let downmix_mono = report.stereo.map(|s| s.dual_mono).unwrap_or(false);
    if downmix_mono {
        health.push(HealthFinding::info(
            "Dual-mono source",
            "Both channels are identical — processing as mono for a cleaner result.".into(),
        ));
    } else if let Some(s) = report.stereo {
        if s.correlation < -0.5 {
            health.push(HealthFinding::warn(
                "Out-of-phase stereo",
                format!(
                    "Channels are anti-correlated (r = {:.2}); this can cancel in mono.",
                    s.correlation
                ),
                Some("Consider exporting mono."),
            ));
        }
    }

    // --- DC / HPF -------------------------------------------------------------------------
    let music_present = report.music_ratio > 0.3;
    let music_majority = report.music_ratio > 0.6;
    let dc_significant = report.dc_offset.iter().any(|&d| d.abs() > 0.001);
    let cutoff_hz = if music_present { 40.0 } else { 80.0 };
    let dc_hpf = DcHpfConfig {
        mode: HpfMode::Auto,
        cutoff_hz,
        dc_block: true,
    };
    if dc_significant {
        health.push(HealthFinding::info(
            "DC offset removed",
            "A constant DC bias was found and filtered out.".into(),
        ));
    }
    health.push(HealthFinding::info(
        "High-pass filter",
        format!(
            "Rumble filtered below {cutoff_hz:.0} Hz ({}).",
            if music_present {
                "music present"
            } else {
                "speech"
            }
        ),
    ));

    // --- Denoise --------------------------------------------------------------------------
    let snr = report.snr_db as f32;
    let denoise = if snr >= 45.0 {
        health.push(HealthFinding::info(
            "Clean source",
            format!("SNR {snr:.0} dB — denoise skipped to preserve detail."),
        ));
        None
    } else {
        let base = if snr < 35.0 {
            snr_to_strength(snr)
        } else {
            0.2
        };
        let strength = if music_majority { base * 0.6 } else { base };
        health.push(HealthFinding::info(
            "Denoise engaged",
            format!(
                "SNR {snr:.0} dB → {} at strength {strength:.2}{}.",
                denoise_engine_name(tier),
                if music_majority {
                    " (lightened for music)"
                } else {
                    ""
                }
            ),
        ));
        Some(DenoiseConfig {
            strength,
            music_aware: music_majority,
        })
    };

    // --- De-hum (§4.2) --------------------------------------------------------------------
    // Only when the analysis confirmed a (preferably stable) tonal peak — never notch blind.
    let dehum = report.hum.map(|hum| DeHumConfig {
        fundamental_hz: hum.fundamental_hz,
        strength: 1.0,
        ..DeHumConfig::default()
    });
    if let Some(hum) = report.hum {
        health.push(HealthFinding::info(
            "De-hum engaged",
            format!(
                "Removed {:.0} Hz mains hum{} plus harmonics up to 1 kHz.",
                hum.fundamental_hz,
                if hum.stable { " (stable)" } else { "" }
            ),
        ));
    }

    let has_speech = report.speech_ratio > 0.0;

    // --- De-clip (§4.3; trigger from §2: "Clipping regions > 3 → de-clip on") --------------
    // The music guard is not a nicety: reconstructing a percussion transient's flat top would
    // invent an attack that was never played (§4.3 "never engage on percussion"). It is
    // enforced twice — here, and inside the processor from `music_ratio`.
    let clipped = report.clipping_regions > CLIP_REGION_TRIGGER;
    let declip = if clipped && !music_majority {
        let cfg = DeClipConfig {
            music_ratio: report.music_ratio,
            ..DeClipConfig::default()
        };
        health.push(HealthFinding::info(
            "De-clip engaged",
            format!(
                "{} clipped regions rebuilt (flat-tops reconstructed, peaks trimmed to {:.0} dBFS).",
                report.clipping_regions, cfg.target_peak_dbfs
            ),
        ));
        Some(cfg)
    } else {
        if clipped {
            health.push(HealthFinding::warn(
                "Clipping detected",
                format!(
                    "{} clipped regions, but this is a music-majority file — de-clip stays off \
                     so percussion transients are never rebuilt.",
                    report.clipping_regions
                ),
                Some("Re-record or re-export with more headroom."),
            ));
        }
        None
    };
    // Clipped-throughout (§8): a long continuous flat top has no recoverable excursion left.
    if let Some(worst) = report.worst_clip_secs {
        if worst > LONG_CLIP_SECS {
            health.push(HealthFinding::warn(
                "Heavily clipped source",
                format!(
                    "The worst clipped stretch runs {:.0} ms — that much waveform cannot be \
                     recovered, only bounded.",
                    worst * 1000.0
                ),
                Some("Re-record with more headroom; the repair is a best effort."),
            ));
        }
    }

    // --- Mouth de-click (§4.3 gentle) -----------------------------------------------------
    // Speech material only; disabled in music mode so we never chase percussion transients.
    let declick = if music_majority {
        None
    } else {
        Some(MouthDeClickConfig {
            gate_dbfs: report.noise_floor_dbfs as f32 + 6.0,
            ..MouthDeClickConfig::default()
        })
    };

    // --- De-crackle (§4.3 broad) ----------------------------------------------------------
    // Same music guard as the de-click. The threshold is set from the analysis: a pristine
    // source gets a higher bar (nothing there to find — stay out of the way), a noisy one a
    // lower one, since crackle rides in with the same gear that produced the noise.
    let decrackle = if music_majority {
        None
    } else {
        Some(DeCrackleConfig {
            sensitivity: if snr >= 35.0 { 7.0 } else { 6.0 },
            gate_dbfs: report.noise_floor_dbfs as f32 + 6.0,
            ..DeCrackleConfig::default()
        })
    };

    // --- Breath control (§4.5) ------------------------------------------------------------
    // Gentle inhale attenuation; off in music mode.
    let breath = if music_majority || !has_speech {
        None
    } else {
        health.push(HealthFinding::info(
            "Breath control",
            "Inhale breaths softened by ~6 dB (gently ramped, never gated).".into(),
        ));
        Some(BreathConfig {
            noise_floor_dbfs: report.noise_floor_dbfs as f32,
            ..BreathConfig::default()
        })
    };

    // --- De-esser (§4.6) ------------------------------------------------------------------
    // Threshold auto-set from the sibilance statistics so only genuinely harsh frames duck.
    let deess = if has_speech && !music_majority {
        let threshold_ratio = (report.sibilance_ratio * 2.5).clamp(0.12, 0.6);
        health.push(HealthFinding::info(
            "De-esser engaged",
            format!("5–9 kHz sibilance tamed (3:1 above a {threshold_ratio:.2} band ratio)."),
        ));
        Some(DeEsserConfig {
            threshold_ratio,
            ..DeEsserConfig::default()
        })
    } else {
        None
    };

    // --- AutoEQ (§4.7) --------------------------------------------------------------------
    // Speech-shape matching, bounded ±6 dB, never boosting above the source roll-off.
    let autoeq = if has_speech && !music_majority {
        health.push(HealthFinding::info(
            "AutoEQ",
            "Speech spectrum nudged toward a neutral target (±6 dB bells).".into(),
        ));
        Some(AutoEqConfig {
            target: EqTarget::Neutral,
            amount: 0.5,
            bandwidth_hz: report.bandwidth_hz,
            q: 1.0,
        })
    } else {
        None
    };

    // --- Per-speaker leveling (§4.8 per-speaker mode) --------------------------------------
    // The quiet-guest fix. With diarization we know *who* is talking, so each speaker's median
    // speech loudness is normalized to the common target with a static, crossfaded gain before
    // the window AGC ever sees the audio. A window AGC cannot do this — it can only chase the
    // envelope, which is what pumping is.
    let target = preset.target_lufs;
    let noise_gate_lufs = report.noise_floor_dbfs as f32 + 8.0;
    let speaker = diarization.and_then(|diar| {
        // Music-majority material is left to the music-mode AGC: normalizing "speakers" across a
        // music bed would flatten intentional dynamics.
        if music_majority || diar.speakers.is_empty() || diar.segments.is_empty() {
            return None;
        }
        Some(SpeakerLevelingConfig {
            memory: memory.cloned().unwrap_or_default(),
            noise_gate_lufs,
            ..SpeakerLevelingConfig::new(diar.clone(), target)
        })
    });
    if let (Some(cfg), Some(diar)) = (&speaker, diarization) {
        let known = diar
            .speakers
            .iter()
            .filter(|s| cfg.memory.get(&s.label).is_some())
            .count();
        health.push(HealthFinding::info(
            "Per-speaker leveling",
            format!(
                "{} speakers detected — each one's median speech loudness normalized to \
                 {target:.0} LUFS before the leveler runs (no chasing, no pumping){}.",
                diar.speakers.len(),
                if known > 0 {
                    format!(", and {known} recognized from Voice Memory")
                } else {
                    String::new()
                }
            ),
        ));
    }

    // --- Leveler --------------------------------------------------------------------------
    // Warm start (§4.8: "converges within 2 s"): open the AGC at the gain the file needs. When
    // per-speaker leveling runs first, every speaker already sits on target, so the right
    // opening gain is unity — warm-starting from the *pre*-speaker-gain integrated loudness
    // would make the AGC spend its first seconds undoing work that has already been done.
    let warm_start_db = if speaker.is_some() {
        0.0
    } else {
        (target as f64 - report.integrated_lufs).clamp(-12.0, 12.0) as f32
    };
    let leveler = LevelerConfig {
        target_st_lufs: target,
        max_gain_db: 12.0,
        dynamics_preservation: 0.35,
        music_mode: music_majority,
        // Gate a little above the measured noise floor (dBFS≈LUFS at these levels for gating).
        noise_gate_lufs,
        warm_start_db,
    };
    if music_majority {
        health.push(HealthFinding::info(
            "Music-aware leveling",
            "Music-majority file — leveler eased to keep musical dynamics.".into(),
        ));
    }

    // --- Limiter / loudness ---------------------------------------------------------------
    let limiter = LimiterConfig {
        ceiling_dbtp: preset.true_peak_ceiling_dbtp,
        ..Default::default()
    };
    health.push(HealthFinding::info(
        "Loudness normalized",
        format!(
            "Two-pass normalize to {target:.0} LUFS, true-peak limited to {:.1} dBTP.",
            preset.true_peak_ceiling_dbtp
        ),
    ));

    // --- Advisory findings (surfaced, not acted on) ----------------------------------------
    if report.bandwidth_hz > 0.0 && report.bandwidth_hz < 12_000.0 {
        health.push(HealthFinding::info(
            "Limited bandwidth",
            format!(
                "Source rolls off near {:.0} Hz (phone/Zoom-grade); no fake brightness added.",
                report.bandwidth_hz
            ),
        ));
    }
    if matches!(report.reverb_bucket.as_str(), "noticeable" | "bad") {
        health.push(HealthFinding::warn(
            "Room reverb",
            format!(
                "Reverb rated \"{}\"{}.",
                report.reverb_bucket,
                report
                    .rt60_secs
                    .map(|r| format!(" (~{r:.2} s RT60)"))
                    .unwrap_or_default()
            ),
            Some("Studio tier adds strong dereverb."),
        ));
    }

    ChainConfig {
        downmix_mono,
        dc_hpf,
        dehum,
        declip,
        declick,
        decrackle,
        denoise,
        breath,
        deess,
        autoeq,
        speaker,
        leveler,
        limiter,
        dither: DitherConfig::default(),
        target_lufs: target,
        preset_name: preset.name.clone(),
        tier,
        health_card: health,
    }
}

/// One memoized stage output.
struct StageCacheEntry {
    in_hash: u64,
    cfg_hash: u64,
    out: AudioBuffer,
}

/// The number of cacheable front stages, in chain order (03 §3): downmix, DC/HPF, de-hum,
/// de-clip, mouth de-click, de-crackle, denoise, breath, de-esser, AutoEQ, per-speaker leveling,
/// leveler.
const CACHED_STAGES: usize = 12;

/// The chain runner. Holds the stage cache across `render` calls so re-masters reuse work.
pub struct Chain {
    sample_rate: u32,
    cache: Vec<Option<StageCacheEntry>>,
    /// What the per-speaker stage last decided. Kept beside the cache because a cache *hit* on
    /// that stage means the same audio and the same speakers produced the same gains — the
    /// report must still be able to say so without re-running the stage.
    speaker_gains: Vec<SpeakerGain>,
    /// Ditto for the profiles derived on the last run of that stage.
    voice_memory: VoiceMemory,
}

/// The result of a render: the audio plus what happened (for the report and tests).
pub struct RenderOutcome {
    /// The rendered audio.
    pub audio: AudioBuffer,
    /// Loudness before (measured on the chain input).
    pub before: LoudnessSnapshot,
    /// Loudness after (measured on the output).
    pub after: LoudnessSnapshot,
    /// Names of stages served from cache this render.
    pub cache_hits: Vec<&'static str>,
    /// What per-speaker leveling did, per speaker (§4.8). Empty when the file was not diarized.
    pub speaker_gains: Vec<SpeakerGain>,
    /// Fresh Voice Memory profiles derived from this render (§4.7) — what the storage lane in
    /// `anvil-project` persists so the next episode recognizes this cast. Populated only when
    /// [`SpeakerLevelingConfig::profile_autoeq`] was set.
    pub voice_memory: VoiceMemory,
}

impl Chain {
    /// A fresh chain for `sample_rate`, with an empty cache.
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            cache: (0..CACHED_STAGES).map(|_| None).collect(),
            speaker_gains: Vec::new(),
            voice_memory: VoiceMemory::default(),
        }
    }

    /// Run a cacheable front stage: reuse memoized output on an (input, config) hit.
    fn cached_stage<F>(
        &mut self,
        idx: usize,
        name: &'static str,
        input: AudioBuffer,
        cfg_hash: u64,
        hits: &mut Vec<&'static str>,
        compute: F,
    ) -> AudioBuffer
    where
        F: FnOnce(AudioBuffer) -> AudioBuffer,
    {
        let in_hash = hash_buffer(&input);
        if let Some(entry) = &self.cache[idx] {
            if entry.in_hash == in_hash && entry.cfg_hash == cfg_hash {
                tracing::info!("cache hit: stage {name} reused");
                hits.push(name);
                return entry.out.clone();
            }
        }
        let out = compute(input);
        self.cache[idx] = Some(StageCacheEntry {
            in_hash,
            cfg_hash,
            out: out.clone(),
        });
        out
    }

    /// Render `input` through the chain per `config`.
    pub fn render(&mut self, input: &AudioBuffer, config: &ChainConfig) -> RenderOutcome {
        let before = measure_loudness(input);
        let sr = self.sample_rate;
        let mut hits = Vec::new();

        // Stage 0: downmix (config keyed on the bool).
        let buf = self.cached_stage(
            0,
            "downmix",
            input.clone(),
            hash_config(&config.downmix_mono),
            &mut hits,
            |b| {
                if config.downmix_mono {
                    downmix_to_mono(&b)
                } else {
                    b
                }
            },
        );

        // Stage 1: DC/HPF.
        let dc_cfg = config.dc_hpf;
        let buf = self.cached_stage(
            1,
            "dc_hpf",
            buf,
            hash_config(&dc_cfg),
            &mut hits,
            |mut b| {
                let mut m = DcHpf::new(b.channel_count(), sr, dc_cfg);
                m.process(&mut b);
                b
            },
        );

        // Stage 2: de-hum (only when the analysis confirmed hum; cache key folds in None).
        let dehum_cfg = config.dehum;
        let buf = self.cached_stage(
            2,
            "dehum",
            buf,
            hash_config(&dehum_cfg),
            &mut hits,
            |mut b| {
                if let Some(cfg) = dehum_cfg {
                    let mut m = DeHum::new(b.channel_count(), sr, cfg);
                    m.process(&mut b);
                }
                b
            },
        );

        // Stage 3: de-clip — first of the repair group (03 §3). The flat tops must go before
        // anything else looks at the waveform: to a transient detector a clipping corner *is*
        // a click, and to a denoiser the harmonics it radiates are just program material.
        let declip_cfg = config.declip;
        let buf = self.cached_stage(
            3,
            "declip",
            buf,
            hash_config(&declip_cfg),
            &mut hits,
            |mut b| {
                if let Some(cfg) = declip_cfg {
                    let mut m = DeClip::new(sr, cfg);
                    m.process(&mut b);
                }
                b
            },
        );

        // Stage 4: mouth de-click (before denoise, so denoise doesn't smear the glitch).
        let declick_cfg = config.declick;
        let buf = self.cached_stage(
            4,
            "declick",
            buf,
            hash_config(&declick_cfg),
            &mut hits,
            |mut b| {
                if let Some(cfg) = declick_cfg {
                    let mut m = MouthDeClick::new(sr, cfg);
                    m.process(&mut b);
                }
                b
            },
        );

        // Stage 5: de-crackle — the broad impulse pass, after the gentle mouth de-click has
        // taken the obvious ones out of its local statistics.
        let decrackle_cfg = config.decrackle;
        let buf = self.cached_stage(
            5,
            "decrackle",
            buf,
            hash_config(&decrackle_cfg),
            &mut hits,
            |mut b| {
                if let Some(cfg) = decrackle_cfg {
                    let mut m = DeCrackle::new(sr, cfg);
                    m.process(&mut b);
                }
                b
            },
        );

        // Stage 6: denoise (only if engaged). The cache key folds in the None case AND the
        // tier: Fast (RNNoise) / Standard (DeepFilterNet3) / Studio pick different engines and
        // therefore produce different audio, so they must never share a cached stage (03 §4.4).
        let denoise_cfg = config.denoise;
        let denoise_tier = match config.tier {
            Tier::Fast => DenoiseTier::Fast,
            Tier::Standard => DenoiseTier::Standard,
            Tier::Studio => DenoiseTier::Studio,
        };
        let buf = self.cached_stage(
            6,
            "denoise",
            buf,
            hash_config(&(denoise_cfg, tier_str(config.tier))),
            &mut hits,
            |mut b| {
                if let Some(cfg) = denoise_cfg {
                    // If the requested tier can't initialize (ORT/model/EP failure), fall back
                    // to `Denoiser::new` — which is DFN3 with an RNNoise safety net — rather
                    // than failing the render. `Denoiser::tier()` reports what actually ran.
                    let mut d = Denoiser::try_with_tier(b.channel_count(), cfg, denoise_tier)
                        .unwrap_or_else(|_| Denoiser::new(b.channel_count(), cfg));
                    d.process(&mut b);
                }
                b
            },
        );

        // Stage 7: breath control (after denoise so it acts on the cleaned inhale).
        let breath_cfg = config.breath;
        let buf = self.cached_stage(
            7,
            "breath",
            buf,
            hash_config(&breath_cfg),
            &mut hits,
            |mut b| {
                if let Some(cfg) = breath_cfg {
                    let mut m = BreathControl::new(sr, cfg);
                    m.process(&mut b);
                }
                b
            },
        );

        // Stage 8: de-esser.
        let deess_cfg = config.deess;
        let buf = self.cached_stage(
            8,
            "deess",
            buf,
            hash_config(&deess_cfg),
            &mut hits,
            |mut b| {
                if let Some(cfg) = deess_cfg {
                    let mut m = DeEsser::new(sr, cfg);
                    m.process(&mut b);
                }
                b
            },
        );

        // Stage 9: AutoEQ.
        let autoeq_cfg = config.autoeq;
        let buf = self.cached_stage(
            9,
            "autoeq",
            buf,
            hash_config(&autoeq_cfg),
            &mut hits,
            |mut b| {
                if let Some(cfg) = autoeq_cfg {
                    let mut m = AutoEq::new(sr, cfg);
                    m.process(&mut b);
                }
                b
            },
        );

        // Stage 10: per-speaker leveling (§4.8) — *before* the window AGC, which is the whole
        // point: normalize each speaker to the common target statically, and the AGC downstream
        // has nothing left to chase. Passthrough when the file was not diarized.
        let spk_cfg = config.speaker.as_ref();
        let mut computed: Option<(Vec<SpeakerGain>, VoiceMemory)> = None;
        let buf = self.cached_stage(
            10,
            "speaker",
            buf,
            hash_config(&spk_cfg),
            &mut hits,
            |mut b| {
                if let Some(cfg) = spk_cfg {
                    let mut sp = SpeakerLeveler::new(sr, cfg.clone());
                    sp.process(&mut b);
                    computed = Some((sp.applied().to_vec(), sp.derived().clone()));
                }
                b
            },
        );
        match computed {
            Some((gains, memory)) => {
                self.speaker_gains = gains;
                self.voice_memory = memory;
            }
            // A cache hit keeps the previous run's values (same input + same config ⇒ same
            // decisions); a chain reconfigured *without* speakers must forget them.
            None if spk_cfg.is_none() => {
                self.speaker_gains.clear();
                self.voice_memory = VoiceMemory::default();
            }
            None => {}
        }

        // Stage 11: leveler.
        let lev_cfg = config.leveler;
        let pre_loudness = self.cached_stage(
            11,
            "leveler",
            buf,
            hash_config(&lev_cfg),
            &mut hits,
            |mut b| {
                let mut lev = Leveler::new(sr, lev_cfg);
                lev.process(&mut b);
                b
            },
        );

        // Loudness normalize: drive make-up gain into the true-peak limiter and converge the
        // *limited* integrated loudness onto target (§4.9/§4.10). The limiter owns the ceiling;
        // see `converge_drive_gain` for why the old single flat-trim correction could not.
        let target = config.target_lufs as f64;
        let l0 = measure_integrated(&pre_loudness);
        let limiter_cfg = config.limiter;
        let apply = |gain_db: f64| -> AudioBuffer {
            let mut b = pre_loudness.clone();
            apply_gain_db(&mut b, gain_db as f32);
            let mut lim = TruePeakLimiter::with_rate(limiter_cfg, sr);
            lim.process(&mut b);
            b
        };
        // The probe (limit-then-measure) is infallible here, so the search never errors.
        let solution = converge_drive_gain(target, l0, |gain_db| {
            Ok(GainProbe {
                l_final: measure_integrated(&apply(gain_db)),
                // Whole-buffer re-applies via `apply`, whose limiter recomputes its own trim, so
                // the carried trim is unused on this path (kept 1.0). The *l_final* it measures is
                // identical to the streaming probe's, which is what keeps the two paths in parity.
                trim: 1.0,
            })
        })
        .expect("whole-buffer loudness probe is infallible");
        if !solution.converged {
            tracing::warn!(
                target_lufs = target,
                reached_lufs = solution.l_final,
                "loudness could not converge onto target within the drive cap; \
                 shipping closest ({:.2} LU off)",
                target - solution.l_final
            );
        }
        let mut out = apply(solution.gain_db);

        // Dither (only for a real bit-depth reduction; float output = no-op) (§4.11).
        if config.dither.engaged() {
            Dither::new(config.dither).process(&mut out);
        }

        let after = measure_loudness(&out);
        RenderOutcome {
            audio: out,
            before,
            after,
            cache_hits: hits,
            speaker_gains: self.speaker_gains.clone(),
            voice_memory: self.voice_memory.clone(),
        }
    }
}

/// Run the **front stages** (downmix → DC/HPF → de-hum → de-clip → de-click → de-crackle →
/// denoise → breath → de-ess → AutoEQ → per-speaker leveling → adaptive leveler) on `input`,
/// returning the post-leveler audio plus what the per-speaker stage decided.
///
/// This is [`Chain::render`]'s front half with **no stage cache** — one linear pass. The
/// streaming master (`crate::stream`) calls it on bounded overlapping segments and crossfades
/// the results, so the whole file is never resident; the two-pass loudness normalize and
/// true-peak limiter (which need whole-file scalars) run afterwards over the streamed result.
///
/// A single whole-file call is byte-identical to `Chain::render`'s front portion (same modules,
/// same order, same fresh-per-call state) — the cache only ever *returns* the same bytes it
/// would compute, so dropping it changes nothing but memory.
pub fn run_front_stages(
    input: &AudioBuffer,
    config: &ChainConfig,
    sample_rate: u32,
) -> (AudioBuffer, Vec<SpeakerGain>, VoiceMemory) {
    let sr = sample_rate;

    // Stage 0: downmix.
    let mut buf = if config.downmix_mono {
        downmix_to_mono(input)
    } else {
        input.clone()
    };

    // Stage 1: DC/HPF.
    {
        let mut m = DcHpf::new(buf.channel_count(), sr, config.dc_hpf);
        m.process(&mut buf);
    }
    // Stage 2: de-hum.
    if let Some(cfg) = config.dehum {
        let mut m = DeHum::new(buf.channel_count(), sr, cfg);
        m.process(&mut buf);
    }
    // Stage 3: de-clip.
    if let Some(cfg) = config.declip {
        let mut m = DeClip::new(sr, cfg);
        m.process(&mut buf);
    }
    // Stage 4: mouth de-click.
    if let Some(cfg) = config.declick {
        let mut m = MouthDeClick::new(sr, cfg);
        m.process(&mut buf);
    }
    // Stage 5: de-crackle.
    if let Some(cfg) = config.decrackle {
        let mut m = DeCrackle::new(sr, cfg);
        m.process(&mut buf);
    }
    // Stage 6: denoise (tier picks the engine, with the same RNNoise safety net as render).
    if let Some(cfg) = config.denoise {
        let denoise_tier = match config.tier {
            Tier::Fast => DenoiseTier::Fast,
            Tier::Standard => DenoiseTier::Standard,
            Tier::Studio => DenoiseTier::Studio,
        };
        let mut d = Denoiser::try_with_tier(buf.channel_count(), cfg, denoise_tier)
            .unwrap_or_else(|_| Denoiser::new(buf.channel_count(), cfg));
        d.process(&mut buf);
    }
    // Stage 7: breath control.
    if let Some(cfg) = config.breath {
        let mut m = BreathControl::new(sr, cfg);
        m.process(&mut buf);
    }
    // Stage 8: de-esser.
    if let Some(cfg) = config.deess {
        let mut m = DeEsser::new(sr, cfg);
        m.process(&mut buf);
    }
    // Stage 9: AutoEQ.
    if let Some(cfg) = config.autoeq {
        let mut m = AutoEq::new(sr, cfg);
        m.process(&mut buf);
    }
    // Stage 10: per-speaker leveling. Buffer-relative, so this is only valid on a whole-file
    // buffer — the streaming master runs the single-speaker path (speaker = None) and keeps the
    // diarized path on `Chain::render`.
    let (mut gains, mut memory) = (Vec::new(), VoiceMemory::default());
    if let Some(cfg) = config.speaker.as_ref() {
        let mut sp = SpeakerLeveler::new(sr, cfg.clone());
        sp.process(&mut buf);
        gains = sp.applied().to_vec();
        memory = sp.derived().clone();
    }
    // Stage 11: adaptive leveler.
    {
        let mut lev = Leveler::new(sr, config.leveler);
        lev.process(&mut buf);
    }

    (buf, gains, memory)
}

/// Assemble the [`MasterReport`] from the pieces.
pub fn build_master_report(
    analysis: AnalysisReport,
    config: &ChainConfig,
    outcome: &RenderOutcome,
) -> MasterReport {
    let chain_version = analysis.chain_version;
    let target = config.target_lufs;

    let modules = vec![
        ModuleReport {
            name: "dc_hpf".into(),
            engaged: config.dc_hpf.mode != HpfMode::Off || config.dc_hpf.dc_block,
            strength: None,
            detail: format!("DC blocker + {:.0} Hz high-pass", config.dc_hpf.cutoff_hz),
        },
        ModuleReport {
            name: "dehum".into(),
            engaged: config.dehum.is_some(),
            strength: config.dehum.map(|d| d.strength),
            detail: match config.dehum {
                Some(d) => format!("notch cascade at {:.0} Hz + harmonics", d.fundamental_hz),
                None => "skipped (no confirmed hum)".into(),
            },
        },
        ModuleReport {
            name: "declip".into(),
            engaged: config.declip.is_some(),
            strength: None,
            detail: match config.declip {
                Some(c) => format!(
                    "{} clipped regions rebuilt (cubic flat-top reconstruction, peaks trimmed \
                     to {:.0} dBFS)",
                    analysis.clipping_regions, c.target_peak_dbfs
                ),
                None if analysis.clipping_regions > CLIP_REGION_TRIGGER => {
                    "off (music-mode percussion guard)".into()
                }
                None => "skipped (no clipping)".into(),
            },
        },
        ModuleReport {
            name: "declick".into(),
            engaged: config.declick.is_some(),
            strength: None,
            detail: match config.declick {
                Some(_) => "mouth-click outliers bridged (≤ 2 ms)".into(),
                None => "off (music mode)".into(),
            },
        },
        ModuleReport {
            name: "decrackle".into(),
            engaged: config.decrackle.is_some(),
            strength: None,
            detail: match config.decrackle {
                Some(c) => format!(
                    "impulse outliers ({:.0}σ vs local stats) bridged with AR interpolation \
                     (≤ {:.0} ms)",
                    c.sensitivity, c.max_gap_ms
                ),
                None => "off (music mode)".into(),
            },
        },
        ModuleReport {
            name: "denoise".into(),
            engaged: config.denoise.is_some(),
            strength: config.denoise.map(|d| d.strength),
            detail: match config.denoise {
                Some(d) => format!(
                    "{}, ~{:.0} dB max attenuation",
                    denoise_engine_name(config.tier),
                    d.max_attenuation_db()
                ),
                None => "skipped (clean source)".into(),
            },
        },
        ModuleReport {
            name: "breath".into(),
            engaged: config.breath.is_some(),
            strength: config.breath.map(|b| b.reduction_db),
            detail: match config.breath {
                Some(b) => format!("breaths softened −{:.0} dB (ramped)", b.reduction_db),
                None => "off (music mode)".into(),
            },
        },
        ModuleReport {
            name: "deess".into(),
            engaged: config.deess.is_some(),
            strength: config.deess.map(|d| d.threshold_ratio),
            detail: match config.deess {
                Some(d) => format!(
                    "5–9 kHz {:.0}:1 above {:.2} band ratio",
                    d.ratio, d.threshold_ratio
                ),
                None => "skipped (no sibilance)".into(),
            },
        },
        ModuleReport {
            name: "autoeq".into(),
            engaged: config.autoeq.is_some(),
            strength: config.autoeq.map(|a| a.amount),
            detail: match config.autoeq {
                Some(a) => format!(
                    "{:?} target, amount {:.2} (±6 dB bells)",
                    a.target, a.amount
                ),
                None => "skipped (non-speech)".into(),
            },
        },
        ModuleReport {
            name: "speaker".into(),
            engaged: config.speaker.is_some(),
            strength: None,
            detail: match &config.speaker {
                Some(_) if !outcome.speaker_gains.is_empty() => format!(
                    "per-speaker leveling → {target:.0} LUFS ({})",
                    outcome
                        .speaker_gains
                        .iter()
                        .map(|g| format!(
                            "{} {:.1} LUFS{} {:+.1} dB",
                            g.label,
                            g.median_lufs,
                            match g.source {
                                SpeakerMedianSource::Remembered => " (remembered)",
                                SpeakerMedianSource::Unknown => " (unmeasurable)",
                                SpeakerMedianSource::Measured => "",
                            },
                            g.gain_db
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Some(_) => "per-speaker leveling engaged, but no speaker could be measured".into(),
                None => "skipped (no diarization — single-speaker leveling)".into(),
            },
        },
        ModuleReport {
            name: "leveler".into(),
            engaged: true,
            strength: None,
            detail: format!(
                "adaptive leveler → {target:.0} LUFS speech target{}",
                if config.leveler.music_mode {
                    " (music mode)"
                } else {
                    ""
                }
            ),
        },
        ModuleReport {
            name: "loudness".into(),
            engaged: true,
            strength: None,
            detail: format!(
                "two-pass normalize {:.1} → {:.1} LUFS (target {target:.0})",
                outcome.before.integrated_lufs, outcome.after.integrated_lufs
            ),
        },
        ModuleReport {
            name: "limiter".into(),
            engaged: true,
            strength: None,
            detail: format!("true-peak ceiling {:.1} dBTP", config.limiter.ceiling_dbtp),
        },
        ModuleReport {
            name: "dither".into(),
            engaged: config.dither.engaged(),
            strength: None,
            detail: if config.dither.engaged() {
                "TPDF dither at 16-bit".into()
            } else {
                "off (float output)".into()
            },
        },
    ];

    // Honest loudness residual: if the converged output still sits outside the ±0.5 LU contract
    // (pathologically peaky content the limiter can't drive to target within its depth cap),
    // surface it rather than shipping silently out of spec.
    let mut health_card = config.health_card.clone();
    let after_lufs = outcome.after.integrated_lufs;
    if after_lufs.is_finite() && (target as f64 - after_lufs).abs() > LOUDNESS_TOL_CONTRACT {
        health_card.push(HealthFinding::warn(
            "Loudness off target",
            format!(
                "Output integrated loudness is {after_lufs:.1} LUFS — {:.1} LU from the {target:.0} \
                 LUFS target. The source is extremely peaky; the true-peak limiter is at its depth \
                 limit, so driving louder would only add distortion. True peak stays at the ceiling.",
                (after_lufs - target as f64).abs()
            ),
            Some("Reduce the source crest factor (tame transients) or accept a lower target."),
        ));
    }

    MasterReport {
        analysis,
        before: outcome.before,
        after: outcome.after,
        preset: config.preset_name.clone(),
        tier: tier_str(config.tier).into(),
        chain_version,
        modules,
        health_card,
    }
}

fn tier_str(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => "fast",
        Tier::Standard => "standard",
        Tier::Studio => "studio",
    }
}

/// The denoise engine a tier actually runs (03 §4.4): Fast = RNNoise, Standard = DeepFilterNet3,
/// Studio = DFN3 at full suppression. The Health Card and the module chip must name what really
/// ran — reporting "RNNoise" while DFN3 does the work would be a lie in the UI.
fn denoise_engine_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => "RNNoise",
        Tier::Standard => "DeepFilterNet3",
        Tier::Studio => "DeepFilterNet3 (Studio)",
    }
}

/// Average all channels into a single mono channel.
fn downmix_to_mono(buf: &AudioBuffer) -> AudioBuffer {
    let frames = buf.frames();
    let ch = buf.channel_count().max(1);
    let mut mono = vec![0.0f32; frames];
    for c in 0..buf.channel_count() {
        for (i, &s) in buf.channel(c).iter().enumerate() {
            mono[i] += s;
        }
    }
    let inv = 1.0 / ch as f32;
    for s in &mut mono {
        *s *= inv;
    }
    AudioBuffer::from_planar(vec![mono], buf.sample_rate())
}

/// Multiply the whole buffer by a dB gain.
fn apply_gain_db(buf: &mut AudioBuffer, db: f32) {
    let g = 10f32.powf(db / 20.0);
    if (g - 1.0).abs() < 1e-9 {
        return;
    }
    for channel in buf.planar_mut() {
        for s in channel.iter_mut() {
            *s *= g;
        }
    }
}

/// One probe of the limited chain at a candidate drive gain: the true-peak-limited integrated
/// loudness reached, plus the limiter's zero-tolerance static trim at that gain.
///
/// `l_final` is measured **after** the limiter's flat trim, i.e. it is exactly what the meter
/// reads on the rendered output — so a whole-buffer probe (limit-then-measure) and a streaming
/// probe (measure the look-ahead-limited loudness, add `20·log10(trim)`) return the same number
/// for the same content and gain. That identity is what keeps the two master paths in loudness
/// parity (see `crate::stream`).
pub(crate) struct GainProbe {
    /// Integrated loudness of the limited (post-trim) output, LUFS.
    pub l_final: f64,
    /// The limiter's static trim (`ceiling / measured_true_peak`, ≤ 1.0) at this gain. The
    /// streaming render pass replays it; the whole-buffer limiter recomputes the identical value.
    pub trim: f32,
}

/// The drive gain the loudness normalize settled on.
pub(crate) struct GainSolution {
    /// Make-up/drive gain (dB) to apply *before* the limiter.
    pub gain_db: f64,
    /// The limiter static trim at `gain_db` (streaming render needs it explicitly).
    pub trim: f32,
    /// The limited integrated loudness reached (LUFS); `-inf`/`NaN` guards to silence.
    pub l_final: f64,
    /// Whether `l_final` landed inside the ±0.5 LU M1 contract (false ⇒ a residual is surfaced).
    pub converged: bool,
}

/// Aim inside the ±0.5 contract so meter float-order never tips a good render out of spec.
const LOUDNESS_TOL_TIGHT: f64 = 0.3;
/// The M1 exit contract: |integrated − target| ≤ 0.5 LU.
const LOUDNESS_TOL_CONTRACT: f64 = 0.5;
/// Bounded pass count. Probes are cheap (a spill re-read / buffer re-limit — DFN3 never re-runs),
/// so this is a determinism/robustness bound, not a perf one: enough for a first deficit step,
/// a secant/false-position lock-on, and a couple of refinements, even across an over-limiting
/// loudness valley. The iteration count is a **pure function of content** (ADR-003).
const LOUDNESS_MAX_PASSES: usize = 8;
/// Clamp any single gain step so a near-flat local slope (limiter saturating) can't fling the
/// drive wildly; the march-up then proceeds in bounded increments.
const LOUDNESS_MAX_STEP_DB: f64 = 12.0;
/// Limiting-depth cap: how far past the nominal normalize gain (`target − l0`) we will drive
/// into the limiter chasing target. Beyond this the content is pathologically peaky — driving
/// harder is either futile (the limiter is saturating and loudness no longer rises) or destroys
/// the signal — so we stop, keep the closest gain, and surface the residual honestly.
const LOUDNESS_MAX_EXTRA_DRIVE_DB: f64 = 30.0;

/// Drive make-up gain into the true-peak limiter and **converge** the limited integrated loudness
/// onto `target` (03 §4.9/§4.10). `l0` is the pre-normalize integrated loudness of the material
/// the limiter will see; `probe(gain_db)` runs the gain→limiter→meter path and returns the
/// resulting [`GainProbe`].
///
/// This replaces the old single flat-trim correction. That correction could not converge on
/// high-crest content: a flat post-limiter trim preserves crest, so raising the make-up gain
/// raised the true peak in lock-step, the trim shrank by the same factor, and the integrated
/// loudness was invariant — pinned several LU below target with the true peak sitting exactly on
/// the ceiling. Here the *limiter* owns the ceiling (it rides peaks down, reducing crest), and we
/// search the drive gain that lands the limited loudness on target with the true peak ≤ ceiling
/// by construction.
///
/// The search is a bounded, deterministic root-find: a first deficit (slope-1) step, then
/// secant steps while unbracketed — falling back to a fixed march when the local slope goes flat
/// or negative (the limiter can *lose* loudness when over-driven, so `l_final(gain)` is not
/// globally monotonic) — and false-position once the target is bracketed. Every step is clamped
/// and the whole thing is capped, so the number of probes depends only on the content.
pub(crate) fn converge_drive_gain(
    target: f64,
    l0: f64,
    mut probe: impl FnMut(f64) -> Result<GainProbe, DspError>,
) -> Result<GainSolution, DspError> {
    let g0 = if l0.is_finite() { target - l0 } else { 0.0 };

    // Silence / degenerate input: there is nothing to normalize. Probe once at unity make-up so
    // the render still gets a valid trim, and report it as converged (no spurious residual warn).
    if !l0.is_finite() {
        let p = probe(g0)?;
        return Ok(GainSolution {
            gain_db: g0,
            trim: p.trim,
            l_final: p.l_final,
            converged: true,
        });
    }

    let g_hi_cap = g0 + LOUDNESS_MAX_EXTRA_DRIVE_DB;
    // We only ever undershoot at g0 (the limiter attenuates, never boosts), but allow a little
    // headroom below in case an overshoot has to be walked back during refinement.
    let g_lo_cap = g0 - LOUDNESS_MAX_STEP_DB;

    let mut g = g0;
    let mut best_err = f64::INFINITY;
    let mut best = GainSolution {
        gain_db: g0,
        trim: 1.0,
        l_final: f64::NEG_INFINITY,
        converged: false,
    };
    // Bracket: `lo` = highest gain seen with l_final < target; `hi` = lowest with l_final ≥ target.
    let mut lo: Option<(f64, f64)> = None;
    let mut hi: Option<(f64, f64)> = None;
    let mut prev: Option<(f64, f64)> = None;

    for _ in 0..LOUDNESS_MAX_PASSES {
        let g_cl = g.clamp(g_lo_cap, g_hi_cap);
        let p = probe(g_cl)?;
        let lf = p.l_final;
        let err = if lf.is_finite() {
            target - lf
        } else {
            f64::INFINITY
        };
        if err.abs() < best_err {
            best_err = err.abs();
            best = GainSolution {
                gain_db: g_cl,
                trim: p.trim,
                l_final: lf,
                converged: false,
            };
        }
        if err.abs() <= LOUDNESS_TOL_TIGHT {
            break;
        }
        // Maintain the bracket from finite probes only.
        if lf.is_finite() {
            if lf < target {
                if lo.is_none_or(|(lg, _)| g_cl > lg) {
                    lo = Some((g_cl, lf));
                }
            } else if hi.is_none_or(|(hg, _)| g_cl < hg) {
                hi = Some((g_cl, lf));
            }
        }
        // Pinned at a cap and still on the wrong side: no more drive to give.
        if (g_cl >= g_hi_cap && err > 0.0) || (g_cl <= g_lo_cap && err < 0.0) {
            break;
        }

        let next = match (lo, hi) {
            // Bracketed: false-position across the whole [lo, hi] span. Interpolating the *net*
            // trend is robust to a non-monotonic interior (an over-limiting valley between them).
            (Some((lg, llf)), Some((hg, hlf))) if (hlf - llf).abs() > 1e-9 => {
                lg + (target - llf) * (hg - lg) / (hlf - llf)
            }
            // Unbracketed: secant off the previous probe when the slope is sane, else march a
            // fixed step toward the deficit (climbs out of a flat/negative-slope valley).
            _ => match prev {
                Some((pg, plf)) => {
                    let slope = (lf - plf) / (g_cl - pg);
                    if slope.is_finite() && slope > 0.05 {
                        g_cl + (target - lf) / slope
                    } else {
                        g_cl + err.signum() * LOUDNESS_MAX_STEP_DB
                    }
                }
                // First correction: the old one-shot's deficit step (slope-1 assumption).
                None => g_cl + err,
            },
        };
        prev = Some((g_cl, lf));
        let step = (next - g_cl).clamp(-LOUDNESS_MAX_STEP_DB, LOUDNESS_MAX_STEP_DB);
        g = (g_cl + step).clamp(g_lo_cap, g_hi_cap);
    }

    // Converged if we reached the contract; also "converged" (no warn) if the material is silent.
    best.converged =
        !best.l_final.is_finite() || (target - best.l_final).abs() <= LOUDNESS_TOL_CONTRACT;
    Ok(best)
}

/// Integrated loudness (LUFS) of a buffer, or −inf on failure/silence.
fn measure_integrated(buf: &AudioBuffer) -> f64 {
    measure_loudness(buf).integrated_lufs
}

/// Measure integrated loudness, true peak, and LRA of a buffer for the report.
pub fn measure_loudness(buf: &AudioBuffer) -> LoudnessSnapshot {
    let ch = buf.channel_count() as u32;
    let frames = buf.frames();
    if ch == 0 || frames == 0 {
        return LoudnessSnapshot {
            integrated_lufs: -120.0,
            true_peak_dbtp: -120.0,
            loudness_range_lu: 0.0,
        };
    }
    let mut meter = match EbuR128::new(ch, buf.sample_rate(), Mode::I | Mode::LRA | Mode::TRUE_PEAK)
    {
        Ok(m) => m,
        Err(_) => {
            return LoudnessSnapshot {
                integrated_lufs: -120.0,
                true_peak_dbtp: -120.0,
                loudness_range_lu: 0.0,
            }
        }
    };
    let mut interleaved = vec![0.0f32; frames * ch as usize];
    for c in 0..ch as usize {
        for (f, &s) in buf.channel(c).iter().enumerate() {
            interleaved[f * ch as usize + c] = s;
        }
    }
    let _ = meter.add_frames_f32(&interleaved);
    let integrated = meter.loudness_global().unwrap_or(f64::NEG_INFINITY);
    let lra = meter.loudness_range().unwrap_or(0.0);
    let tp = measure_true_peak(buf).unwrap_or(0.0);
    LoudnessSnapshot {
        integrated_lufs: if integrated.is_finite() {
            integrated
        } else {
            -120.0
        },
        true_peak_dbtp: if tp > 0.0 {
            20.0 * (tp as f64).log10()
        } else {
            -120.0
        },
        loudness_range_lu: if lra.is_finite() { lra } else { 0.0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Analyzer;
    use std::f32::consts::TAU;

    fn analyze(buf: &AudioBuffer) -> AnalysisReport {
        let mut a = Analyzer::new(buf.channel_count()).unwrap();
        a.push(buf).unwrap();
        a.finish(1)
    }

    fn noisy_tone(secs: usize) -> AudioBuffer {
        let n = 48_000 * secs;
        let mut seed = 0xABCD_1234u32;
        let s: Vec<f32> = (0..n)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                0.2 * (i as f32 * 180.0 * TAU / 48_000.0).sin() + 0.03 * noise
            })
            .collect();
        AudioBuffer::from_planar(vec![s.clone(), s], 48_000)
    }

    #[test]
    fn master_hits_target_and_ceiling() {
        let buf = noisy_tone(6);
        let report = analyze(&buf);
        let preset = Preset::default(); // −16 LUFS, −1 dBTP
        let mut cfg = auto_configure(&report, &preset, Tier::Standard);
        // This test is about the two-pass loudness math (§4.9) and the true-peak limiter
        // (§4.10) — not the denoiser. The fixture is a sustained 180 Hz tone, and the Standard
        // tier is now DeepFilterNet3, a *speech* model: it correctly suppresses a non-speech
        // tone almost entirely, which would leave the normalizer nothing to bring to target.
        // Take denoise out of the picture so this test measures what it claims to.
        cfg.denoise = None;
        let mut chain = Chain::new(buf.sample_rate());
        let outcome = chain.render(&buf, &cfg);

        assert!(
            (outcome.after.integrated_lufs - preset.target_lufs as f64).abs() <= 0.5,
            "integrated {} not within 0.5 LU of target {}",
            outcome.after.integrated_lufs,
            preset.target_lufs
        );
        assert!(
            outcome.after.true_peak_dbtp <= preset.true_peak_ceiling_dbtp as f64 + 0.01,
            "true peak {} exceeded ceiling {}",
            outcome.after.true_peak_dbtp,
            preset.true_peak_ceiling_dbtp
        );
    }

    #[test]
    fn stage_cache_reuses_front_stages_on_downstream_tweak() {
        let buf = noisy_tone(3);
        let report = analyze(&buf);
        let preset = Preset::default();
        let mut cfg = auto_configure(&report, &preset, Tier::Standard);
        let mut chain = Chain::new(buf.sample_rate());

        let first = chain.render(&buf, &cfg);
        assert!(first.cache_hits.is_empty(), "cold cache: no hits");

        // Tweak only a downstream module (the limiter ceiling).
        cfg.limiter.ceiling_dbtp = -2.0;
        let second = chain.render(&buf, &cfg);
        // Every front stage should be reused (all sit upstream of the limiter).
        for stage in [
            "downmix",
            "dc_hpf",
            "dehum",
            "declip",
            "declick",
            "decrackle",
            "denoise",
            "breath",
            "deess",
            "autoeq",
            "leveler",
        ] {
            assert!(
                second.cache_hits.contains(&stage),
                "stage {stage} should be a cache hit"
            );
        }
        assert!(second.after.true_peak_dbtp <= -2.0 + 0.01);
    }

    #[test]
    fn full_master_is_deterministic_double_render() {
        // A repaired render must be bit-identical on a re-render from a cold cache.
        let buf = noisy_tone(4);
        let report = analyze(&buf);
        let cfg = auto_configure(&report, &Preset::default(), Tier::Standard);

        let mut chain_a = Chain::new(buf.sample_rate());
        let mut chain_b = Chain::new(buf.sample_rate());
        let a = chain_a.render(&buf, &cfg);
        let b = chain_b.render(&buf, &cfg);
        assert_eq!(a.audio, b.audio, "double render must be bit-identical");
    }

    /// A peaky, high-crest signal (in-phase harmonics ≈ a pulse train). At target loudness its
    /// true peak wants to sit far over the ceiling, so the *limiter* must ride it down — the exact
    /// shape the old single flat-trim correction could not normalize (it pinned the true peak on
    /// the ceiling and left integrated loudness several LU below target). The convergence must now
    /// land integrated on target with the true peak still on the ceiling.
    fn high_crest_pulse(secs: usize) -> AudioBuffer {
        use std::f32::consts::TAU;
        let n = 48_000 * secs;
        let f0 = 120.0;
        let harmonics = 12;
        let mut seed = 0x51ED_2A17u32;
        let s: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                let mut v = 0.0f32;
                for k in 1..=harmonics {
                    v += (t * f0 * k as f32 * TAU).cos();
                }
                v /= harmonics as f32;
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                0.7 * v + 0.02 * noise
            })
            .collect();
        AudioBuffer::from_planar(vec![s.clone(), s], 48_000)
    }

    #[test]
    fn high_crest_loudness_converges_into_the_limiter() {
        let buf = high_crest_pulse(5);
        let report = analyze(&buf);
        let preset = Preset::default(); // −16 LUFS, −1 dBTP
        let mut cfg = auto_configure(&report, &preset, Tier::Standard);
        // Isolate the loudness/limiter stage: repair + denoise would reshape the crest we are
        // deliberately testing. (The streaming/parity end-to-end test exercises the full chain.)
        cfg.denoise = None;
        cfg.declip = None;
        cfg.declick = None;
        cfg.decrackle = None;
        cfg.dehum = None;
        cfg.deess = None;
        cfg.autoeq = None;
        cfg.breath = None;

        let mut chain = Chain::new(buf.sample_rate());
        let outcome = chain.render(&buf, &cfg);
        let target = preset.target_lufs as f64;
        let ceiling = preset.true_peak_ceiling_dbtp as f64;

        // Converged inside the ±0.3 LU the iteration aims for (tighter than the ±0.5 contract).
        assert!(
            (outcome.after.integrated_lufs - target).abs() <= 0.3,
            "high-crest integrated {} not within 0.3 LU of target {}",
            outcome.after.integrated_lufs,
            target
        );
        // True peak never over the ceiling (zero-tolerance guarantee) ...
        assert!(
            outcome.after.true_peak_dbtp <= ceiling + 0.01,
            "true peak {} over ceiling {}",
            outcome.after.true_peak_dbtp,
            ceiling
        );
        // ... and driven right up to it: proof the *limiter* reached target (not a gain-only path),
        // i.e. the fixture actually exercises the flat-trim pathology this fix removes.
        assert!(
            outcome.after.true_peak_dbtp >= ceiling - 0.5,
            "true peak {} did not reach the ceiling — limiter never engaged, so this fixture would \
             not guard the flat-trim undershoot",
            outcome.after.true_peak_dbtp
        );
    }

    #[test]
    fn dehum_engages_only_when_hum_present() {
        let report = analyze(&noisy_tone(3)); // clean tone, no hum
        let cfg = auto_configure(&report, &Preset::default(), Tier::Standard);
        assert!(
            cfg.dehum.is_none(),
            "no hum ⇒ no de-hum (never notch blind)"
        );

        let mut hummy = analyze(&noisy_tone(3));
        hummy.hum = Some(crate::analysis::Hum {
            fundamental_hz: 60.0,
            stable: true,
        });
        let cfg2 = auto_configure(&hummy, &Preset::default(), Tier::Standard);
        let dh = cfg2.dehum.expect("hum present ⇒ de-hum engages");
        assert_eq!(dh.fundamental_hz, 60.0);
    }

    #[test]
    fn repair_modules_off_in_music_mode() {
        let mut report = analyze(&noisy_tone(3));
        report.music_ratio = 0.8; // music-majority
        report.speech_ratio = 0.1;
        report.clipping_regions = 40; // even badly clipped, percussion is never rebuilt
        let cfg = auto_configure(&report, &Preset::default(), Tier::Standard);
        assert!(cfg.declip.is_none(), "de-clip off in music mode (§4.3)");
        assert!(cfg.declick.is_none(), "de-click off in music mode");
        assert!(cfg.decrackle.is_none(), "de-crackle off in music mode");
        assert!(cfg.breath.is_none(), "breath off in music mode");
        assert!(cfg.deess.is_none(), "de-ess off in music mode");
        assert!(cfg.autoeq.is_none(), "AutoEQ off in music mode");
        assert!(
            cfg.health_card
                .iter()
                .any(|f| f.title == "Clipping detected" && f.severity == "warn"),
            "the suppressed de-clip must still be surfaced"
        );
    }

    #[test]
    fn declip_engages_only_above_the_clipping_trigger() {
        // 03 §2: "Clipping regions > 3 → de-clip on".
        let base = analyze(&noisy_tone(3));
        for (regions, expect) in [(0u32, false), (3, false), (4, true), (40, true)] {
            let mut report = base.clone();
            report.clipping_regions = regions;
            let cfg = auto_configure(&report, &Preset::default(), Tier::Standard);
            assert_eq!(
                cfg.declip.is_some(),
                expect,
                "{regions} clipped regions → de-clip {expect}"
            );
        }
    }

    #[test]
    fn declip_repairs_a_clipped_master_end_to_end() {
        // A hot, hard-clipped tone through the whole chain: the auto-decision must engage
        // de-clip, and the master must still land on target and under the ceiling.
        let n = 48_000 * 5;
        let s: Vec<f32> = (0..n)
            .map(|i| (1.6 * (i as f32 * 200.0 * TAU / 48_000.0).sin()).clamp(-1.0, 1.0))
            .collect();
        let buf = AudioBuffer::from_planar(vec![s.clone(), s], 48_000);
        let report = analyze(&buf);
        assert!(
            report.clipping_regions > 3,
            "fixture should read as clipped, got {}",
            report.clipping_regions
        );

        let preset = Preset::default();
        let cfg = auto_configure(&report, &preset, Tier::Standard);
        assert!(cfg.declip.is_some(), "clipped source ⇒ de-clip engaged");

        let mut chain = Chain::new(buf.sample_rate());
        let outcome = chain.render(&buf, &cfg);
        assert!(
            (outcome.after.integrated_lufs - preset.target_lufs as f64).abs() <= 0.5,
            "integrated {} off target",
            outcome.after.integrated_lufs
        );
        assert!(outcome.after.true_peak_dbtp <= preset.true_peak_ceiling_dbtp as f64 + 0.01);

        // And the repair survives the chain: the output is measurably less clipped.
        let clipped_samples = |x: &[f32]| -> usize {
            let peak = x.iter().fold(0.0f32, |m, &v| m.max(v.abs()));
            let level = peak * 0.999;
            x.iter().filter(|&&v| v.abs() >= level).count()
        };
        assert!(
            clipped_samples(outcome.audio.channel(0)) * 10 < clipped_samples(buf.channel(0)),
            "the mastered output should hold far fewer at-ceiling samples"
        );
    }

    #[test]
    fn auto_configure_skips_denoise_when_clean() {
        let mut report = analyze(&noisy_tone(3));
        report.snr_db = 50.0; // pretend it's pristine
        let cfg = auto_configure(&report, &Preset::default(), Tier::Standard);
        assert!(cfg.denoise.is_none());
    }

    #[test]
    fn auto_configure_engages_denoise_when_noisy() {
        let mut report = analyze(&noisy_tone(3));
        report.snr_db = 20.0;
        let cfg = auto_configure(&report, &Preset::default(), Tier::Standard);
        let d = cfg.denoise.expect("denoise should engage at 20 dB SNR");
        assert!(d.strength > 0.3);
    }

    // --- Per-speaker leveling: the quiet-guest gate (06 §2) --------------------------------

    /// The chain config for the quiet-guest fixture, with denoise taken out of the picture.
    ///
    /// The fixture is synthetic tones, and the Standard tier's denoiser is DeepFilterNet3 — a
    /// *speech* model, which correctly suppresses a non-speech tone almost entirely and would
    /// leave the leveler nothing to measure. This test is about §4.8, not §4.4 (the existing
    /// `master_hits_target_and_ceiling` makes the same trade for the same reason).
    fn quiet_guest_config(
        buf: &AudioBuffer,
        diar: &anvil_asr::Diarization,
        memory: Option<&VoiceMemory>,
    ) -> (ChainConfig, Preset) {
        let preset = Preset::default(); // −16 LUFS, −1 dBTP
        let report = analyze(buf);
        let mut cfg =
            auto_configure_with_diarization(&report, &preset, Tier::Standard, Some(diar), memory);
        cfg.denoise = None;
        (cfg, preset)
    }

    /// **The M4 gate (06 §2): "quiet-guest fixture Δmedian ≤ 1 LU post".**
    ///
    /// A host and a guest 12 LU apart, alternating turns. After the full chain their median
    /// speech loudnesses must agree to within 1 LU — which no window-based AGC can promise,
    /// because by the time it has ramped onto the guest the guest has stopped talking.
    #[test]
    fn quiet_guest_medians_land_within_one_lu() {
        let (buf, diar) = crate::speaker::tests::quiet_guest_fixture();
        let before = crate::speaker::tests::speaker_medians(&buf, &diar, -60.0);
        let gap_before = (before[0] - before[1]).abs();
        assert!(
            gap_before > 10.0,
            "the fixture must actually pose the problem: Δ {gap_before} LU"
        );

        let (cfg, preset) = quiet_guest_config(&buf, &diar, None);
        assert!(cfg.speaker.is_some(), "diarization ⇒ per-speaker leveling");
        let mut chain = Chain::new(buf.sample_rate());
        let outcome = chain.render(&buf, &cfg);

        let after = crate::speaker::tests::speaker_medians(&outcome.audio, &diar, -60.0);
        let gap_after = (after[0] - after[1]).abs();
        println!(
            "quiet-guest gate: Δmedian {gap_before:.2} LU → {gap_after:.2} LU \
             (host {:.2} → {:.2}, guest {:.2} → {:.2})\n  applied: {:#?}",
            before[0], after[0], before[1], after[1], outcome.speaker_gains
        );
        assert!(
            gap_after <= 1.0,
            "06 §2 gate: post-master Δmedian must be ≤ 1 LU, got {gap_after:.2} LU ({after:?})"
        );

        // The rest of the chain's promises still hold on this file.
        assert!(
            (outcome.after.integrated_lufs - preset.target_lufs as f64).abs() <= 0.5,
            "integrated {} off target",
            outcome.after.integrated_lufs
        );
        assert!(outcome.after.true_peak_dbtp <= preset.true_peak_ceiling_dbtp as f64 + 0.01);

        // And the report says what it did.
        let report = build_master_report(analyze(&buf), &cfg, &outcome);
        let spk = report
            .modules
            .iter()
            .find(|m| m.name == "speaker")
            .expect("speaker module reported");
        assert!(spk.engaged);
        assert!(spk.detail.contains("Guest"), "detail: {}", spk.detail);
        assert!(report
            .health_card
            .iter()
            .any(|f| f.title == "Per-speaker leveling"));
    }

    /// The same fixture with the window AGC alone (no diarization) — the control. It must *not*
    /// close the gap, or the per-speaker stage would be solving a problem that does not exist.
    #[test]
    fn the_window_agc_alone_cannot_close_the_quiet_guest_gap() {
        let (buf, diar) = crate::speaker::tests::quiet_guest_fixture();
        let report = analyze(&buf);
        let mut cfg = auto_configure(&report, &Preset::default(), Tier::Standard);
        cfg.denoise = None;
        assert!(
            cfg.speaker.is_none(),
            "no diarization ⇒ no per-speaker stage"
        );

        let mut chain = Chain::new(buf.sample_rate());
        let outcome = chain.render(&buf, &cfg);
        let after = crate::speaker::tests::speaker_medians(&outcome.audio, &diar, -60.0);
        let gap = (after[0] - after[1]).abs();
        assert!(
            gap > 1.0,
            "if a 3 s-window AGC could already hold the medians within 1 LU ({gap:.2}), M4 would \
             be pointless — this control is what makes the gate mean something"
        );
    }

    /// Voice Memory: a returning guest whose profile we already hold gets the same treatment even
    /// when this episode gives us too little of them to measure.
    #[test]
    fn voice_memory_levels_a_speaker_we_cannot_measure_this_episode() {
        let (buf, diar) = crate::speaker::tests::quiet_guest_fixture();
        let memory = VoiceMemory::new(vec![
            crate::speaker::SpeakerProfile::new("Host", -15.7),
            crate::speaker::SpeakerProfile::new("Guest", -27.7),
        ]);
        let (mut cfg, _) = quiet_guest_config(&buf, &diar, Some(&memory));
        // Nobody clears the bar this episode ⇒ every median must come from memory.
        cfg.speaker.as_mut().unwrap().min_speech_secs = 1e9;

        let mut chain = Chain::new(buf.sample_rate());
        let outcome = chain.render(&buf, &cfg);
        assert!(outcome
            .speaker_gains
            .iter()
            .all(|g| g.source == crate::speaker::MedianSource::Remembered));

        let after = crate::speaker::tests::speaker_medians(&outcome.audio, &diar, -60.0);
        let gap = (after[0] - after[1]).abs();
        assert!(
            gap <= 1.0,
            "remembered profiles must level just as well as measured ones: Δ {gap:.2} LU"
        );
    }

    /// The master hands back profiles for next episode, and they describe the cast it just heard.
    #[test]
    fn a_diarized_render_derives_voice_memory_profiles() {
        let (buf, diar) = crate::speaker::tests::quiet_guest_fixture();
        let (mut cfg, _) = quiet_guest_config(&buf, &diar, None);
        cfg.speaker.as_mut().unwrap().profile_autoeq = Some(AutoEqConfig::default());

        let mut chain = Chain::new(buf.sample_rate());
        let outcome = chain.render(&buf, &cfg);

        let memory = &outcome.voice_memory;
        assert_eq!(memory.profiles.len(), 2);
        let guest = memory.get("Guest").expect("the guest is remembered");
        let host = memory.get("Host").expect("the host is remembered");
        assert!(
            host.median_lufs > guest.median_lufs + 10.0,
            "the profiles must record the gap as it *arrived* (pre-leveling), not post: {memory:?}"
        );
    }

    #[test]
    fn per_speaker_leveling_is_deterministic() {
        let (buf, diar) = crate::speaker::tests::quiet_guest_fixture();
        let (cfg, _) = quiet_guest_config(&buf, &diar, None);
        let mut a = Chain::new(buf.sample_rate());
        let mut b = Chain::new(buf.sample_rate());
        assert_eq!(
            a.render(&buf, &cfg).audio,
            b.render(&buf, &cfg).audio,
            "double render must be bit-identical"
        );
    }

    /// Music-majority material is left to the music-mode AGC: normalizing "speakers" across a
    /// music bed would flatten dynamics that are there on purpose.
    #[test]
    fn per_speaker_leveling_stays_off_in_music_mode() {
        let (buf, diar) = crate::speaker::tests::quiet_guest_fixture();
        let mut report = analyze(&buf);
        report.music_ratio = 0.8;
        report.speech_ratio = 0.1;
        let cfg = auto_configure_with_diarization(
            &report,
            &Preset::default(),
            Tier::Standard,
            Some(&diar),
            None,
        );
        assert!(
            cfg.speaker.is_none(),
            "music-majority ⇒ no per-speaker stage"
        );
    }

    #[test]
    fn dual_mono_collapses_to_mono_output() {
        let buf = noisy_tone(3); // both channels identical
        let mut report = analyze(&buf);
        // force dual-mono decision
        if let Some(s) = report.stereo.as_mut() {
            s.dual_mono = true;
        }
        let cfg = auto_configure(&report, &Preset::default(), Tier::Standard);
        assert!(cfg.downmix_mono);
        let mut chain = Chain::new(buf.sample_rate());
        let outcome = chain.render(&buf, &cfg);
        assert_eq!(outcome.audio.channel_count(), 1);
    }
}
