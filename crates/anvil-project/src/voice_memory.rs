//! Voice Memory: per-show speaker profiles (04 §S2 AutoEQ note, 03 §4.7/§4.8) so a returning
//! host or guest gets the same EQ/leveling treatment episode after episode instead of the
//! chain re-learning them from scratch every time.
//!
//! [`VoiceMemory`] is a per-show store of [`SpeakerProfile`]s, persisted as one JSON file per
//! show in the platform config dir (mirrors [`crate::Settings`]'s load/save shape). The DSP
//! lane reads [`SpeakerProfile`] to seed AutoEQ (03 §4.7 "per-speaker when diarization is
//! available") and the adaptive leveler's per-speaker mode (03 §4.8) — so
//! [`SpeakerProfile`]/[`EqBand`]'s field names and types are a contract, not just an internal
//! detail. [`EqBand`] mirrors `anvil_dsp::autoeq::BandFit` structurally (center/gain/Q) but is
//! kept as a plain serde type here rather than reused from `anvil-dsp`, so this crate doesn't
//! pick up a DSP dependency just to remember a few numbers per speaker.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use anvil_core::platform::Platform as _;
use anvil_core::{Error, Result};

use crate::PROJECT_SCHEMA_VERSION;

/// One parametric EQ band from a fitted AutoEQ curve (03 §4.7: "fit \u{2264}8 biquads ...
/// bounded \u{00b1}6 dB, Q \u{2264} 2"). Structurally mirrors `anvil_dsp::autoeq::BandFit`
/// (center frequency / gain / Q) so copying a fit into a profile is a straight field-for-field
/// map on the DSP lane's side.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    /// Centre frequency, Hz.
    pub freq_hz: f32,
    /// Applied gain, dB.
    pub gain_db: f32,
    /// Bell Q.
    pub q: f32,
}

impl EqBand {
    pub fn new(freq_hz: f32, gain_db: f32, q: f32) -> Self {
        Self {
            freq_hz,
            gain_db,
            q,
        }
    }
}

/// A returning speaker's learned treatment for one show: their typical loudness, the gain
/// offset that brings them to the show's common target (03 §4.8 per-speaker leveler mode),
/// and their fitted AutoEQ curve (03 §4.7 per-speaker AutoEQ).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeakerProfile {
    /// Diarization-assigned speaker id/label, stable across episodes of the same show (e.g.
    /// `"host"`, `"guest_1"`). Case-sensitive, matched exactly by [`VoiceMemory`]'s
    /// upsert/merge/lookup methods.
    pub speaker_label: String,
    /// Median speech loudness across episodes seen so far, ST-LUFS (03 §4.8 slow AGC target
    /// basis).
    pub median_lufs: f32,
    /// Gain offset (dB) that brings this speaker's median loudness to the show's common
    /// target (03 §4.8: "each speaker's median speech loudness is first normalized to the
    /// common target").
    pub gain_offset_db: f32,
    /// Fitted AutoEQ bands for this speaker (03 §4.7).
    pub eq_bands: Vec<EqBand>,
}

impl SpeakerProfile {
    /// A profile from a single episode's measurement — the starting point before any
    /// [`VoiceMemory::merge_profile`] call blends in a second episode.
    pub fn new(
        speaker_label: impl Into<String>,
        median_lufs: f32,
        gain_offset_db: f32,
        eq_bands: Vec<EqBand>,
    ) -> Self {
        Self {
            speaker_label: speaker_label.into(),
            median_lufs,
            gain_offset_db,
            eq_bands,
        }
    }
}

