//! Per-speaker leveling + Voice Memory (03 §4.8 "Per-speaker mode", §4.7 "Per-speaker when
//! diarization available").
//!
//! ## Why this exists
//! A window-based AGC can only ever *chase* the level: it sees a 3 s trailing window, so when a
//! −28 LUFS guest answers a −16 LUFS host it spends a second or two ramping, and when the host
//! cuts back in it ramps the other way. That is the pumping every podcast leveler is accused of,
//! and no amount of slew tuning fixes it — the information the AGC needs (*who is talking*) is
//! not in the envelope.
//!
//! Diarization has that information. So: measure each speaker's **median speech-gated
//! short-term loudness** over the whole file, work out the **static gain** that puts every
//! speaker on the same target, and apply it *before* the AGC ever runs. The AGC then has almost
//! nothing left to do — which is precisely the point. This is a per-speaker *normalization*, not
//! a second compressor.
//!
//! ## What runs, in order
//! 1. **Median measurement**, on the audio as it arrives. This is the one frame of reference for
//!    everything else: it is the number a [`SpeakerProfile`] stores, and the number a stored
//!    profile substitutes for. Measure it anywhere else in the stage and a profile written this
//!    episode would not mean the same thing when it is read next episode.
//! 2. **Voice Memory EQ** (§4.7): for any speaker with a stored profile carrying an AutoEQ curve,
//!    that curve is applied over that speaker's turns and crossfaded in/out — so a returning host
//!    gets the same voicing episode to episode. (Its small effect on level is left to the AGC
//!    downstream, which is chasing the same target and will absorb a fraction of a dB.)
//! 3. **Static per-speaker gain**, crossfaded at every turn boundary.
//!
//! ## No clicks
//! Gains and EQ weights are built as piecewise-constant per-hop envelopes and then smoothed with
//! a **centred moving average** of `crossfade_ms`. A moving average over a step yields a linear
//! ramp centred on the boundary, so a turn change becomes a `crossfade_ms` crossfade rather than
//! a discontinuity, and the per-sample gain step is bounded by (total change) / (crossfade
//! samples). Deterministic (ADR-003): fixed windows, sequential math, no entropy.
//!
//! ## Fallback
//! No diarization ⇒ this stage does not exist in the chain and the single-speaker AGC path in
//! [`crate::leveler`] is untouched. A diarized file with a speaker we cannot measure (too little
//! speech) falls back to that speaker's remembered median, and failing that to 0 dB — never to a
//! guess.

use anvil_asr::{Diarization, SpeakerSegment};
use anvil_core::HOP_SAMPLES;
use anvil_media::AudioBuffer;
use serde::{Deserialize, Serialize};

use crate::autoeq::{AutoEq, AutoEqConfig, BandFit};
use crate::biquad::{Biquad, KWeighting};
use crate::Processor;

/// Short-term loudness window: 3 s of hops (03 §4.8 — the same window the AGC tracks).
const ST_WINDOW_HOPS: usize = 300;
/// BS.1770 loudness offset for a single K-weighted channel.
const LUFS_OFFSET: f32 = -0.691;

/// The remembered treatment for one speaker — **the Voice Memory record**.
///
/// This is the struct the storage lane (`anvil-project`) persists per show and hands back on the
/// next episode. It is deliberately plain: three numbers and a curve, all serde, no handles into
/// the DSP.
///
/// - `speaker_label` — the *user-facing* name ("Rob", "Guest"), which is what survives across
///   episodes. Diarization ids do not: cluster ids are per-run, so id 0 in episode 12 need not be
///   the same human as id 0 in episode 11. The label is the key.
/// - `median_lufs` — the speaker's median speech-gated short-term loudness as measured last time.
///   Used as a **prior**: if this episode gives us too little of that speaker to measure honestly
///   (a guest with two sentences), we level them from the remembered median instead of from a
///   noisy one.
/// - `gain_offset_db` — a trim on top of the computed gain ("Rob always likes to sit 1 dB hot").
///   The user's knob; defaults to 0.
/// - `eq_bands` — the AutoEQ curve fitted to that speaker's voice (§4.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeakerProfile {
    /// User-facing speaker name — the key that survives across episodes.
    pub speaker_label: String,
    /// Median speech-gated short-term loudness, LUFS, from the render this profile was derived
    /// from.
    pub median_lufs: f32,
    /// User trim applied on top of the computed per-speaker gain, dB (default 0).
    pub gain_offset_db: f32,
    /// The speaker's AutoEQ curve (§4.7). Empty = no remembered EQ.
    pub eq_bands: Vec<BandFit>,
}

impl SpeakerProfile {
    /// A profile with no EQ and no trim — just a remembered loudness.
    pub fn new(speaker_label: impl Into<String>, median_lufs: f32) -> Self {
        Self {
            speaker_label: speaker_label.into(),
            median_lufs,
            gain_offset_db: 0.0,
            eq_bands: Vec::new(),
        }
    }
}

/// Everything remembered about a show's cast — what `anvil-project` stores and reloads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VoiceMemory {
    /// One profile per known speaker, keyed by [`SpeakerProfile::speaker_label`].
    pub profiles: Vec<SpeakerProfile>,
}

impl VoiceMemory {
    /// Build from a list of profiles.
    pub fn new(profiles: Vec<SpeakerProfile>) -> Self {
        Self { profiles }
    }

    /// The profile for `label`, if we have met them before.
    pub fn get(&self, label: &str) -> Option<&SpeakerProfile> {
        self.profiles.iter().find(|p| p.speaker_label == label)
    }

    /// No speakers remembered.
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }
}

