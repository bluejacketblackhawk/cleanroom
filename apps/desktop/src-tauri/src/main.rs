// Prevents an additional console window on Windows in release. DO NOT REMOVE.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // The NSIS uninstaller (`installer-hooks.nsh`, `NSIS_HOOK_PREUNINSTALL`) execs this
    // exe with `--uninstall-cleanup` before it deletes any files. `anvil-core::platform`
    // writes its own HKCU keys (Explorer context menu, "Open with" list, autostart) only
    // when the user opts in from Settings — the installer never wrote them, so its own
    // generated uninstall logic doesn't know about them either. This flag runs the same
    // idempotent unregister calls the Settings toggles use, so uninstall never leaves
    // orphaned registry keys behind regardless of what the user turned on. Never opens a
    // window; exits immediately either way.
    if std::env::args().any(|a| a == "--uninstall-cleanup") {
        anvil_desktop_lib::uninstall_cleanup();
        return;
    }
    anvil_desktop_lib::run()
}
