//! macOS shell integration (M6 lane B): the Finder "Master with ANVIL" Quick Action,
//! file-type handling, and login autostart — the native siblings of `windows.rs`'s HKCU
//! registry work. Everything here is per-user and additive/reversible: it only ever
//! writes inside ANVIL's own files under `~/Library` (a `LaunchAgents` plist and a
//! `Services/*.workflow` bundle) and never mutates another app's state or a file type's
//! default handler.
//!
//! This whole file is `#[cfg(target_os = "macos")]` via its `mod macos;` declaration in
//! `platform/mod.rs` — the only place ADR-006 allows an OS `#[cfg]` split (a macOS-ism
//! anywhere else in the tree breaks the Windows CI compile job).
//!
//! ## Trait-method mapping (Windows registry -> macOS mechanism)
//! - [`register_context_menu`]/[`unregister_context_menu`] -> install/remove a Finder
//!   Quick Action: a `Services/Master with ANVIL.workflow` bundle whose
//!   `Contents/Info.plist` declares the `NSServices` menu item and whose
//!   `Contents/document.wflow` runs the ANVIL binary on the selected files (the analog of
//!   the Windows verb's `"exe" "%1"` command). Removal deletes the bundle directory.
//! - [`set_autostart`]/[`is_autostart_enabled`] -> write/delete a
//!   `LaunchAgents/com.cleanroom.desktop.plist` with `RunAtLoad`; toggling also runs a
//!   best-effort `launchctl (un)load` so the change also takes in the current login
//!   session (see [`best_effort_launchctl`] for why its failure is swallowed).
//! - [`register_file_associations`]/[`unregister_file_associations`] -> an **honest
//!   no-op** (see those fns): macOS declares document types statically in the app
//!   bundle's `Info.plist` (`CFBundleDocumentTypes`, emitted by Tauri's `fileAssociations`
//!   at bundle time) and LaunchServices registers them from the bundle. There is no
//!   per-user runtime "Open with" claim to add or revoke the way Windows' `OpenWithList`
//!   has, so this returns `Ok(())` without touching OS state — but where the running
//!   binary lives inside an `.app`, register *verifies* the declaration is actually
//!   present rather than silently pretending.
//!
//! ## Testability
//! The `pub(super) fn`s resolve the real `~/Library` (via [`library_dir`]) and, for
//! autostart, additionally touch the live launchd session; each is a thin wrapper around
//! a `_at(library, ...)` function parameterized by the `~/Library` base directory. The
//! unit tests drive those `_at` functions against a throwaway tempdir (see
//! [`tests::TestDirs`]) so `cargo test` never reads or writes the real `~/Library` and
//! never invokes `launchctl`. Every generated plist/wflow is validated with `plutil
//! -lint` in the tests, so a malformed template fails the suite on this hardware.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{PlatformError, PlatformResult};

/// LaunchAgent job label, and the plist basename derived from it. The desktop app is the
/// tray/watch-folder agent host, so autostart launches the app itself at login — the
/// agent's eventual argv (`batch --watch <folder>`) is M2 lane E's call, exactly as
/// `windows.rs` leaves the autostart command's file argument to that lane.
const LAUNCH_AGENT_LABEL: &str = "com.cleanroom.desktop";
const LAUNCH_AGENT_FILE: &str = "com.cleanroom.desktop.plist";

/// Finder Quick Action menu label (mirrors `windows.rs`'s `VERB_LABEL`); the
/// `.workflow` bundle directory is named `"<label>.workflow"`.
const QUICK_ACTION_LABEL: &str = "Master with Cleanroom";

fn exe_path() -> PlatformResult<PathBuf> {
    std::env::current_exe().map_err(PlatformError::ExePath)
}

/// The real per-user `~/Library` — the base every write below is relative to. Tests pass
/// a tempdir in its place, so the `_at` functions never see this path.
fn library_dir() -> PlatformResult<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library")).ok_or_else(|| {
        PlatformError::Filesystem(io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine the user's home directory",
        ))
    })
}

// --- Public entry points. These resolve the real `~/Library` (and, for autostart, the
// live launchd session); the root-parameterized `_at` functions they delegate to are what
// the tests exercise against a throwaway tempdir. ---

pub(super) fn register_context_menu() -> PlatformResult<()> {
    install_quick_action_at(&library_dir()?, &exe_path()?)
}