/// Per-speaker leveling configuration (03 §4.8, per-speaker mode).
///
/// Carries the [`Diarization`] itself: the stage is meaningless without one, and folding it into
/// the config is what lets the chain's stage cache key on "the same audio, the same speakers,
/// the same memory ⇒ the same output".
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerLevelingConfig {
    /// Who spoke when.
    pub diarization: Diarization,
    /// Remembered per-speaker treatment. Empty = first time we have seen this cast.
    pub memory: VoiceMemory,
    /// The common target every speaker's median is normalized to (LUFS) — the same short-term
    /// target the AGC downstream uses, so the AGC opens with nothing to correct.
    pub target_lufs: f32,
    /// Maximum magnitude of the static per-speaker gain, dB (03: ±12).
    pub max_gain_db: f32,
    /// Crossfade length at a turn boundary, ms.
    pub crossfade_ms: f32,
    /// Loudness (LUFS) below which a hop is not speech and is excluded from the median.
    pub noise_gate_lufs: f32,
    /// A speaker with less than this much gated speech cannot be measured honestly; we fall back
    /// to their remembered median, then to 0 dB.
    pub min_speech_secs: f32,
    /// When `Some`, the stage also **derives fresh profiles** for this cast as it renders (fitting
    /// each speaker's AutoEQ curve with this config) and hands them back via
    /// [`SpeakerLeveler::derived`] / [`crate::RenderOutcome::voice_memory`], ready for the storage
    /// lane to persist. `None` = apply-only.
    pub profile_autoeq: Option<AutoEqConfig>,
}

impl SpeakerLevelingConfig {
    /// A config for `diarization` at `target_lufs`, with the rest at spec defaults.
    pub fn new(diarization: Diarization, target_lufs: f32) -> Self {
        Self {
            diarization,
            memory: VoiceMemory::default(),
            target_lufs,
            max_gain_db: 12.0,
            crossfade_ms: 120.0,
            noise_gate_lufs: -50.0,
            min_speech_secs: 1.5,
            profile_autoeq: None,
        }
    }

    /// Is there anything for this stage to do?
    pub fn engaged(&self) -> bool {
        !self.diarization.speakers.is_empty() && !self.diarization.segments.is_empty()
    }
}

/// Where a speaker's median came from — surfaced in the report, because "we remembered you" and
/// "we measured you" are different claims and the Health Card must not conflate them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MedianSource {
    /// Measured from this file's audio.
    Measured,
    /// Not enough speech in this file — taken from the stored [`SpeakerProfile`].
    Remembered,
    /// Neither available: the speaker is left alone (0 dB).
    Unknown,
}

/// What the stage actually did to one speaker (for [`crate::MasterReport`] and the tests).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeakerGain {
    /// Diarization speaker id.
    pub speaker: u32,
    /// Display label (the Voice Memory key).
    pub label: String,
    /// The median short-term loudness this speaker was leveled from, LUFS.
    pub median_lufs: f32,
    /// Where that median came from.
    pub source: MedianSource,
    /// The static gain applied to this speaker's turns, dB (includes any profile trim).
    pub gain_db: f32,
    /// Number of AutoEQ bands applied from Voice Memory (0 = none).
    pub eq_bands: usize,
}

/// The per-speaker leveler. Consumes a [`SpeakerLevelingConfig`], mutates the buffer, and reports
/// what it did via [`SpeakerLeveler::applied`].
#[derive(Debug, Clone)]
pub struct SpeakerLeveler {
    config: SpeakerLevelingConfig,
    sample_rate: u32,
    applied: Vec<SpeakerGain>,
    derived: VoiceMemory,
    common_offset_db: f32,
}

impl SpeakerLeveler {
    /// Build for `sample_rate` with `config`.
    pub fn new(sample_rate: u32, config: SpeakerLevelingConfig) -> Self {
        Self {
            config,
            sample_rate,
            applied: Vec::new(),
            derived: VoiceMemory::default(),
            common_offset_db: 0.0,
        }
    }

    /// What was applied, per speaker, by the last [`Processor::process`] call.
    pub fn applied(&self) -> &[SpeakerGain] {
        &self.applied
    }

    /// The common offset (dB) the whole cast was slid by so every speaker's gain fitted inside
    /// ±`max_gain_db` — see the `process` implementation. Non-zero means the file as a whole sat
    /// too far from the target for a bounded static gain to reach, so the *spread* was preserved
    /// and the remaining level is left to the AGC and the two-pass normalize (which is what they
    /// are for). Zero in the ordinary case.
    pub fn common_offset_db(&self) -> f32 {
        self.common_offset_db
    }

    /// Fresh profiles for this cast, derived from the audio the last [`Processor::process`] call
    /// saw — what the storage lane persists. Empty unless
    /// [`SpeakerLevelingConfig::profile_autoeq`] was set.
    pub fn derived(&self) -> &VoiceMemory {
        &self.derived
    }

    /// The config.
    pub fn config(&self) -> &SpeakerLevelingConfig {
        &self.config
    }
}