/// Per-show speaker-profile store: one file per show in the platform config dir, reused
/// across every episode of that show so a returning host/guest is recognized and treated
/// consistently (feature name: Voice Memory).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceMemory {
    pub schema_version: u32,
    pub show_id: String,
    pub profiles: Vec<SpeakerProfile>,
    /// How many episodes have contributed to each speaker's profile so far, keyed by
    /// `speaker_label`. Internal bookkeeping for [`Self::merge_profile`]'s running average —
    /// deliberately not `pub`: the DSP lane consumes `profiles`, not this. `#[serde(default)]`
    /// so a file written before this field existed still loads.
    #[serde(default)]
    episode_counts: BTreeMap<String, u32>,
}

impl VoiceMemory {
    /// A fresh, empty store for `show_id`.
    pub fn new(show_id: impl Into<String>) -> Self {
        Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            show_id: show_id.into(),
            profiles: Vec::new(),
            episode_counts: BTreeMap::new(),
        }
    }

    /// The conventional on-disk path for `show_id`'s voice memory file in the platform config
    /// dir (`voice_memory/<show_id>.json`). Real load/save call sites use this; tests pass
    /// their own tempdir path instead so they never touch the user's real config dir (matches
    /// [`crate::Settings::default_path`]).
    pub fn default_path(show_id: &str) -> PathBuf {
        anvil_core::platform::current()
            .config_dir()
            .join("voice_memory")
            .join(format!("{}.json", sanitize_file_stem(show_id)))
    }

    /// Load `show_id`'s voice memory from `path`, migrating forward first if it was written
    /// by an older build (see [`migrate_voice_memory`]). First run (file doesn't exist yet)
    /// yields an empty [`VoiceMemory::new`] for `show_id` rather than an error, matching
    /// [`crate::Settings::load`]. A newer-than-supported schema version is a hard error rather
    /// than silently dropping fields (ADR-008 "code must support at least N-1 schema
    /// version"), matching [`crate::Project::load`].
    pub fn load(path: &Path, show_id: &str) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let raw: serde_json::Value = serde_json::from_slice(&bytes)?;
                migrate_voice_memory(raw)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new(show_id)),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist this store to `path`, creating parent directories as needed.
    /// Write-temp-then-rename for crash safety, matching [`crate::Project::save`] and
    /// [`crate::Settings::save`] (ADR-008 "crash-safe via write-temp-then-rename").
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Look up a speaker's stored profile by exact label match.
    pub fn profile(&self, speaker_label: &str) -> Option<&SpeakerProfile> {
        self.profiles
            .iter()
            .find(|p| p.speaker_label == speaker_label)
    }

    /// Insert `profile`, or replace the existing profile with the same `speaker_label`
    /// outright. This is a deliberate overwrite (e.g. a user-edited profile from the UI), not
    /// a refinement — it resets that speaker's merge weighting, so the next
    /// [`Self::merge_profile`] call blends against this new baseline rather than history from
    /// before the overwrite. Callers that want a new episode's measurement to *refine* the
    /// existing profile instead of replacing it should use [`Self::merge_profile`].
    pub fn upsert_profile(&mut self, profile: SpeakerProfile) {
        let label = profile.speaker_label.clone();
        match self.profiles.iter_mut().find(|p| p.speaker_label == label) {
            Some(existing) => *existing = profile,
            None => self.profiles.push(profile),
        }
        self.episode_counts.insert(label, 1);
    }

    /// Fold a new episode's measurement of `incoming.speaker_label` into the stored profile
    /// instead of clobbering it: `median_lufs`, `gain_offset_db`, and each EQ band are updated
    /// as a running average across every episode this speaker has been seen in (03 §4.7 "Voice
    /// Memory feature stores these curves per show" implies they accumulate, not reset each
    /// episode). If the label hasn't been seen before, this behaves like
    /// [`Self::upsert_profile`] — there's nothing to average against yet.
    ///
    /// EQ bands are matched by position: the same `AutoEqConfig` (target curve, band count)
    /// fits bands in a stable order episode to episode, so index-matching is equivalent to
    /// matching by band identity without needing a fuzzy frequency match. If one profile has
    /// more bands than the other (e.g. after a chain-version change), the extra bands are kept
    /// as-is rather than dropped.
    pub fn merge_profile(&mut self, incoming: SpeakerProfile) {
        let label = incoming.speaker_label.clone();
        let episodes_so_far = self.episode_counts.get(&label).copied().unwrap_or(0);

        match self.profiles.iter_mut().find(|p| p.speaker_label == label) {
            None => {
                self.profiles.push(incoming);
                self.episode_counts.insert(label, 1);
            }
            Some(existing) => {
                let new_count = episodes_so_far.max(1) + 1;
                let new_n = new_count as f32;

                existing.median_lufs =
                    running_average(existing.median_lufs, incoming.median_lufs, new_n);
                existing.gain_offset_db =
                    running_average(existing.gain_offset_db, incoming.gain_offset_db, new_n);
                existing.eq_bands = merge_eq_bands(&existing.eq_bands, &incoming.eq_bands, new_n);

                self.episode_counts.insert(label, new_count);
            }
        }
    }
}

