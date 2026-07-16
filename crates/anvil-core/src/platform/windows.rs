//! Windows shell integration (M2 lane D): Explorer context menu, "Open with" file
//! associations, and login autostart. Everything here is scoped to `HKCU` — per-user,
//! no admin rights — and additive/reversible: nothing in this file ever touches a file
//! type's default handler (`UserChoice`) or writes outside Cleanroom's own registry keys.
//!
//! This whole file is `#[cfg(windows)]` via its `mod windows;` declaration in
//! `platform/mod.rs` — the only place ADR-006 allows that split (Windows-isms
//! elsewhere in the tree break the macOS CI compile job).
//!
//! The public `pub(super) fn`s below (the ones with no trailing `_at`) are what
//! [`super::CurrentPlatform`] calls and always target the real hives
//! (`Software\Classes`, `...\CurrentVersion\Run`). Each one is a thin wrapper around a
//! root-parameterized `_at` function; the unit tests exercise those directly against a
//! throwaway test-scoped root instead of the real hives, so `cargo test` never mutates
//! an actual install (see [`tests::TestRoot`]).

use std::io;
use std::path::{Path, PathBuf};

use winreg::enums::*;
use winreg::RegKey;

use super::{PlatformError, PlatformResult};

/// File extensions (no leading dot) Cleanroom can master, mirrored from the format list in
/// `docs/adr/005-media-io.md`. Video types are demuxed/remuxed with `-c:v copy` — Cleanroom
/// only ever touches the audio track — but still get the context menu verb and "Open
/// with" entry since that's the file the user right-clicks.
const SUPPORTED_EXTENSIONS: &[&str] = &[
    // audio
    "wav", "flac", "mp3", "m4a", "aac", "ogg", //
    // video (audio track only; container is remuxed, not re-encoded)
    "mp4", "mov", "mkv", "webm",
];

/// Context-menu verb key name. Not shown to the user — the label is the verb key's
/// default value ([`VERB_LABEL`]); this is just the registry identifier.
const VERB_KEY: &str = "Cleanroom.MasterWith";

/// Context-menu label shown in Explorer.
const VERB_LABEL: &str = "Master with Cleanroom";

/// Name Cleanroom registers itself under in `Software\Classes\Applications` for the
/// "Open with" picker. Matches the CLI binary name (ADR-007); the desktop app shares
/// the registration since both accept a file path as `argv[1]`.
const APP_KEY: &str = "cleanroom.exe";

/// Value name Cleanroom writes under the `Run` key for autostart.
const AUTOSTART_VALUE: &str = "Cleanroom";

fn exe_path() -> PlatformResult<PathBuf> {
    std::env::current_exe().map_err(PlatformError::ExePath)
}

/// Opens the real per-user `Software\Classes` root (equivalent to `HKEY_CLASSES_ROOT`
/// for this user, but doesn't require the merged view). Always exists under `HKCU`.
fn classes_root() -> PlatformResult<RegKey> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags("Software\\Classes", KEY_ALL_ACCESS)
        .map_err(PlatformError::Registry)
}

/// Opens the real per-user autostart `Run` key. Always exists under `HKCU`.
fn run_root() -> PlatformResult<RegKey> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
            KEY_ALL_ACCESS,
        )
        .map_err(PlatformError::Registry)
}

pub(super) fn register_context_menu() -> PlatformResult<()> {
    register_context_menu_at(&classes_root()?, &exe_path()?)
}

pub(super) fn unregister_context_menu() -> PlatformResult<()> {
    unregister_context_menu_at(&classes_root()?)
}

pub(super) fn register_file_associations() -> PlatformResult<()> {
    register_file_associations_at(&classes_root()?, &exe_path()?)
}

pub(super) fn unregister_file_associations() -> PlatformResult<()> {
    unregister_file_associations_at(&classes_root()?)
}

pub(super) fn set_autostart(enabled: bool) -> PlatformResult<()> {
    let run = run_root()?;
    if enabled {
        set_autostart_at(&run, &exe_path()?)
    } else {
        clear_autostart_at(&run)
    }
}