impl Processor for SpeakerLeveler {
    fn process(&mut self, buffer: &mut AudioBuffer) {
        self.applied.clear();
        self.derived = VoiceMemory::default();
        self.common_offset_db = 0.0;
        let frames = buffer.frames();
        let channels = buffer.channel_count();
        if frames == 0 || channels == 0 || !self.config.engaged() {
            return;
        }

        let sr = self.sample_rate as f32;
        let hop = HOP_SAMPLES;
        let n_hops = frames.div_ceil(hop);
        let fade_hops = crossfade_hops(self.config.crossfade_ms, sr, hop);
        let gate = self.config.noise_gate_lufs;

        // Which hops belong to which speaker (a hop is attributed to the turn covering its
        // midpoint; where two turns overlap, the later one in segment order wins — the diarizer
        // emits genuine overlaps and *some* deterministic choice has to be made).
        let owner = hop_owners(&self.config.diarization.segments, n_hops, hop, sr);

        // --- 1. Per-speaker median speech-gated short-term loudness ---------------------------
        // Measured on the audio as it arrives, before this stage changes anything — the frame of
        // reference a `SpeakerProfile` is written in and read back in.
        let hop_ms = hop_mean_squares(buffer, hop);
        let min_hops = (self.config.min_speech_secs * sr / hop as f32)
            .ceil()
            .max(1.0) as usize;

        // Pass 1: each speaker's median, and the raw gain that would put them on the target.
        let mut raw: Vec<(f32, MedianSource, f32, usize)> = Vec::new(); // median, source, gain, eq
        for speaker in &self.config.diarization.speakers {
            let gated = gated_hops(&hop_ms, &owner, speaker.id, gate);

            let profile = self.config.memory.get(&speaker.label);
            let measured = if gated.len() >= min_hops {
                median_st_lufs(&gated)
            } else {
                None
            };

            // Measured beats remembered: a mic that moved since last episode must not be leveled
            // from last episode's number. Remembered beats nothing: a guest with two sentences is
            // better served by what we know of them than by leaving them buried.
            let (median, source) = match (measured, profile.map(|p| p.median_lufs)) {
                (Some(m), _) => (m, MedianSource::Measured),
                (None, Some(m)) => (m, MedianSource::Remembered),
                (None, None) => (0.0, MedianSource::Unknown),
            };
            let trim = profile.map(|p| p.gain_offset_db).unwrap_or(0.0);
            let gain = match source {
                MedianSource::Unknown => 0.0,
                _ => (self.config.target_lufs - median) + trim,
            };
            raw.push((
                median,
                source,
                gain,
                profile.map(|p| p.eq_bands.len()).unwrap_or(0),
            ));
        }

        // Pass 2: fit the cast inside ±max_gain **without breaking the spread**.
        //
        // This is the crux. Two quantities come out of pass 1: how far apart the speakers are,
        // and how far the cast as a whole is from the target. They are not equally precious.
        // The *spread* is the thing nothing downstream can fix — a window AGC cannot pull one
        // speaker up without pulling the other up with them. The *absolute offset* is the thing
        // every downstream stage fixes for free: the AGC targets the same number, and after it
        // the two-pass normalize (§4.9) applies an exact static gain to land the file on target.
        //
        // So when the required gains do not fit in ±max_gain, we slide the whole cast by a
        // common offset rather than clipping the extremes independently — the speakers stay
        // aligned with each other, and the level they sit at is somebody else's problem, which is
        // the correct division of labour. (A cast whose *spread* alone exceeds 2·max_gain is
        // genuinely beyond what a static gain can fix; there we clamp, and say so in the report.)
        let bounds = raw
            .iter()
            .filter(|(_, source, _, _)| *source != MedianSource::Unknown)
            .fold(None::<(f32, f32)>, |acc, &(_, _, g, _)| match acc {
                Some((lo, hi)) => Some((lo.min(g), hi.max(g))),
                None => Some((g, g)),
            });
        let max = self.config.max_gain_db;
        let offset = match bounds {
            // The smallest common shift that brings every speaker inside ±max_gain (0 when they
            // already fit).
            Some((lo, hi)) if hi - lo <= 2.0 * max => 0.0f32.clamp(-max - lo, max - hi),
            // Spread wider than any static gain can span: centre the cast so the clamping damage
            // is shared, rather than dumping all of it on one speaker.
            Some((lo, hi)) => -0.5 * (lo + hi),
            None => 0.0,
        };
        self.common_offset_db = offset;

        let mut gain_of: Vec<f32> = Vec::new(); // indexed by speaker id
        for (speaker, &(median, source, gain, eq_bands)) in
            self.config.diarization.speakers.iter().zip(&raw)
        {
            let gain_db = match source {
                // We know nothing about this speaker; a guess would be worse than unity.
                MedianSource::Unknown => 0.0,
                _ => (gain + offset).clamp(-max, max),
            };
            let idx = speaker.id as usize;
            if gain_of.len() <= idx {
                gain_of.resize(idx + 1, 0.0);
            }
            gain_of[idx] = gain_db;

            self.applied.push(SpeakerGain {
                speaker: speaker.id,
                label: speaker.label.clone(),
                median_lufs: median,
                source,
                gain_db,
                eq_bands,
            });
        }

        // --- 2. Derive fresh profiles (§4.7 Voice Memory), from that same untouched audio ------
        if let Some(eq) = self.config.profile_autoeq {
            self.derived = derive_profiles(buffer, &self.config.diarization, gate, Some(eq));
        }

        // --- 3. Voice Memory EQ (§4.7), crossfaded over each speaker's turns -------------------
        for speaker in &self.config.diarization.speakers {
            let Some(profile) = self.config.memory.get(&speaker.label) else {
                continue;
            };
            if profile.eq_bands.is_empty() {
                continue;
            }
            let hits: Vec<f32> = owner
                .iter()
                .map(|&who| if who == Some(speaker.id) { 1.0 } else { 0.0 })
                .collect();
            if hits.iter().all(|&w| w <= 0.0) {
                continue;
            }
            let weight = smoothed_envelope(&hits, fade_hops);
            apply_eq_weighted(buffer, &profile.eq_bands, &weight, self.sample_rate, hop);
        }

        // --- 4. The static per-speaker gain, crossfaded at every boundary ----------------------
        // Unattributed hops (nobody talking: music, silence, an intro sting) take 0 dB, and the
        // moving average slides them into and out of the neighbouring speakers' gains — so a turn
        // boundary that sits in a pause is a fade through unity, not a step.
        let target_db: Vec<f32> = owner
            .iter()
            .map(|&who| match who {
                Some(id) => gain_of.get(id as usize).copied().unwrap_or(0.0),
                None => 0.0,
            })
            .collect();
        let hop_gain_db = smoothed_envelope(&target_db, fade_hops);
        apply_hop_gain(buffer, &hop_gain_db, hop);
    }
}

/// BS.1770-4 relative gate: −10 LU below the mean of the absolutely-gated hops.
const RELATIVE_GATE_LU: f32 = 10.0;