/// Incremental mean update: `old` is the mean of `new_n - 1` prior samples, `new` is the
/// `new_n`-th sample. Standard running-average formula (`mean += (x - mean) / n`), used so
/// [`VoiceMemory::merge_profile`] doesn't need to keep the full measurement history around,
/// just the running count.
fn running_average(old: f32, new: f32, new_n: f32) -> f32 {
    old + (new - old) / new_n
}

/// Blend two speakers' EQ band lists position-by-position via [`running_average`]. See
/// [`VoiceMemory::merge_profile`]'s doc comment for why position-matching is reasonable here.
fn merge_eq_bands(existing: &[EqBand], incoming: &[EqBand], new_n: f32) -> Vec<EqBand> {
    let len = existing.len().max(incoming.len());
    let mut merged = Vec::with_capacity(len);
    for i in 0..len {
        merged.push(match (existing.get(i), incoming.get(i)) {
            (Some(e), Some(n)) => EqBand {
                freq_hz: running_average(e.freq_hz, n.freq_hz, new_n),
                gain_db: running_average(e.gain_db, n.gain_db, new_n),
                q: running_average(e.q, n.q, new_n),
            },
            (Some(e), None) => *e,
            (None, Some(n)) => *n,
            (None, None) => unreachable!("i < len implies at least one side has an entry"),
        });
    }
    merged
}