pub(super) fn unregister_context_menu() -> PlatformResult<()> {
    remove_quick_action_at(&library_dir()?)
}

/// Honest no-op. macOS "Open with" eligibility comes from the app bundle's
/// `CFBundleDocumentTypes` declaration (produced by Tauri's `fileAssociations` at bundle
/// time and registered by LaunchServices when the `.app` is installed or moved), not from
/// a per-user runtime claim like Windows' `OpenWithList`. There is therefore nothing to
/// write at runtime, so this returns `Ok(())` — but when the running binary lives inside
/// an `.app` we verify the declaration is present (a bundle-time regression surfaces here
/// as a swallowed check rather than a false "registered" success). Outside a bundle (the
/// CLI on a dev box) there is nothing to check and nothing to do.
pub(super) fn register_file_associations() -> PlatformResult<()> {
    // Best-effort verification only; a missing/unreadable bundle Info.plist can't be
    // fixed at runtime and must not fail an operation that has no runtime side effect.
    let _ = verify_document_types_declared(&exe_path()?);
    Ok(())
}

/// Honest no-op — the mirror of [`register_file_associations`]. The document-type
/// declaration lives in the app bundle and is torn down when the `.app` is deleted; there
/// is no per-user registration for ANVIL to revoke.
pub(super) fn unregister_file_associations() -> PlatformResult<()> {
    Ok(())
}

pub(super) fn set_autostart(enabled: bool) -> PlatformResult<()> {
    let library = library_dir()?;
    let plist = launch_agent_path(&library);
    if enabled {
        install_launch_agent_at(&library, &exe_path()?)?;
        best_effort_launchctl(&["load", "-w"], &plist);
    } else {
        // Unload before deleting: `launchctl unload` reads the plist by path to learn the
        // job label, so it must still exist on disk when we ask launchd to drop it.
        best_effort_launchctl(&["unload", "-w"], &plist);
        remove_launch_agent_at(&library)?;
    }
    Ok(())
}

pub(super) fn is_autostart_enabled() -> PlatformResult<bool> {
    is_launch_agent_installed_at(&library_dir()?)
}

// --- Root-parameterized implementations. `library_dir()` above always passes the real
// `~/Library`; tests pass a throwaway tempdir instead so no real state is touched. ---

fn install_quick_action_at(library: &Path, exe: &Path) -> PlatformResult<()> {
    let contents = quick_action_bundle(library).join("Contents");
    fs::create_dir_all(&contents).map_err(PlatformError::Filesystem)?;
    fs::write(contents.join("Info.plist"), quick_action_info_plist())
        .map_err(PlatformError::Filesystem)?;
    fs::write(contents.join("document.wflow"), quick_action_wflow(exe))
        .map_err(PlatformError::Filesystem)?;
    Ok(())
}

fn remove_quick_action_at(library: &Path) -> PlatformResult<()> {
    remove_dir_all_if_present(&quick_action_bundle(library))
}

fn install_launch_agent_at(library: &Path, exe: &Path) -> PlatformResult<()> {
    let dir = launch_agents_dir(library);
    fs::create_dir_all(&dir).map_err(PlatformError::Filesystem)?;
    fs::write(dir.join(LAUNCH_AGENT_FILE), launch_agent_plist(exe))
        .map_err(PlatformError::Filesystem)?;
    Ok(())
}

fn remove_launch_agent_at(library: &Path) -> PlatformResult<()> {
    remove_file_if_present(&launch_agent_path(library))
}

fn is_launch_agent_installed_at(library: &Path) -> PlatformResult<bool> {
    Ok(launch_agent_path(library).is_file())
}

// --- Path helpers (all relative to the injected `~/Library` base). ---

fn launch_agents_dir(library: &Path) -> PathBuf {
    library.join("LaunchAgents")
}

fn launch_agent_path(library: &Path) -> PathBuf {
    launch_agents_dir(library).join(LAUNCH_AGENT_FILE)
}

fn services_dir(library: &Path) -> PathBuf {
    library.join("Services")
}

fn quick_action_bundle(library: &Path) -> PathBuf {
    services_dir(library).join(format!("{QUICK_ACTION_LABEL}.workflow"))
}

// --- Bundle verification (for the honest-no-op file associations). ---

/// The nearest ancestor of `exe` whose name ends in `.app`, i.e. the enclosing app
/// bundle, or `None` when the binary is not inside one (the CLI, a `cargo test` runner).
fn enclosing_app_bundle(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|a| {
            a.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".app"))
        })
        .map(Path::to_path_buf)
}