/// A speaker's hop mean-squares, in time order, that count as **their speech**.
///
/// Two gates, exactly as BS.1770-4 does it for integrated loudness:
///
/// 1. an **absolute** gate (`gate`, set from the analysis noise floor) drops the obvious silence;
/// 2. a **relative** gate 10 LU below the mean of what survived drops the rest — the pauses
///    between syllables, the tail of a word, the room tone inside a turn.
///
/// The relative gate is not a nicety, it is what makes the measurement *level-invariant*, and
/// this module cannot work without that. Boosting a quiet guest by 12 dB lifts their room tone by
/// 12 dB too; against a fixed absolute gate that room tone would suddenly qualify as "speech" and
/// drag their median down, so a stage that had aligned two speakers perfectly would measure as
/// having missed by 2 LU. Gate relative to each speaker's own level and the question "how loud is
/// this person when they talk" gets the same answer no matter what gain is in front of it.
fn gated_hops(hop_ms: &[f32], owner: &[Option<u32>], speaker: u32, gate: f32) -> Vec<f32> {
    let absolute: Vec<f32> = owner
        .iter()
        .enumerate()
        .filter(|&(_, &who)| who == Some(speaker))
        .map(|(h, _)| hop_ms[h])
        .filter(|&ms| ms_to_lufs(ms) > gate)
        .collect();
    if absolute.is_empty() {
        return absolute;
    }
    let mean_ms = absolute.iter().sum::<f32>() / absolute.len() as f32;
    let relative = ms_to_lufs(mean_ms) - RELATIVE_GATE_LU;
    absolute
        .into_iter()
        .filter(|&ms| ms_to_lufs(ms) > relative)
        .collect()
}

/// Derive a fresh [`VoiceMemory`] from a render — the builder the storage lane calls after a
/// master so next episode starts from what we learned in this one (§4.7 "Voice Memory feature
/// stores these curves per show").
///
/// `buffer` should be the audio the profiles describe (the pre-normalize render is the honest
/// choice: it is the voice as the chain shaped it, before the file-wide loudness gain). `autoeq`
/// is the config the curves are fitted with; pass `None` to derive loudness-only profiles.
///
/// A speaker with no gated speech at all yields no profile — an empty record is worse than no
/// record, because next episode would level them from a lie.
pub fn derive_profiles(
    buffer: &AudioBuffer,
    diarization: &Diarization,
    noise_gate_lufs: f32,
    autoeq: Option<AutoEqConfig>,
) -> VoiceMemory {
    let frames = buffer.frames();
    if frames == 0 || buffer.channel_count() == 0 || diarization.segments.is_empty() {
        return VoiceMemory::default();
    }
    let hop = HOP_SAMPLES;
    let sr = buffer.sample_rate() as f32;
    let n_hops = frames.div_ceil(hop);
    let owner = hop_owners(&diarization.segments, n_hops, hop, sr);
    let hop_ms = hop_mean_squares(buffer, hop);

    let mut profiles = Vec::new();
    for speaker in &diarization.speakers {
        let gated = gated_hops(&hop_ms, &owner, speaker.id, noise_gate_lufs);
        let Some(median) = median_st_lufs(&gated) else {
            continue;
        };

        let eq_bands = match autoeq {
            Some(cfg) => {
                let solo = speaker_only_buffer(buffer, &owner, speaker.id, hop);
                AutoEq::new(buffer.sample_rate(), cfg).fit_bands(&solo)
            }
            None => Vec::new(),
        };

        profiles.push(SpeakerProfile {
            speaker_label: speaker.label.clone(),
            median_lufs: median,
            gain_offset_db: 0.0,
            eq_bands,
        });
    }
    VoiceMemory::new(profiles)
}

/// The median speech-gated **short-term** loudness of a speaker, from their gated hop
/// mean-squares in time order. `None` when there is nothing to measure.
///
/// Short-term = a 3 s window (03 §4.8). We slide that window over the speaker's *gated* hops —
/// so a 3 s window is 3 s of them actually talking, not 3 s of wall-clock that might be half the
/// other person. A speaker with less than one full window gets a single reading over whatever
/// they have, which is the honest answer for a short turn.
pub fn median_st_lufs(gated_hop_ms: &[f32]) -> Option<f32> {
    if gated_hop_ms.is_empty() {
        return None;
    }
    let window = ST_WINDOW_HOPS.min(gated_hop_ms.len());
    let mut sum: f32 = gated_hop_ms[..window].iter().sum();
    let mut readings = Vec::with_capacity(gated_hop_ms.len() - window + 1);
    readings.push(ms_to_lufs(sum / window as f32));
    for h in window..gated_hop_ms.len() {
        sum += gated_hop_ms[h] - gated_hop_ms[h - window];
        readings.push(ms_to_lufs(sum / window as f32));
    }
    Some(median(&mut readings))
}

/// Median of a slice (even counts average the two middles). Sorts in place with `total_cmp`, so
/// it is deterministic even with NaNs in the input.
fn median(values: &mut [f32]) -> f32 {
    values.sort_by(|a, b| a.total_cmp(b));
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        0.5 * (values[n / 2 - 1] + values[n / 2])
    }
}

/// LUFS of a K-weighted mean-square (the same mapping the AGC uses).
#[inline]
fn ms_to_lufs(mean_square: f32) -> f32 {
    if mean_square > 1e-12 {
        LUFS_OFFSET + 10.0 * mean_square.log10()
    } else {
        -120.0
    }
}

/// K-weighted mean-square of the mono downmix, per hop.
fn hop_mean_squares(buffer: &AudioBuffer, hop: usize) -> Vec<f32> {
    let frames = buffer.frames();
    let channels = buffer.channel_count();
    let n_hops = frames.div_ceil(hop);
    let mut out = vec![0.0f32; n_hops];
    let mut kw = KWeighting::default();
    let inv_ch = 1.0 / channels as f32;
    for (h, slot) in out.iter_mut().enumerate() {
        let start = h * hop;
        let end = (start + hop).min(frames);
        let mut acc = 0.0f32;
        for i in start..end {
            let mut mono = 0.0f32;
            for c in 0..channels {
                mono += buffer.channel(c)[i];
            }
            let k = kw.process(mono * inv_ch);
            acc += k * k;
        }
        *slot = acc / (end - start).max(1) as f32;
    }
    out
}