/// Replace characters that are invalid (or awkward) in a Windows/macOS file name with `_`, so
/// a `show_id` containing e.g. `/` can't escape the `voice_memory/` directory or fail to
/// create.
fn sanitize_file_stem(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// Migration hook keyed off `schema_version`, mirroring [`crate::project::migrate_manifest`].
/// No migrations exist yet — v1 is the only version — so this is currently a passthrough for
/// `version <= PROJECT_SCHEMA_VERSION`.
fn migrate_voice_memory(raw: serde_json::Value) -> Result<VoiceMemory> {
    let found = raw
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;

    if found > PROJECT_SCHEMA_VERSION {
        return Err(Error::UnsupportedSchemaVersion {
            found,
            supported: PROJECT_SCHEMA_VERSION,
        });
    }

    // Migration stub: future schema bumps add `if found < N { ... }` steps here, each
    // rewriting `raw` forward one version, before falling through to the deserialize.
    let migrated = raw;

    Ok(serde_json::from_value(migrated)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(freq_hz: f32, gain_db: f32, q: f32) -> EqBand {
        EqBand::new(freq_hz, gain_db, q)
    }

    #[test]
    fn new_store_is_empty() {
        let vm = VoiceMemory::new("the-daily-show");
        assert_eq!(vm.show_id, "the-daily-show");
        assert!(vm.profiles.is_empty());
        assert_eq!(vm.schema_version, PROJECT_SCHEMA_VERSION);
    }

    #[test]
    fn load_missing_file_yields_empty_store_for_show_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("show.json");
        let vm = VoiceMemory::load(&path, "show-a").unwrap();
        assert_eq!(vm, VoiceMemory::new("show-a"));
    }

    #[test]
    fn save_then_load_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("show.json");

        let mut vm = VoiceMemory::new("show-a");
        vm.upsert_profile(SpeakerProfile::new(
            "host",
            -18.5,
            2.1,
            vec![band(120.0, -1.5, 1.0), band(3000.0, 2.0, 1.4)],
        ));

        vm.save(&path).unwrap();
        let loaded = VoiceMemory::load(&path, "show-a").unwrap();

        assert_eq!(loaded, vm);
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("show.json");
        VoiceMemory::new("show-a").save(&path).unwrap();
        assert!(path.exists());
        assert!(!tmp.path().join("show.json.tmp").exists());
    }

    #[test]
    fn load_rejects_newer_schema_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("show.json");

        let mut raw = serde_json::to_value(VoiceMemory::new("show-a")).unwrap();
        raw["schema_version"] = serde_json::json!(PROJECT_SCHEMA_VERSION + 1);
        std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();

        let err = VoiceMemory::load(&path, "show-a").unwrap_err();
        assert!(matches!(err, Error::UnsupportedSchemaVersion { .. }));
    }

    #[test]
    fn default_path_is_namespaced_and_per_show() {
        let path = VoiceMemory::default_path("the-daily-show");
        assert!(path.ends_with("the-daily-show.json"));
        assert!(path.parent().unwrap().ends_with("voice_memory"));
    }

    #[test]
    fn sanitize_file_stem_strips_path_separators() {
        assert_eq!(sanitize_file_stem("a/b\\c:d"), "a_b_c_d");
    }

    #[test]
    fn upsert_inserts_new_profile() {
        let mut vm = VoiceMemory::new("show-a");
        vm.upsert_profile(SpeakerProfile::new("host", -18.0, 2.0, vec![]));
        assert_eq!(vm.profiles.len(), 1);
        assert_eq!(vm.profile("host").unwrap().median_lufs, -18.0);
    }

    #[test]
    fn upsert_replaces_existing_profile_outright() {
        let mut vm = VoiceMemory::new("show-a");
        vm.upsert_profile(SpeakerProfile::new(
            "host",
            -18.0,
            2.0,
            vec![band(100.0, 1.0, 1.0)],
        ));
        vm.upsert_profile(SpeakerProfile::new("host", -12.0, -1.0, vec![]));

        assert_eq!(vm.profiles.len(), 1);
        let p = vm.profile("host").unwrap();
        assert_eq!(p.median_lufs, -12.0, "upsert must overwrite, not blend");
        assert_eq!(p.gain_offset_db, -1.0);
        assert!(p.eq_bands.is_empty());
    }

    #[test]
    fn upsert_does_not_clobber_other_speakers() {
        let mut vm = VoiceMemory::new("show-a");
        vm.upsert_profile(SpeakerProfile::new("host", -18.0, 0.0, vec![]));
        vm.upsert_profile(SpeakerProfile::new("guest_1", -22.0, 4.0, vec![]));
        assert_eq!(vm.profiles.len(), 2);
        assert_eq!(vm.profile("host").unwrap().median_lufs, -18.0);
        assert_eq!(vm.profile("guest_1").unwrap().median_lufs, -22.0);
    }

    #[test]
    fn merge_on_new_speaker_behaves_like_insert() {
        let mut vm = VoiceMemory::new("show-a");
        vm.merge_profile(SpeakerProfile::new(
            "host",
            -18.0,
            2.0,
            vec![band(100.0, 1.0, 1.0)],
        ));
        assert_eq!(vm.profiles.len(), 1);
        assert_eq!(vm.profile("host").unwrap().median_lufs, -18.0);
    }

    #[test]
    fn merge_running_averages_median_lufs_and_gain_offset() {
        let mut vm = VoiceMemory::new("show-a");
        vm.merge_profile(SpeakerProfile::new("host", -20.0, 0.0, vec![]));
        // Second episode measures a different level: expect the simple average of two
        // samples (-20 and -16 => -18).
        vm.merge_profile(SpeakerProfile::new("host", -16.0, 4.0, vec![]));

        let p = vm.profile("host").unwrap();
        assert!(
            (p.median_lufs - (-18.0)).abs() < 1e-4,
            "expected running average of two episodes, got {}",
            p.median_lufs
        );
        assert!((p.gain_offset_db - 2.0).abs() < 1e-4);

        // Third episode: running mean of (-20, -16, -16) => -17.333...
        vm.merge_profile(SpeakerProfile::new("host", -16.0, 4.0, vec![]));
        let p = vm.profile("host").unwrap();
        assert!(
            (p.median_lufs - (-17.333_33)).abs() < 1e-3,
            "expected 3-episode running average, got {}",
            p.median_lufs
        );
    }

    #[test]
    fn merge_never_clobbers_the_way_upsert_does() {
        let mut vm = VoiceMemory::new("show-a");
        vm.merge_profile(SpeakerProfile::new("host", -20.0, 0.0, vec![]));
        vm.merge_profile(SpeakerProfile::new("host", -10.0, 0.0, vec![]));

        let p = vm.profile("host").unwrap();
        // A clobbering merge would leave -10.0; a running average lands strictly between.
        assert!(p.median_lufs > -20.0 && p.median_lufs < -10.0);
    }

    #[test]
    fn merge_averages_eq_bands_by_position() {
        let mut vm = VoiceMemory::new("show-a");
        vm.merge_profile(SpeakerProfile::new(
            "host",
            -18.0,
            0.0,
            vec![band(100.0, 2.0, 1.0)],
        ));
        vm.merge_profile(SpeakerProfile::new(
            "host",
            -18.0,
            0.0,
            vec![band(100.0, 4.0, 1.0)],
        ));

        let bands = &vm.profile("host").unwrap().eq_bands;
        assert_eq!(bands.len(), 1);
        assert!((bands[0].gain_db - 3.0).abs() < 1e-4);
    }

    #[test]
    fn merge_keeps_extra_bands_when_counts_differ() {
        let mut vm = VoiceMemory::new("show-a");
        vm.merge_profile(SpeakerProfile::new(
            "host",
            -18.0,
            0.0,
            vec![band(100.0, 2.0, 1.0)],
        ));
        vm.merge_profile(SpeakerProfile::new(
            "host",
            -18.0,
            0.0,
            vec![band(100.0, 4.0, 1.0), band(4000.0, 1.0, 1.5)],
        ));

        let bands = &vm.profile("host").unwrap().eq_bands;
        assert_eq!(
            bands.len(),
            2,
            "the unmatched second band must be kept, not dropped"
        );
        assert!((bands[1].freq_hz - 4000.0).abs() < 1e-4);
    }

    #[test]
    fn merge_after_upsert_starts_a_fresh_average() {
        let mut vm = VoiceMemory::new("show-a");
        vm.merge_profile(SpeakerProfile::new("host", -20.0, 0.0, vec![]));
        vm.merge_profile(SpeakerProfile::new("host", -16.0, 0.0, vec![]));
        // Running mean of (-20, -16) is -18.
        assert!((vm.profile("host").unwrap().median_lufs - (-18.0)).abs() < 1e-4);

        // A manual overwrite resets the baseline...
        vm.upsert_profile(SpeakerProfile::new("host", -10.0, 0.0, vec![]));
        // ...so the next merge averages against -10, not against the pre-upsert history.
        vm.merge_profile(SpeakerProfile::new("host", -14.0, 0.0, vec![]));
        assert!((vm.profile("host").unwrap().median_lufs - (-12.0)).abs() < 1e-4);
    }
}
