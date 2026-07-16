//! Settings → Integration (04 §S8): thin command wrappers over `anvil_core::platform`'s
//! Explorer context menu / "Open with" file associations / login autostart. All three are
//! per-user (`HKCU`), off by default, and idempotent — the Settings screen just reflects
//! and flips them. Uninstall hygiene for whatever the user turned on here lives in
//! `main.rs`'s `--uninstall-cleanup` flag (run by the NSIS uninstaller), not here.

use anvil_core::platform::{Platform, PlatformResult};

#[tauri::command]
pub fn settings_set_context_menu(enabled: bool) -> Result<(), String> {
    let platform = anvil_core::platform::current();
    let result: PlatformResult<()> = if enabled {
        platform.register_context_menu()
    } else {
        platform.unregister_context_menu()
    };
    result.map_err(|e| e.to_string())
}

#[tauri::command]
pub fn settings_set_file_associations(enabled: bool) -> Result<(), String> {
    let platform = anvil_core::platform::current();
    let result: PlatformResult<()> = if enabled {
        platform.register_file_associations()
    } else {
        platform.unregister_file_associations()
    };
    result.map_err(|e| e.to_string())
}

#[tauri::command]
pub fn settings_set_autostart(enabled: bool) -> Result<(), String> {
    anvil_core::platform::current()
        .set_autostart(enabled)
        .map_err(|e| e.to_string())
}

/// Current state of all three toggles, read fresh from the registry each call (cheap,
/// and never drifts from what's actually registered — no cached/duplicated state here).
#[derive(Debug, serde::Serialize)]
pub struct IntegrationStatus {
    pub autostart: bool,
}

#[tauri::command]
pub fn settings_get_integration_status() -> Result<IntegrationStatus, String> {
    // Context-menu/file-association registration has no cheap "is it on" read (it's N
    // per-extension keys, not one flag) — the UI treats those two as write-only toggles
    // and trusts its own last-set state, same as most OS integration panels. Autostart is
    // one value, so it's worth reading back for real.
    let autostart = anvil_core::platform::current()
        .is_autostart_enabled()
        .map_err(|e| e.to_string())?;
    Ok(IntegrationStatus { autostart })
}