/// Which speaker owns each hop, by the turn covering the hop's midpoint. Segments are applied in
/// order, so where two turns overlap the later one wins — the diarizer emits overlaps
/// deliberately (two people talking at once) and *some* deterministic choice must be made.
fn hop_owners(
    segments: &[SpeakerSegment],
    n_hops: usize,
    hop: usize,
    sample_rate: f32,
) -> Vec<Option<u32>> {
    let mut owner = vec![None; n_hops];
    let hop_secs = hop as f64 / sample_rate as f64;
    for seg in segments {
        if seg.end <= seg.start {
            continue;
        }
        // Hops whose midpoint falls inside [start, end).
        let first = ((seg.start / hop_secs) - 0.5).ceil().max(0.0) as usize;
        let last = ((seg.end / hop_secs) - 0.5).ceil().max(0.0) as usize; // exclusive
        for slot in owner.iter_mut().take(last.min(n_hops)).skip(first) {
            *slot = Some(seg.speaker);
        }
    }
    owner
}

/// Crossfade length in hops: odd (so the moving average is centred) and at least 1.
fn crossfade_hops(crossfade_ms: f32, sample_rate: f32, hop: usize) -> usize {
    let samples = (crossfade_ms.max(0.0) / 1000.0) * sample_rate;
    let hops = (samples / hop as f32).round() as usize;
    let hops = hops.max(1);
    if hops.is_multiple_of(2) {
        hops + 1
    } else {
        hops
    }
}

/// Centred moving average of a piecewise-constant envelope, edge-replicated.
///
/// This *is* the crossfade: a boxcar of length `L` over a step produces a linear ramp of length
/// `L` centred on the step. So a turn boundary becomes a `L`-hop linear crossfade between the two
/// speakers' gains, and the per-sample gain step is bounded by (total change) / (L · hop).
fn smoothed_envelope(target: &[f32], window: usize) -> Vec<f32> {
    let n = target.len();
    if n == 0 {
        return Vec::new();
    }
    if window <= 1 {
        return target.to_vec();
    }
    let half = (window / 2) as isize;
    let at = |i: isize| -> f32 { target[i.clamp(0, n as isize - 1) as usize] };

    let mut out = vec![0.0f32; n];
    // Running sum over [i-half, i+half], edge-replicated.
    let mut sum: f32 = (-half..=half).map(at).sum();
    let inv = 1.0 / window as f32;
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = sum * inv;
        let i = i as isize;
        sum += at(i + half + 1) - at(i - half);
    }
    out
}

/// Apply a per-hop dB gain envelope, linearly interpolated between hop values (so the gain is
/// continuous at hop edges too).
fn apply_hop_gain(buffer: &mut AudioBuffer, hop_gain_db: &[f32], hop: usize) {
    let frames = buffer.frames();
    let channels = buffer.channel_count();
    let mut prev_lin = 10f32.powf(hop_gain_db[0] / 20.0);
    for (h, &db) in hop_gain_db.iter().enumerate() {
        let target_lin = 10f32.powf(db / 20.0);
        let start = h * hop;
        let end = (start + hop).min(frames);
        let span = (end - start).max(1) as f32;
        for i in start..end {
            let t = (i - start) as f32 / span;
            let g = prev_lin + (target_lin - prev_lin) * t;
            for c in 0..channels {
                buffer.channel_mut(c)[i] *= g;
            }
        }
        prev_lin = target_lin;
    }
}

/// Mix a bell-cascade-filtered copy of the buffer in under a per-hop weight envelope.
///
/// The filter runs over the **whole** buffer (its state never restarts), so the wet signal is
/// itself click-free; the weight envelope then fades that wet signal in over the speaker's turns.
/// Filtering only the speaker's spans instead would restart the biquad state at every turn — a
/// transient at every boundary, which is exactly what we are here to avoid.
fn apply_eq_weighted(
    buffer: &mut AudioBuffer,
    bands: &[BandFit],
    weight_per_hop: &[f32],
    sample_rate: u32,
    hop: usize,
) {
    let frames = buffer.frames();
    for channel in buffer.planar_mut() {
        let mut bells: Vec<Biquad> = bands
            .iter()
            .map(|b| Biquad::peaking(sample_rate as f32, b.center_hz, b.q.min(2.0), b.gain_db))
            .collect();
        let mut prev_w = weight_per_hop[0];
        for (h, &w) in weight_per_hop.iter().enumerate() {
            let start = h * hop;
            let end = (start + hop).min(frames);
            let span = (end - start).max(1) as f32;
            for (i, sample) in channel[start..end].iter_mut().enumerate() {
                let dry = *sample;
                let mut wet = dry;
                for bell in bells.iter_mut() {
                    wet = bell.process(wet);
                }
                let t = i as f32 / span;
                let weight = (prev_w + (w - prev_w) * t).clamp(0.0, 1.0);
                *sample = dry + (wet - dry) * weight;
            }
            prev_w = w;
        }
    }
}

/// The speaker's turns, concatenated — the material an LTAS fit for that speaker is measured on.
/// (The joins are irrelevant to a long-term *average* spectrum: a handful of straddling FFT
/// frames out of hundreds.)
fn speaker_only_buffer(
    buffer: &AudioBuffer,
    owner: &[Option<u32>],
    speaker: u32,
    hop: usize,
) -> AudioBuffer {
    let frames = buffer.frames();
    let channels = buffer.channel_count();
    let mut planes: Vec<Vec<f32>> = vec![Vec::new(); channels];
    for (h, &who) in owner.iter().enumerate() {
        if who != Some(speaker) {
            continue;
        }
        let start = h * hop;
        let end = (start + hop).min(frames);
        for (c, plane) in planes.iter_mut().enumerate() {
            plane.extend_from_slice(&buffer.channel(c)[start..end]);
        }
    }
    AudioBuffer::from_planar(planes, buffer.sample_rate())
}