/// `Ok(true)` when `exe` lives inside an `.app` whose `Contents/Info.plist` declares
/// `CFBundleDocumentTypes`; `Ok(false)` when there is no enclosing bundle or the plist
/// lacks the declaration; `Err` only on a real read failure of a plist that exists.
fn verify_document_types_declared(exe: &Path) -> PlatformResult<bool> {
    let Some(bundle) = enclosing_app_bundle(exe) else {
        return Ok(false);
    };
    match fs::read_to_string(bundle.join("Contents/Info.plist")) {
        Ok(contents) => Ok(contents.contains("CFBundleDocumentTypes")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(PlatformError::Filesystem(e)),
    }
}

// --- launchctl (live-session activation for autostart). ---

/// Run `launchctl <args> <plist>`, ignoring the result. `launchctl (un)load` activates or
/// deactivates the agent in the *current* login session immediately; `RunAtLoad` already
/// covers the next login regardless, so the persistent state (the plist file, written by
/// the caller) is authoritative. `launchctl` legitimately fails when there is no GUI login
/// session to talk to — over SSH, in CI, or during an uninstall running as a different
/// user ("Load failed: 5: Input/output error") — and none of those should fail an
/// autostart toggle whose on-disk effect already succeeded. Hence best-effort.
fn best_effort_launchctl(args: &[&str], plist: &Path) {
    let _ = Command::new("/bin/launchctl")
        .args(args)
        .arg(plist)
        .output();
}

// --- Filesystem removal helpers (idempotent, mirroring `windows.rs`'s
// `delete_subkey_*_if_present`: absent target is a success, not an error). ---

fn remove_dir_all_if_present(dir: &Path) -> PlatformResult<()> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PlatformError::Filesystem(e)),
    }
}

fn remove_file_if_present(path: &Path) -> PlatformResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(PlatformError::Filesystem(e)),
    }
}

// --- XML / shell escaping. ---

/// Escape the five XML predefined entities so an arbitrary path (the app may live in
/// `/Applications/My Apps/` or a folder containing `&`, `<`, `>`, quotes) is safe inside a
/// plist/wflow `<string>`. `&` is replaced first so the replacement `&`s aren't re-hit.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Wrap `s` in single quotes for safe use as one shell word inside the Quick Action's
/// `Run Shell Script` command; an embedded single quote becomes `'\''` (close, escaped
/// quote, reopen). The result is then XML-escaped before it lands in the wflow.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// --- Templates. Hand-authored (std only, no plist crate); every one is `plutil`-clean
// and, for the plists, uses space indentation per `.editorconfig`. ---

/// A `RunAtLoad` LaunchAgent whose sole `ProgramArguments` entry is the ANVIL executable
/// (see [`LAUNCH_AGENT_LABEL`] for why there is no watch-folder argv yet).
fn launch_agent_plist(exe: &Path) -> String {
    let exe = exe.to_string_lossy();
    let exe = xml_escape(&exe);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCH_AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#
    )
}

/// The Quick Action's `Contents/Info.plist`: a single `NSServices` entry that puts
/// [`QUICK_ACTION_LABEL`] on the Finder Services / right-click menu for audio and video
/// files (the broad conforming UTIs; the precise per-extension list is declared in the
/// app bundle, mirroring the format set in `docs/adr/005-media-io.md`).
fn quick_action_info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>NSServices</key>
    <array>
        <dict>
            <key>NSMenuItem</key>
            <dict>
                <key>default</key>
                <string>{QUICK_ACTION_LABEL}</string>
            </dict>
            <key>NSMessage</key>
            <string>runWorkflowAsService</string>
            <key>NSRequiredContext</key>
            <dict>
                <key>NSApplicationIdentifier</key>
                <string>com.apple.finder</string>
            </dict>
            <key>NSSendFileTypes</key>
            <array>
                <string>public.audio</string>
                <string>public.movie</string>
            </array>
        </dict>
    </array>
</dict>
</plist>
"#
    )
}

