//! Diagnostics export (04 §S8 "About… diagnostics export", 05 §M5.F): a zip of logs +
//! basic system info the user can attach to a GitHub issue. ANVIL is a privacy-brand
//! product (02 §Privacy) — this zip **never** contains audio, transcripts, project
//! content, or any file the user opened. It contains exactly two kinds of thing:
//! `system.json` (the [`SystemInfo`] fields below, nothing else) and the app's own
//! tracing log files (plain text, written by `lib.rs::init_logging`; capped in count and
//! size so an old install doesn't produce a multi-hundred-MB zip). If a fact isn't a named
//! field on [`SystemInfo`], it does not ship — no raw env dump, no file paths the user
//! opened, no preset contents.

use std::io::Write;
use std::path::PathBuf;

use anvil_core::platform::Platform;
use serde::Serialize;

/// Everything `export_diagnostics` puts in `system.json`. Deliberately a short, named
/// list — not a dump of `std::env::vars()` (could carry a Windows username in a path) and
/// not a dump of the log directory's contents beyond what's copied in separately.
#[derive(Debug, Serialize)]
struct SystemInfo {
    app_version: &'static str,
    chain_version: u32,
    platform: &'static str,
    os: &'static str,
    arch: &'static str,
    cpu_count: usize,
    generated_at_unix: u64,
}

fn collect_system_info() -> SystemInfo {
    SystemInfo {
        app_version: env!("CARGO_PKG_VERSION"),
        chain_version: anvil_core::CHAIN_VERSION,
        platform: anvil_core::platform::current().name(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        cpu_count: std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(0),
        generated_at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    }
}

/// Log files under the platform log dir, most-recently-modified first, capped to
/// [`MAX_LOG_FILES`] files and [`MAX_LOG_BYTES`] total (per-file, tail-truncated) so the
/// zip stays small and honestly "logs", not "your whole install".
const MAX_LOG_FILES: usize = 5;
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Where `lib.rs::init_logging` points `tracing-appender`'s daily rolling file writer.
/// Kept in one place so the writer and the diagnostics reader can't drift apart.
pub fn log_dir() -> PathBuf {
    anvil_core::platform::current().config_dir().join("logs")
}

fn collect_log_files() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(log_dir()) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), modified))
        })
        .collect();
    files.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    files.truncate(MAX_LOG_FILES);
    files.into_iter().map(|(p, _)| p).collect()
}

/// Result surfaced to the Settings screen: where the zip landed and roughly what's in it,
/// so the UI can say "2 log files, no audio" instead of the export being a black box.
#[derive(Debug, Serialize)]
pub struct DiagnosticsResult {
    pub zip_path: String,
    pub log_file_count: usize,
}

/// Write a diagnostics zip to `target_path`: `system.json` plus up to [`MAX_LOG_FILES`]
/// most-recent log files. See the module doc for exactly what is and isn't included.
#[tauri::command]
pub fn export_diagnostics(target_path: String) -> Result<DiagnosticsResult, String> {
    let target = PathBuf::from(&target_path);
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }

    let file = std::fs::File::create(&target).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let info_json = serde_json::to_vec_pretty(&collect_system_info()).map_err(|e| e.to_string())?;
    zip.start_file("system.json", options)
        .map_err(|e| e.to_string())?;
    zip.write_all(&info_json).map_err(|e| e.to_string())?;

    let mut budget = MAX_LOG_BYTES;
    let mut included = 0usize;
    for path in collect_log_files() {
        if budget == 0 {
            break;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let take = bytes.len().min(budget as usize);
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("log.txt")
            .to_string();
        zip.start_file(format!("logs/{name}"), options)
            .map_err(|e| e.to_string())?;
        // Tail-truncate: the most recent lines are the ones worth debugging a crash from.
        zip.write_all(&bytes[bytes.len() - take..])
            .map_err(|e| e.to_string())?;
        budget -= take as u64;
        included += 1;
    }

    zip.finish().map_err(|e| e.to_string())?;

    Ok(DiagnosticsResult {
        zip_path: target.to_string_lossy().into_owned(),
        log_file_count: included,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Structural guard on the privacy promise: `SystemInfo`'s serialized keys are exactly
    /// this short allow-list. If a future edit adds a free-form field (a raw env dump, a
    /// file path, …) this fails loudly instead of quietly widening what ships in a bug
    /// report.
    #[test]
    fn system_info_has_no_free_form_fields() {
        let json = serde_json::to_value(collect_system_info()).unwrap();
        let obj = json.as_object().unwrap();
        let allowed = [
            "app_version",
            "chain_version",
            "platform",
            "os",
            "arch",
            "cpu_count",
            "generated_at_unix",
        ];
        for key in obj.keys() {
            assert!(
                allowed.contains(&key.as_str()),
                "unexpected diagnostics field: {key}"
            );
        }
    }

    #[test]
    fn export_diagnostics_writes_a_zip_with_system_json_and_never_audio() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("diag.zip");
        let result = export_diagnostics(target.to_string_lossy().into_owned()).unwrap();
        assert!(std::path::Path::new(&result.zip_path).exists());

        let file = std::fs::File::open(&result.zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut saw_system_json = false;
        for i in 0..archive.len() {
            let entry = archive.by_index(i).unwrap();
            let name = entry.name().to_string();
            if name == "system.json" {
                saw_system_json = true;
            }
            for banned in [
                ".wav",
                ".mp3",
                ".flac",
                ".m4a",
                ".mp4",
                ".mov",
                ".anvilproj",
            ] {
                assert!(
                    !name.ends_with(banned),
                    "diagnostics zip must never contain audio/video/project files: {name}"
                );
            }
        }
        assert!(saw_system_json);
    }
}