// `pub(crate)` so the chain's own tests can reuse the quiet-guest fixture and the median metric
// the 06 §2 gate is written against — one fixture, measured one way, in both places.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use anvil_asr::{Speaker, SpeakerSegment};
    use std::f32::consts::TAU;

    /// A tone at `amp` for `secs`, appended to `out`, phase-continuous with what came before.
    fn tone(out: &mut Vec<f32>, amp: f32, secs: f32, freq: f32, sr: f32) {
        let start = out.len();
        let n = (secs * sr) as usize;
        for i in 0..n {
            out.push(amp * (((start + i) as f32) * freq * TAU / sr).sin());
        }
    }

    /// **The quiet-guest fixture (06 §1 corpus class 6, the gate in 06 §2).**
    ///
    /// Two speakers alternating 4 s turns, a **12 LU** median gap between them, 0.5 s of nobody
    /// talking between turns. The voice is a 220 Hz tone under a *syllabic* envelope — 0.25 s on,
    /// 0.15 s off, raised-cosine edges — which is not decoration: a continuous steady tone reads
    /// as **music** to the analysis segmenter (03 §1: "music is near-continuously active with
    /// steadier energy; speech has syllabic gaps"), and a music-majority file correctly turns the
    /// per-speaker stage off. Syllabic gaps are what make this a *speech* fixture at all.
    ///
    /// A constant −70 dBFS noise floor gives the gate something honest to sit above.
    pub(crate) fn quiet_guest_fixture() -> (AudioBuffer, Diarization) {
        let sr = 48_000.0f32;
        let host_amp = 0.25f32;
        let guest_amp = host_amp * 10f32.powf(-12.0 / 20.0); // 12 dB quieter

        let turn = 4.0f32;
        let pause = 0.5f32;
        let syl_on = 0.25f32;
        let syl_off = 0.15f32;
        let ramp = 0.01f32; // 10 ms raised-cosine syllable edges — no clicks in the *fixture*

        // The bare voice, phase-continuous across the whole file, then windowed.
        let total = (6.0 * (turn + pause) * sr) as usize;
        let mut s: Vec<f32> = Vec::with_capacity(total);
        tone(&mut s, 1.0, 6.0 * (turn + pause), 220.0, sr);

        let mut seed = 0x5EED_1234u32;
        let mut segments = Vec::new();
        for (i, sample) in s.iter_mut().enumerate() {
            let t = i as f32 / sr;
            let cycle = t % (turn + pause);
            let turn_idx = (t / (turn + pause)) as usize;
            let amp = if turn_idx.is_multiple_of(2) {
                host_amp
            } else {
                guest_amp
            };

            // Syllabic envelope inside the turn; silence inside the pause.
            let env = if cycle >= turn {
                0.0
            } else {
                let syl = cycle % (syl_on + syl_off);
                if syl >= syl_on {
                    0.0
                } else if syl < ramp {
                    0.5 - 0.5 * (std::f32::consts::PI * syl / ramp).cos()
                } else if syl > syl_on - ramp {
                    0.5 - 0.5 * (std::f32::consts::PI * (syl_on - syl) / ramp).cos()
                } else {
                    1.0
                }
            };

            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            *sample = *sample * amp * env + 0.0006 * noise; // ~−70 dBFS floor
        }

        for k in 0..6usize {
            let start = k as f64 * (turn + pause) as f64;
            segments.push(SpeakerSegment {
                speaker: if k.is_multiple_of(2) { 0 } else { 1 },
                start,
                end: start + turn as f64,
            });
        }

        let diar = Diarization {
            speakers: vec![
                Speaker {
                    id: 0,
                    label: "Host".into(),
                },
                Speaker {
                    id: 1,
                    label: "Guest".into(),
                },
            ],
            segments,
        };
        (AudioBuffer::from_planar(vec![s], 48_000), diar)
    }

    /// The per-speaker median of a buffer, measured **the way the stage measures it** (same
    /// double gate, same short-term window, same median) — the metric the 06 §2 gate is written
    /// against. One definition, used to decide and to score, so the gate cannot be gamed by
    /// measuring differently from how we act.
    pub(crate) fn speaker_medians(buf: &AudioBuffer, diar: &Diarization, gate: f32) -> Vec<f32> {
        let hop = HOP_SAMPLES;
        let n_hops = buf.frames().div_ceil(hop);
        let owner = hop_owners(&diar.segments, n_hops, hop, buf.sample_rate() as f32);
        let ms = hop_mean_squares(buf, hop);
        diar.speakers
            .iter()
            .map(|sp| median_st_lufs(&gated_hops(&ms, &owner, sp.id, gate)).unwrap_or(-120.0))
            .collect()
    }

    fn config(diar: Diarization) -> SpeakerLevelingConfig {
        SpeakerLevelingConfig {
            noise_gate_lufs: -60.0,
            ..SpeakerLevelingConfig::new(diar, -18.0)
        }
    }

    #[test]
    fn the_fixture_really_has_a_twelve_lu_gap() {
        let (buf, diar) = quiet_guest_fixture();
        let m = speaker_medians(&buf, &diar, -60.0);
        let delta = (m[0] - m[1]).abs();
        assert!(
            (delta - 12.0).abs() < 0.5,
            "fixture gap should be ~12 LU, got {delta} ({m:?})"
        );
    }

    #[test]
    fn per_speaker_leveling_closes_the_gap() {
        let (buf, diar) = quiet_guest_fixture();
        let mut out = buf.clone();
        let cfg = config(diar.clone());
        let mut lev = SpeakerLeveler::new(48_000, cfg);
        lev.process(&mut out);

        let m = speaker_medians(&out, &diar, -60.0);
        let delta = (m[0] - m[1]).abs();
        assert!(
            delta <= 1.0,
            "post per-speaker leveling the medians must be within 1 LU: {m:?} (Δ {delta})"
        );
        // And both must land on the target, not merely agree with each other.
        for &v in &m {
            assert!(
                (v - (-18.0)).abs() <= 1.0,
                "speaker median {v} should sit on the −18 LUFS target"
            );
        }

        let applied = lev.applied();
        assert_eq!(applied.len(), 2);
        assert!(applied.iter().all(|a| a.source == MedianSource::Measured));
        assert!(
            applied[1].gain_db > applied[0].gain_db + 8.0,
            "the quiet guest must get much more gain than the host: {applied:?}"
        );
    }

    #[test]
    fn no_clicks_at_speaker_boundaries() {
        // A single unbroken tone, with diarization claiming two speakers own the two halves and
        // Voice Memory forcing a 12 dB gain difference between them. Every sample-to-sample step
        // in the *input* is the tone's own; anything bigger in the output is our boundary.
        let sr = 48_000.0f32;
        let mut s = Vec::new();
        tone(&mut s, 0.25, 12.0, 220.0, sr);
        let buf = AudioBuffer::from_planar(vec![s], 48_000);

        let diar = Diarization {
            speakers: vec![
                Speaker {
                    id: 0,
                    label: "A".into(),
                },
                Speaker {
                    id: 1,
                    label: "B".into(),
                },
            ],
            segments: vec![
                SpeakerSegment {
                    speaker: 0,
                    start: 0.0,
                    end: 6.0,
                },
                SpeakerSegment {
                    speaker: 1,
                    start: 6.0,
                    end: 12.0,
                },
            ],
        };

        let mut cfg = config(diar);
        // Remembered medians 12 LU apart ⇒ a 12 dB step in gain right at t = 6 s.
        cfg.memory = VoiceMemory::new(vec![
            SpeakerProfile::new("A", -18.0),
            SpeakerProfile::new("B", -30.0),
        ]);
        cfg.min_speech_secs = 1e9; // force the remembered medians to be used
        let mut out = buf.clone();
        let mut lev = SpeakerLeveler::new(48_000, cfg);
        lev.process(&mut out);

        let gains: Vec<f32> = lev.applied().iter().map(|a| a.gain_db).collect();
        assert!(
            (gains[1] - gains[0] - 12.0).abs() < 0.01,
            "the forced gain gap should be 12 dB, got {gains:?}"
        );
        let max_lin = 10f32.powf(gains.iter().cloned().fold(0.0, f32::max) / 20.0);

        let step = |x: &[f32]| -> f32 {
            x.windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .fold(0.0, f32::max)
        };
        let in_step = step(buf.channel(0));
        let out_step = step(out.channel(0));
        assert!(
            out_step <= in_step * max_lin * 1.02,
            "a boundary click: max output step {out_step} exceeds the tone's own step scaled by \
             the largest gain ({}) — the crossfade is not doing its job",
            in_step * max_lin
        );
    }

    #[test]
    fn no_diarization_is_a_noop() {
        let (buf, _) = quiet_guest_fixture();
        let mut out = buf.clone();
        let cfg = config(Diarization::default());
        assert!(!cfg.engaged());
        SpeakerLeveler::new(48_000, cfg).process(&mut out);
        assert_eq!(out, buf, "no speakers ⇒ the stage must not touch the audio");
    }

    #[test]
    fn deterministic() {
        let (buf, diar) = quiet_guest_fixture();
        let (mut a, mut b) = (buf.clone(), buf.clone());
        SpeakerLeveler::new(48_000, config(diar.clone())).process(&mut a);
        SpeakerLeveler::new(48_000, config(diar)).process(&mut b);
        assert_eq!(a, b, "double render must be bit-identical");
    }

    #[test]
    fn an_unmeasurable_speaker_falls_back_to_memory_then_to_unity() {
        let (buf, diar) = quiet_guest_fixture();

        // Nobody clears `min_speech_secs`, so both speakers are unmeasurable this episode.
        let mut cfg = config(diar.clone());
        cfg.min_speech_secs = 1e9;
        cfg.memory = VoiceMemory::new(vec![SpeakerProfile::new("Guest", -30.0)]);
        let mut out = buf.clone();
        let mut lev = SpeakerLeveler::new(48_000, cfg);
        lev.process(&mut out);

        let applied = lev.applied();
        assert_eq!(
            applied[0].source,
            MedianSource::Unknown,
            "no memory for Host"
        );
        assert_eq!(applied[0].gain_db, 0.0, "unknown ⇒ leave them alone");
        assert_eq!(applied[1].source, MedianSource::Remembered);
        assert!(
            (applied[1].gain_db - 12.0).abs() < 0.01,
            "−30 → −18 = +12 dB"
        );
    }

    #[test]
    fn a_profile_trim_rides_on_top_of_the_computed_gain() {
        let (buf, diar) = quiet_guest_fixture();
        let mut cfg = config(diar.clone());
        cfg.memory = VoiceMemory::new(vec![SpeakerProfile {
            speaker_label: "Host".into(),
            median_lufs: -18.0,
            gain_offset_db: 2.0,
            eq_bands: Vec::new(),
        }]);
        let mut out = buf.clone();
        let mut lev = SpeakerLeveler::new(48_000, cfg);
        lev.process(&mut out);

        let host = &lev.applied()[0];
        assert_eq!(
            host.source,
            MedianSource::Measured,
            "measured beats remembered"
        );
        // Host measures ≈ −15.7 LUFS ⇒ base gain ≈ −2.3 dB, plus the +2 dB trim.
        let base = -18.0 - host.median_lufs;
        assert!(
            (host.gain_db - (base + 2.0)).abs() < 0.01,
            "trim must ride on top: {host:?}"
        );
    }

    #[test]
    fn gain_is_bounded_by_max_gain_db() {
        let (buf, diar) = quiet_guest_fixture();
        let mut cfg = config(diar);
        cfg.max_gain_db = 3.0; // narrower than half the 12 LU spread: clamping is unavoidable
        let mut out = buf.clone();
        let mut lev = SpeakerLeveler::new(48_000, cfg);
        lev.process(&mut out);
        for a in lev.applied() {
            assert!(
                a.gain_db.abs() <= 3.0 + 1e-6,
                "gain {} unbounded",
                a.gain_db
            );
        }
        // With an unfittable spread the damage is *shared* (±3 dB, symmetric), not dumped on one
        // speaker (0 and −6, say).
        let gains: Vec<f32> = lev.applied().iter().map(|a| a.gain_db).collect();
        assert!(
            (gains[0] + 3.0).abs() < 1e-3 && (gains[1] - 3.0).abs() < 1e-3,
            "{gains:?}"
        );
    }

    /// When the cast cannot all fit inside ±`max_gain_db`, the **spread** must survive and the
    /// leftover level must be handed to the AGC — clipping the extremes independently would
    /// silently re-open the very gap this module exists to close.
    #[test]
    fn a_cast_that_does_not_fit_is_slid_as_one_so_the_spread_survives() {
        let (buf, diar) = quiet_guest_fixture();
        let mut cfg = config(diar);
        // Target far above what a ±12 dB gain can reach for the quiet guest (who needs ~+22 dB).
        cfg.target_lufs = -6.0;
        let mut out = buf.clone();
        let mut lev = SpeakerLeveler::new(48_000, cfg);
        lev.process(&mut out);

        let g: Vec<f32> = lev.applied().iter().map(|a| a.gain_db).collect();
        let m: Vec<f32> = lev.applied().iter().map(|a| a.median_lufs).collect();
        let wanted = (m[0] - m[1]).abs(); // the gap that must be closed
        assert!(
            ((g[1] - g[0]) - wanted).abs() < 0.01,
            "the spread must be closed exactly even when the target is out of reach: \
             gains {g:?} vs gap {wanted}"
        );
        assert!(
            g.iter().all(|v| v.abs() <= 12.0 + 1e-6),
            "still bounded: {g:?}"
        );
        assert!(
            lev.common_offset_db() < -1.0,
            "the cast should have been slid down as one: {}",
            lev.common_offset_db()
        );
        // And the medians really do agree afterwards.
        let after = speaker_medians(&out, &lev.config().diarization, -60.0);
        assert!((after[0] - after[1]).abs() <= 1.0, "{after:?}");
    }

    #[test]
    fn derived_profiles_round_trip_through_serde_and_reproduce_the_gains() {
        let (buf, diar) = quiet_guest_fixture();
        let memory = derive_profiles(&buf, &diar, -60.0, Some(AutoEqConfig::default()));
        assert_eq!(memory.profiles.len(), 2);
        assert_eq!(memory.get("Host").unwrap().gain_offset_db, 0.0);

        let json = serde_json::to_string(&memory).expect("serialize");
        for key in [
            "speaker_label",
            "median_lufs",
            "gain_offset_db",
            "eq_bands",
            "center_hz",
            "gain_db",
        ] {
            assert!(json.contains(key), "VoiceMemory JSON missing {key}");
        }
        let back: VoiceMemory = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, memory);

        // The remembered medians must be the ones we would measure again on the same audio.
        let measured = speaker_medians(&buf, &diar, -60.0);
        for (p, m) in back.profiles.iter().zip(&measured) {
            assert!(
                (p.median_lufs - m).abs() < 1e-4,
                "profile {} remembers {} but the render measures {m}",
                p.speaker_label,
                p.median_lufs
            );
        }
    }

    #[test]
    fn a_remembered_eq_curve_actually_filters_that_speaker() {
        let (buf, diar) = quiet_guest_fixture();
        let mut cfg = config(diar);
        cfg.memory = VoiceMemory::new(vec![SpeakerProfile {
            speaker_label: "Guest".into(),
            median_lufs: -27.7,
            gain_offset_db: 0.0,
            // A deep cut right on the fixture's 220 Hz tone: unmistakable if it lands.
            eq_bands: vec![BandFit {
                center_hz: 250.0,
                gain_db: -6.0,
                q: 1.0,
            }],
        }]);
        let mut out = buf.clone();
        let mut lev = SpeakerLeveler::new(48_000, cfg);
        lev.process(&mut out);
        assert_eq!(lev.applied()[1].eq_bands, 1);

        // The guest's turns must be the ones that changed shape. (Compare energy in the middle
        // of a guest turn against the same span with the EQ removed but the gain kept.)
        let mut gain_only = buf.clone();
        let mut cfg2 = config(lev.config().diarization.clone());
        cfg2.noise_gate_lufs = -60.0;
        SpeakerLeveler::new(48_000, cfg2).process(&mut gain_only);

        // Guest turn 2 runs 4.5 s → 8.5 s; sample the middle second of it.
        let span = 5 * 48_000..6 * 48_000;
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        let with_eq = rms(&out.channel(0)[span.clone()]);
        let without = rms(&gain_only.channel(0)[span]);
        assert!(
            with_eq < without * 0.8,
            "a −6 dB bell on the tone should audibly cut the guest: {with_eq} vs {without}"
        );
    }

    #[test]
    fn silence_is_a_safe_noop() {
        let mut buf = AudioBuffer::from_planar(vec![vec![0.0; 48_000]], 48_000);
        let diar = Diarization {
            speakers: vec![Speaker {
                id: 0,
                label: "A".into(),
            }],
            segments: vec![SpeakerSegment {
                speaker: 0,
                start: 0.0,
                end: 1.0,
            }],
        };
        let mut lev = SpeakerLeveler::new(48_000, config(diar));
        lev.process(&mut buf);
        assert!(buf.channel(0).iter().all(|s| s.is_finite()));
        assert_eq!(lev.applied()[0].source, MedianSource::Unknown);
    }

    #[test]
    fn a_moving_average_over_a_step_is_a_centred_ramp() {
        // The crossfade primitive itself: 5-hop boxcar over a 0 → 12 step.
        let target: Vec<f32> = (0..10).map(|i| if i < 5 { 0.0 } else { 12.0 }).collect();
        let out = smoothed_envelope(&target, 5);
        assert_eq!(out[0], 0.0, "well before the step: untouched");
        assert_eq!(out[9], 12.0, "well after the step: untouched");
        assert!((out[4] - 4.8).abs() < 1e-4, "ramping in: {out:?}");
        assert!((out[5] - 7.2).abs() < 1e-4, "ramping out: {out:?}");
        for w in out.windows(2) {
            assert!(
                w[1] - w[0] <= 12.0 / 5.0 + 1e-4,
                "no hop-to-hop jump bigger than the ramp slope: {out:?}"
            );
        }
    }
}