pub(super) fn is_autostart_enabled() -> PlatformResult<bool> {
    is_autostart_enabled_at(&run_root()?)
}

// --- Root-parameterized implementations. `classes_root`/`run_root` above always pass
// the real hives; tests pass a throwaway test-scoped root instead. ---

fn register_context_menu_at(classes_root: &RegKey, exe: &Path) -> PlatformResult<()> {
    let command = quoted_command(exe);
    for ext in SUPPORTED_EXTENSIONS {
        let (verb_key, _) = classes_root
            .create_subkey(format!("SystemFileAssociations\\.{ext}\\shell\\{VERB_KEY}"))?;
        verb_key.set_value("", &VERB_LABEL)?;
        let (cmd_key, _) = verb_key.create_subkey("command")?;
        cmd_key.set_value("", &command)?;
    }
    Ok(())
}

fn unregister_context_menu_at(classes_root: &RegKey) -> PlatformResult<()> {
    for ext in SUPPORTED_EXTENSIONS {
        delete_subkey_all_if_present(
            classes_root,
            &format!("SystemFileAssociations\\.{ext}\\shell\\{VERB_KEY}"),
        )?;
    }
    Ok(())
}

fn register_file_associations_at(classes_root: &RegKey, exe: &Path) -> PlatformResult<()> {
    let command = quoted_command(exe);
    let (app_key, _) =
        classes_root.create_subkey(format!("Applications\\{APP_KEY}\\shell\\open"))?;
    let (cmd_key, _) = app_key.create_subkey("command")?;
    cmd_key.set_value("", &command)?;

    // `OpenWithList` only adds Cleanroom to the "Open with" picker for each extension; it
    // never writes `UserChoice`, so the type's default handler is untouched.
    for ext in SUPPORTED_EXTENSIONS {
        classes_root.create_subkey(format!(".{ext}\\OpenWithList\\{APP_KEY}"))?;
    }
    Ok(())
}

fn unregister_file_associations_at(classes_root: &RegKey) -> PlatformResult<()> {
    delete_subkey_all_if_present(classes_root, &format!("Applications\\{APP_KEY}"))?;
    for ext in SUPPORTED_EXTENSIONS {
        // Delete only Cleanroom's own entry, never the shared `OpenWithList` parent —
        // other apps may have registered siblings under it.
        delete_subkey_if_present(classes_root, &format!(".{ext}\\OpenWithList\\{APP_KEY}"))?;
    }
    Ok(())
}

fn set_autostart_at(run_root: &RegKey, exe: &Path) -> PlatformResult<()> {
    run_root
        .set_value(AUTOSTART_VALUE, &quoted_command(exe))
        .map_err(PlatformError::Registry)
}

fn clear_autostart_at(run_root: &RegKey) -> PlatformResult<()> {
    match run_root.delete_value(AUTOSTART_VALUE) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PlatformError::Registry(e)),
    }
}

fn is_autostart_enabled_at(run_root: &RegKey) -> PlatformResult<bool> {
    match run_root.get_value::<String, _>(AUTOSTART_VALUE) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(PlatformError::Registry(e)),
    }
}

/// The command Explorer runs for the context-menu verb / "Open with" entry / autostart:
/// the current executable with the file path (or, for autostart, nothing yet — the
/// tray agent's own argv is M2 lane E's call) quoted for spaces.
fn quoted_command(exe: &Path) -> String {
    format!("\"{}\" \"%1\"", exe.display())
}

fn delete_subkey_all_if_present(root: &RegKey, path: &str) -> PlatformResult<()> {
    match root.delete_subkey_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PlatformError::Registry(e)),
    }
}

fn delete_subkey_if_present(root: &RegKey, path: &str) -> PlatformResult<()> {
    match root.delete_subkey(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PlatformError::Registry(e)),
    }
}

// --- Legacy-identity cleanup (upgrade from the pre-rename "ANVIL" builds) ------------------
// The rename changed the ProgID / Applications-key / autostart-value NAMES, so an in-place
// Cleanroom upgrade orphans an old ANVIL install's registry footprint (NSIS only removes keys
// under the NEW names). Remove the old names so upgraders don't keep dead "Open with" and
// context-menu entries. Per-user (HKCU), idempotent, best-effort.
const LEGACY_VERB_KEY: &str = "Anvil.MasterWith";
const LEGACY_APP_KEY: &str = "anvil.exe";
const LEGACY_AUTOSTART_VALUE: &str = "ANVIL";

