//! Back-catalog mode (04 §S4): expand a folder into [`BatchJob`]s, optionally mirroring
//! its subdirectory structure into the output directory ("preserve folder structure").
//!
//! This module only decides *what* to process and *where the output goes*; the actual
//! rendering/concurrency/isolation lives in [`crate::queue`].

use std::path::{Path, PathBuf};

use anvil_project::{Preset, Tier};

use crate::error::BatchError;
use crate::queue::BatchJob;

/// The container extensions batch/watch will pick up (mirrors S1's drop zone:
/// "wav/mp3/m4a/flac/mp4" plus a few common siblings). Case-insensitive.
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "wav", "wave", "mp3", "m4a", "m4b", "flac", "ogg", "oga", "aac", "wma", "aiff", "aif", "mp4",
    "mov", "mkv",
];

/// Whether `path`'s extension is one batch/watch knows how to pick up.
pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            SUPPORTED_EXTENSIONS
                .iter()
                .any(|s| s.eq_ignore_ascii_case(e))
        })
        .unwrap_or(false)
}

/// Where batch/back-catalog/watch output lands and how it's named.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputSettings {
    /// Root output directory. Always created if missing.
    pub output_dir: PathBuf,
    /// Mirror each input's subdirectory (relative to the scanned root) under
    /// `output_dir` instead of flattening every output into one folder (04 §S4
    /// "preserve folder structure"). Only meaningful for [`folder_targets`]; flat
    /// submissions via [`flat_targets`] have no common root to preserve, and always
    /// flatten into `output_dir`.
    pub preserve_structure: bool,
    /// Output filename template; `{name}` is replaced with the input's file stem
    /// (08 §S8 "output naming tokens" — only `{name}` is implemented so far).
    ///
    /// The extension is fixed to `.wav` here — the interim encoder seam
    /// (`queue::render_job`) only ever writes 16-bit PCM WAV. Swap in format-aware
    /// naming once `anvil_media::encode` lands and outputs can target the full format
    /// matrix.
    pub naming: String,
}

impl OutputSettings {
    /// Output settings with the 04 §S8 default naming pattern, flattened (no structure
    /// preservation).
    pub fn new(output_dir: impl Into<PathBuf>) -> Self {
        Self {
            output_dir: output_dir.into(),
            preserve_structure: false,
            naming: "{name}_mastered".into(),
        }
    }

    /// Same settings but mirroring the scanned root's subdirectories under `output_dir`.
    pub fn preserving_structure(mut self) -> Self {
        self.preserve_structure = true;
        self
    }

    fn file_name_for(&self, input: &Path) -> String {
        let stem = input
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        format!("{}.wav", self.naming.replace("{name}", stem))
    }
}

/// Recursively list every supported file under `root`, sorted for deterministic
/// ordering. Fails with [`BatchError::FolderUnreachable`] if `root` can't be read.
pub fn scan_folder(root: &Path) -> Result<Vec<PathBuf>, BatchError> {
    let mut out = Vec::new();
    scan_into(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn scan_into(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), BatchError> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            BatchError::FolderUnreachable(dir.to_path_buf())
        } else {
            BatchError::Io(e)
        }
    })?;
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            scan_into(&path, out)?;
        } else if is_supported(&path) {
            out.push(path);
        }
    }
    Ok(())
}

/// Resolve one input's output path under `output`, mirroring its subdirectory (relative
/// to `root`) when [`OutputSettings::preserve_structure`] is set.
pub fn resolve_output(input: &Path, root: &Path, output: &OutputSettings) -> PathBuf {
    let file_name = output.file_name_for(input);
    if output.preserve_structure {
        let rel_dir = input
            .parent()
            .and_then(|p| p.strip_prefix(root).ok())
            .unwrap_or_else(|| Path::new(""));
        output.output_dir.join(rel_dir).join(file_name)
    } else {
        output.output_dir.join(file_name)
    }
}

/// Build [`BatchJob`]s for a flat file list — no common root, so every output flattens
/// into `output.output_dir` regardless of [`OutputSettings::preserve_structure`].
pub fn flat_targets(
    inputs: Vec<PathBuf>,
    preset: &Preset,
    tier: Tier,
    output: &OutputSettings,
) -> Vec<BatchJob> {
    inputs
        .into_iter()
        .map(|input| {
            let out_path = output.output_dir.join(output.file_name_for(&input));
            BatchJob {
                input,
                output: out_path,
                preset: preset.clone(),
                tier,
            }
        })
        .collect()
}

/// Recurse `root` and build [`BatchJob`]s for every matching file (back-catalog mode, 04
/// §S4), honoring [`OutputSettings::preserve_structure`].
pub fn folder_targets(
    root: &Path,
    preset: &Preset,
    tier: Tier,
    output: &OutputSettings,
) -> Result<Vec<BatchJob>, BatchError> {
    let files = scan_folder(root)?;
    Ok(files
        .into_iter()
        .map(|input| {
            let out_path = resolve_output(&input, root, output);
            BatchJob {
                input,
                output: out_path,
                preset: preset.clone(),
                tier,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_common_extensions_case_insensitively() {
        assert!(is_supported(Path::new("ep1.WAV")));
        assert!(is_supported(Path::new("ep1.mp3")));
        assert!(!is_supported(Path::new("ep1.txt")));
        assert!(!is_supported(Path::new("no_extension")));
    }

    #[test]
    fn flat_output_ignores_preserve_structure() {
        let output = OutputSettings::new("/out").preserving_structure();
        let jobs = flat_targets(
            vec![PathBuf::from("/a/b/ep1.wav")],
            &Preset::default(),
            Tier::Standard,
            &output,
        );
        assert_eq!(jobs[0].output, PathBuf::from("/out/ep1_mastered.wav"));
    }

    #[test]
    fn resolve_output_flattens_by_default() {
        let output = OutputSettings::new("/out");
        let path = resolve_output(
            Path::new("/root/season1/ep1.wav"),
            Path::new("/root"),
            &output,
        );
        assert_eq!(path, PathBuf::from("/out/ep1_mastered.wav"));
    }

    #[test]
    fn resolve_output_preserves_structure_when_requested() {
        let output = OutputSettings::new("/out").preserving_structure();
        let path = resolve_output(
            Path::new("/root/season1/ep1.wav"),
            Path::new("/root"),
            &output,
        );
        assert_eq!(path, PathBuf::from("/out/season1/ep1_mastered.wav"));
    }

    #[test]
    fn scan_folder_recurses_and_filters_by_extension() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("season1")).unwrap();
        std::fs::write(tmp.path().join("ep1.wav"), b"x").unwrap();
        std::fs::write(tmp.path().join("notes.txt"), b"x").unwrap();
        std::fs::write(tmp.path().join("season1/ep2.mp3"), b"x").unwrap();

        let files = scan_folder(tmp.path()).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|p| p.ends_with("ep1.wav")));
        assert!(files
            .iter()
            .any(|p| p.ends_with("season1/ep2.mp3") || p.ends_with("season1\\ep2.mp3")));
    }

    #[test]
    fn scan_folder_reports_unreachable_root() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let err = scan_folder(&missing).unwrap_err();
        assert!(matches!(err, BatchError::FolderUnreachable(_)));
    }
}