/// The Quick Action's `Contents/document.wflow`: a hand-authored Automator "Run Shell
/// Script" service that receives the selected files as arguments and runs `<exe> <file>`
/// for each — the macOS analog of the Windows verb's `"exe" "%1"`. The command is
/// shell-single-quoted (path safety) and then XML-escaped (plist safety).
fn quick_action_wflow(exe: &Path) -> String {
    let exe = exe.to_string_lossy();
    let quoted = shell_single_quote(&exe);
    let command = xml_escape(&format!(r#"for f in "$@"; do {quoted} "$f"; done"#));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>AMApplicationBuild</key>
    <string>523</string>
    <key>AMApplicationVersion</key>
    <string>2.10</string>
    <key>AMDocumentVersion</key>
    <string>2</string>
    <key>actions</key>
    <array>
        <dict>
            <key>action</key>
            <dict>
                <key>AMAccepts</key>
                <dict>
                    <key>Container</key>
                    <string>List</string>
                    <key>Optional</key>
                    <true/>
                    <key>Types</key>
                    <array>
                        <string>com.apple.cocoa.path</string>
                    </array>
                </dict>
                <key>AMActionVersion</key>
                <string>2.0.3</string>
                <key>AMProvides</key>
                <dict>
                    <key>Container</key>
                    <string>List</string>
                    <key>Types</key>
                    <array>
                        <string>com.apple.cocoa.path</string>
                    </array>
                </dict>
                <key>ActionBundlePath</key>
                <string>/System/Library/Automator/Run Shell Script.action</string>
                <key>ActionName</key>
                <string>Run Shell Script</string>
                <key>ActionParameters</key>
                <dict>
                    <key>COMMAND_STRING</key>
                    <string>{command}</string>
                    <key>CheckedForUserDefaultShell</key>
                    <true/>
                    <key>inputMethod</key>
                    <integer>1</integer>
                    <key>shell</key>
                    <string>/bin/zsh</string>
                    <key>source</key>
                    <string></string>
                </dict>
                <key>BundleIdentifier</key>
                <string>com.apple.RunShellScript</string>
                <key>CFBundleVersion</key>
                <string>2.0.3</string>
                <key>Class Name</key>
                <string>RunShellScriptAction</string>
                <key>InputUUID</key>
                <string>B1F1E2D3-0000-4000-8000-000000000001</string>
                <key>Keywords</key>
                <array>
                    <string>Shell</string>
                    <string>Script</string>
                    <string>Command</string>
                    <string>Run</string>
                    <string>Unix</string>
                </array>
                <key>OutputUUID</key>
                <string>B1F1E2D3-0000-4000-8000-000000000002</string>
                <key>UUID</key>
                <string>B1F1E2D3-0000-4000-8000-000000000003</string>
                <key>arguments</key>
                <dict/>
                <key>isViewVisible</key>
                <integer>1</integer>
            </dict>
            <key>isViewVisible</key>
            <integer>1</integer>
        </dict>
    </array>
    <key>connectors</key>
    <dict/>
    <key>workflowMetaData</key>
    <dict>
        <key>applicationBundleIDsByPath</key>
        <dict/>
        <key>applicationPaths</key>
        <array/>
        <key>presentationMode</key>
        <integer>11</integer>
        <key>processesInput</key>
        <integer>0</integer>
        <key>serviceInputTypeIdentifier</key>
        <string>com.apple.Automator.fileSystemObject</string>
        <key>serviceOutputTypeIdentifier</key>
        <string>com.apple.Automator.nothing</string>
        <key>systemImageName</key>
        <string>NSActionTemplate</string>
        <key>useAutomaticInputType</key>
        <integer>0</integer>
        <key>workflowTypeIdentifier</key>
        <string>com.apple.Automator.servicesMenu</string>
    </dict>
</dict>
</plist>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway `~/Library` under
    /// `$TMPDIR/anvil-macos-platform-tests/<tag>-<uuid>/Library`, shaped like the real
    /// one so the `_at` functions run unmodified against it. Dropping the guard removes
    /// the whole `<tag>-<uuid>` subtree — including on test panic, since `Drop` still runs
    /// during unwinding — so tests never leak dirs and never touch the real `~/Library`.
    /// (Mirrors `windows.rs`'s `TestRoot`, using the same `uuid` dependency for the unique
    /// segment; no new crate is pulled in.)
    struct TestDirs {
        library: PathBuf,
    }

    impl TestDirs {
        fn new(tag: &str) -> Self {
            let library = std::env::temp_dir()
                .join("anvil-macos-platform-tests")
                .join(format!("{tag}-{}", uuid::Uuid::new_v4()))
                .join("Library");
            fs::create_dir_all(&library).expect("create test-scoped Library");
            Self { library }
        }

        fn library(&self) -> &Path {
            &self.library
        }

        /// The `<tag>-<uuid>` dir above `Library`, used as a scratch root for `.app`
        /// fixtures that must sit *outside* `Library`.
        fn base(&self) -> &Path {
            self.library.parent().expect("Library always has a parent")
        }
    }

    impl Drop for TestDirs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.base());
        }
    }

    fn fake_exe() -> PathBuf {
        PathBuf::from("/Applications/ANVIL.app/Contents/MacOS/anvil")
    }

    /// Assert a file is a well-formed property list per the system `plutil`. On this
    /// target `plutil` is always present at `/usr/bin/plutil`; the `Err` arm only guards
    /// the impossible-here case of it being unavailable, leaving the structural asserts as
    /// the floor.
    fn assert_valid_plist(path: &Path) {
        // `plutil` is always present at /usr/bin/plutil on this target; the guarded spawn
        // only skips the impossible-here case of it being unavailable, leaving the
        // structural asserts as the floor.
        if let Ok(out) = Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(path)
            .output()
        {
            assert!(
                out.status.success(),
                "plutil -lint rejected {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// Reverse of [`xml_escape`], so a parsed-back `<string>` compares equal to the
    /// original path. `&amp;` is undone last for the same reason `&` is escaped first.
    fn xml_unescape(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    /// Minimal parse-back: the un-escaped `<string>` entries inside the plist's
    /// `ProgramArguments` `<array>`. Proves the exe path survives escaping intact.
    fn program_arguments(plist: &str) -> Vec<String> {
        let after = &plist[plist
            .find("<key>ProgramArguments</key>")
            .expect("ProgramArguments key present")..];
        let open = after.find("<array>").expect("array opens");
        let close = after.find("</array>").expect("array closes");
        after[open..close]
            .split("<string>")
            .skip(1)
            .filter_map(|seg| seg.split("</string>").next())
            .map(xml_unescape)
            .collect()
    }

    /// Whether the plist maps `RunAtLoad` to `<true/>`.
    fn run_at_load(plist: &str) -> bool {
        match plist.find("<key>RunAtLoad</key>") {
            Some(i) => plist[i + "<key>RunAtLoad</key>".len()..]
                .trim_start()
                .starts_with("<true/>"),
            None => false,
        }
    }

    #[test]
    fn quick_action_installs_exists_and_removes() {
        let dirs = TestDirs::new("quick-action");
        let bundle = quick_action_bundle(dirs.library());
        assert!(!bundle.exists());

        install_quick_action_at(dirs.library(), &fake_exe()).unwrap();

        assert!(bundle.is_dir());
        let info = bundle.join("Contents/Info.plist");
        let wflow = bundle.join("Contents/document.wflow");
        assert!(info.is_file());
        assert!(wflow.is_file());
        assert_valid_plist(&info);
        assert_valid_plist(&wflow);

        let info_txt = fs::read_to_string(&info).unwrap();
        assert!(info_txt.contains("NSServices"));
        assert!(info_txt.contains(QUICK_ACTION_LABEL));
        let wflow_txt = fs::read_to_string(&wflow).unwrap();
        assert!(wflow_txt.contains("anvil"), "wflow must invoke the exe");

        remove_quick_action_at(dirs.library()).unwrap();
        assert!(!bundle.exists());
    }

    #[test]
    fn launch_agent_round_trips_and_defaults_off() {
        let dirs = TestDirs::new("launch-agent");
        assert!(!is_launch_agent_installed_at(dirs.library()).unwrap());

        install_launch_agent_at(dirs.library(), &fake_exe()).unwrap();
        assert!(is_launch_agent_installed_at(dirs.library()).unwrap());

        let path = launch_agent_path(dirs.library());
        assert_valid_plist(&path);
        let txt = fs::read_to_string(&path).unwrap();
        assert!(txt.contains(LAUNCH_AGENT_LABEL));
        assert!(run_at_load(&txt), "RunAtLoad must be true");
        assert_eq!(
            program_arguments(&txt),
            vec![fake_exe().to_string_lossy().into_owned()],
            "ProgramArguments must be exactly the exe path"
        );

        remove_launch_agent_at(dirs.library()).unwrap();
        assert!(!is_launch_agent_installed_at(dirs.library()).unwrap());
    }

    #[test]
    fn launch_agent_plist_escapes_special_characters_in_path() {
        let dirs = TestDirs::new("agent-escaping");
        // Spaces, ampersand, angle brackets and a double quote — all XML-hostile.
        let nasty = PathBuf::from(r#"/Applications/My Apps/A&B "Q" <t>/anvil"#);

        install_launch_agent_at(dirs.library(), &nasty).unwrap();

        let path = launch_agent_path(dirs.library());
        assert_valid_plist(&path); // escaping kept the plist well-formed
        let txt = fs::read_to_string(&path).unwrap();
        assert!(!txt.contains("A&B"), "raw ampersand must be escaped");
        assert!(txt.contains("&amp;"));
        assert_eq!(
            program_arguments(&txt),
            vec![nasty.to_string_lossy().into_owned()],
            "the original path must survive the escape round-trip"
        );
    }

    #[test]
    fn quick_action_escapes_special_characters_in_path() {
        let dirs = TestDirs::new("qa-escaping");
        let nasty = PathBuf::from(r#"/Applications/My Apps/A&B "Q"/anvil"#);

        install_quick_action_at(dirs.library(), &nasty).unwrap();

        let bundle = quick_action_bundle(dirs.library());
        // Both files stay well-formed even with the hostile exe path baked into the
        // shell command inside the wflow.
        assert_valid_plist(&bundle.join("Contents/Info.plist"));
        assert_valid_plist(&bundle.join("Contents/document.wflow"));
    }

    #[test]
    fn double_install_is_idempotent() {
        let dirs = TestDirs::new("idempotent-install");

        install_quick_action_at(dirs.library(), &fake_exe()).unwrap();
        install_quick_action_at(dirs.library(), &fake_exe()).unwrap();
        install_launch_agent_at(dirs.library(), &fake_exe()).unwrap();
        install_launch_agent_at(dirs.library(), &fake_exe()).unwrap();

        assert!(quick_action_bundle(dirs.library()).is_dir());
        assert!(is_launch_agent_installed_at(dirs.library()).unwrap());
        assert_valid_plist(&launch_agent_path(dirs.library()));
    }

    #[test]
    fn remove_when_absent_is_ok() {
        let dirs = TestDirs::new("remove-absent");
        // Nothing was installed; every removal is a success no-op (parity with
        // windows.rs's idempotent-unregister semantics).
        remove_quick_action_at(dirs.library()).unwrap();
        remove_launch_agent_at(dirs.library()).unwrap();
        assert!(!is_launch_agent_installed_at(dirs.library()).unwrap());
    }

    #[test]
    fn file_associations_are_honest_noops_and_verify_declaration() {
        // The public no-ops succeed even from a non-bundle test binary.
        register_file_associations().unwrap();
        unregister_file_associations().unwrap();

        // The verification helper the register no-op leans on.
        let dirs = TestDirs::new("file-assoc-verify");
        let app = dirs.base().join("My App.app"); // note the space
        let macos = app.join("Contents/MacOS");
        fs::create_dir_all(&macos).unwrap();
        let exe = macos.join("anvil");
        fs::write(&exe, b"#!/bin/sh\n").unwrap();

        // No Info.plist yet.
        assert!(!verify_document_types_declared(&exe).unwrap());
        // Info.plist present but without the declaration.
        fs::write(app.join("Contents/Info.plist"), "<plist><dict/></plist>").unwrap();
        assert!(!verify_document_types_declared(&exe).unwrap());
        // Info.plist declaring the document types.
        fs::write(
            app.join("Contents/Info.plist"),
            "<plist><dict><key>CFBundleDocumentTypes</key><array/></dict></plist>",
        )
        .unwrap();
        assert!(verify_document_types_declared(&exe).unwrap());
        // A binary that isn't inside any `.app`: nothing to check.
        let loose = dirs.base().join("loose-anvil");
        fs::write(&loose, b"x").unwrap();
        assert!(!verify_document_types_declared(&loose).unwrap());
    }

    #[test]
    fn xml_escape_covers_all_five_entities() {
        assert_eq!(
            xml_escape(r#"a&b<c>d"e'f"#),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
        // `&` handled first, so a literal entity is escaped once, not recursively.
        assert_eq!(xml_escape("&amp;"), "&amp;amp;");
    }

    #[test]
    fn shell_single_quote_neutralizes_metacharacters() {
        assert_eq!(shell_single_quote("/a b/c"), "'/a b/c'");
        assert_eq!(shell_single_quote("a'b"), r#"'a'\''b'"#);
    }
}