pub(super) fn remove_legacy_identity() -> PlatformResult<()> {
    remove_legacy_identity_at(&classes_root()?, &run_root()?)
}

fn remove_legacy_identity_at(classes_root: &RegKey, run_root: &RegKey) -> PlatformResult<()> {
    for ext in SUPPORTED_EXTENSIONS {
        delete_subkey_all_if_present(
            classes_root,
            &format!("SystemFileAssociations\\.{ext}\\shell\\{LEGACY_VERB_KEY}"),
        )?;
        delete_subkey_if_present(classes_root, &format!(".{ext}\\OpenWithList\\{LEGACY_APP_KEY}"))?;
    }
    delete_subkey_all_if_present(classes_root, &format!("Applications\\{LEGACY_APP_KEY}"))?;
    match run_root.delete_value(LEGACY_AUTOSTART_VALUE) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PlatformError::Registry(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway registry root under `HKCU\Software\AnvilPlatformTests\<unique id>`,
    /// shaped like the real hives (`...\Classes`, `...\Run`) so the `_at` functions run
    /// unmodified against it. Dropping the guard deletes the whole test-scoped subtree
    /// — including on test panic/assertion failure, since `Drop` still runs during
    /// unwinding — so tests never leak keys and never touch the real
    /// `Software\Classes` / `...\Run` hives.
    struct TestRoot {
        base: String,
        classes: RegKey,
        run: RegKey,
    }

    impl TestRoot {
        fn new(test_name: &str) -> Self {
            let base = format!(
                "Software\\AnvilPlatformTests\\{test_name}-{}",
                uuid::Uuid::new_v4()
            );
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let (classes, _) = hkcu
                .create_subkey(format!("{base}\\Classes"))
                .expect("create test-scoped Classes root");
            let (run, _) = hkcu
                .create_subkey(format!("{base}\\Run"))
                .expect("create test-scoped Run root");
            Self { base, classes, run }
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let _ = hkcu.delete_subkey_all(&self.base);
        }
    }

    fn fake_exe() -> PathBuf {
        PathBuf::from(r"C:\fake\cleanroom.exe")
    }

    #[test]
    fn removes_the_legacy_anvil_identity() {
        let root = TestRoot::new("legacy-cleanup");
        // Recreate a pre-rename ANVIL footprint by hand.
        for ext in SUPPORTED_EXTENSIONS {
            root.classes
                .create_subkey(format!(
                    "SystemFileAssociations\\.{ext}\\shell\\{LEGACY_VERB_KEY}"
                ))
                .unwrap();
            root.classes
                .create_subkey(format!(".{ext}\\OpenWithList\\{LEGACY_APP_KEY}"))
                .unwrap();
        }
        root.classes
            .create_subkey(format!("Applications\\{LEGACY_APP_KEY}\\shell\\open\\command"))
            .unwrap();
        root.run
            .set_value(LEGACY_AUTOSTART_VALUE, &String::from("stale ANVIL autostart"))
            .unwrap();

        remove_legacy_identity_at(&root.classes, &root.run).unwrap();

        for ext in SUPPORTED_EXTENSIONS {
            assert!(root
                .classes
                .open_subkey(format!(
                    "SystemFileAssociations\\.{ext}\\shell\\{LEGACY_VERB_KEY}"
                ))
                .is_err());
            assert!(root
                .classes
                .open_subkey(format!(".{ext}\\OpenWithList\\{LEGACY_APP_KEY}"))
                .is_err());
        }
        assert!(root
            .classes
            .open_subkey(format!("Applications\\{LEGACY_APP_KEY}"))
            .is_err());
        assert!(root
            .run
            .get_value::<String, _>(LEGACY_AUTOSTART_VALUE)
            .is_err());

        // Idempotent: a second run against an already-clean root is a no-op, not an error.
        remove_legacy_identity_at(&root.classes, &root.run).unwrap();
    }

    #[test]
    fn context_menu_registers_and_unregisters_all_extensions() {
        let root = TestRoot::new("context-menu");
        register_context_menu_at(&root.classes, &fake_exe()).unwrap();

        for ext in SUPPORTED_EXTENSIONS {
            let verb_path = format!("SystemFileAssociations\\.{ext}\\shell\\{VERB_KEY}");
            let verb_key = root
                .classes
                .open_subkey(&verb_path)
                .unwrap_or_else(|e| panic!("verb key missing for .{ext}: {e}"));
            let label: String = verb_key.get_value("").unwrap();
            assert_eq!(label, VERB_LABEL);

            let cmd: String = verb_key
                .open_subkey("command")
                .unwrap()
                .get_value("")
                .unwrap();
            assert!(cmd.contains("cleanroom.exe"));
            assert!(cmd.contains("%1"));
        }

        unregister_context_menu_at(&root.classes).unwrap();

        for ext in SUPPORTED_EXTENSIONS {
            let verb_path = format!("SystemFileAssociations\\.{ext}\\shell\\{VERB_KEY}");
            assert!(
                root.classes.open_subkey(&verb_path).is_err(),
                "verb key should be gone for .{ext}"
            );
        }
    }

    #[test]
    fn file_associations_register_and_unregister_all_extensions() {
        let root = TestRoot::new("file-assoc");
        register_file_associations_at(&root.classes, &fake_exe()).unwrap();

        let app_cmd: String = root
            .classes
            .open_subkey(format!("Applications\\{APP_KEY}\\shell\\open\\command"))
            .unwrap()
            .get_value("")
            .unwrap();
        assert!(app_cmd.contains("cleanroom.exe"));

        for ext in SUPPORTED_EXTENSIONS {
            let path = format!(".{ext}\\OpenWithList\\{APP_KEY}");
            assert!(
                root.classes.open_subkey(&path).is_ok(),
                "OpenWithList entry missing for .{ext}"
            );
        }

        unregister_file_associations_at(&root.classes).unwrap();

        assert!(root
            .classes
            .open_subkey(format!("Applications\\{APP_KEY}"))
            .is_err());
        for ext in SUPPORTED_EXTENSIONS {
            let path = format!(".{ext}\\OpenWithList\\{APP_KEY}");
            assert!(
                root.classes.open_subkey(&path).is_err(),
                "OpenWithList entry should be gone for .{ext}"
            );
        }
    }

    #[test]
    fn unregistering_file_associations_leaves_sibling_open_with_entries_alone() {
        let root = TestRoot::new("file-assoc-sibling");
        // Simulate another app having already registered under the same extension's
        // OpenWithList — Cleanroom's unregister must not remove it.
        root.classes
            .create_subkey(".wav\\OpenWithList\\other-app.exe")
            .unwrap();

        register_file_associations_at(&root.classes, &fake_exe()).unwrap();
        unregister_file_associations_at(&root.classes).unwrap();

        assert!(
            root.classes
                .open_subkey(".wav\\OpenWithList\\other-app.exe")
                .is_ok(),
            "sibling app's OpenWithList entry must survive Cleanroom's unregister"
        );
    }

    #[test]
    fn autostart_round_trips_and_defaults_off() {
        let root = TestRoot::new("autostart");
        assert!(!is_autostart_enabled_at(&root.run).unwrap());

        set_autostart_at(&root.run, &fake_exe()).unwrap();
        assert!(is_autostart_enabled_at(&root.run).unwrap());
        let cmd: String = root.run.get_value(AUTOSTART_VALUE).unwrap();
        assert!(cmd.contains("cleanroom.exe"));

        clear_autostart_at(&root.run).unwrap();
        assert!(!is_autostart_enabled_at(&root.run).unwrap());
    }

    #[test]
    fn unregister_and_disable_are_idempotent_when_nothing_was_registered() {
        let root = TestRoot::new("idempotent");
        unregister_context_menu_at(&root.classes).unwrap();
        unregister_file_associations_at(&root.classes).unwrap();
        clear_autostart_at(&root.run).unwrap();
    }
}
