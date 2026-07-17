//! Platform abstraction (ADR-006 — "PC-first without a porting cliff").
//!
//! Every OS-specific capability — config/cache locations, file associations, tray,
//! notifications, autostart, shell context menu, dock/taskbar progress, the OS AAC
//! encoder — hides behind this trait. **`#[cfg(windows)]` / `#[cfg(target_os = "macos")]`
//! branching is permitted ONLY in this module.** Anything Windows-specific written
//! elsewhere breaks the macOS CI compile job the day it lands, not in M6.
//!
//! The scaffold implements the portable directory methods; richer capabilities
//! (associations, tray, AAC) are added here as cfg-gated methods per their milestone.
//! M2 lane D added the Windows shell-integration surface (Explorer context menu,
//! "Open with" file associations, login autostart) behind [`windows`]; M6 lane B adds
//! the macOS siblings behind [`macos`] (Finder Quick Action, LaunchAgent autostart,
//! and an honest no-op for the bundle-declared file associations). Any other target
//! falls back to [`stub`], which returns [`PlatformError::NotSupported`] so every OS
//! still compiles the crate.

use std::path::PathBuf;

#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(not(any(windows, target_os = "macos")))]
mod stub;

/// Errors from OS shell-integration operations (context menu, file associations,
/// autostart). Directory lookups (`config_dir`/`cache_dir`) don't fail today so they
/// stay infallible; this type is for the M2 lane D surface and whatever richer
/// capabilities (tray, AAC encoder) join the trait later.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// The requested capability isn't implemented for this OS. Windows and macOS have
    /// real implementations; every other target ([`stub`]) returns this so the crate
    /// still compiles and callers can surface an honest "not available on this OS" message.
    #[error("{0} is not supported on this platform yet")]
    NotSupported(&'static str),

    /// A Windows registry read/write/delete failed.
    #[error("registry operation failed: {0}")]
    Registry(#[from] std::io::Error),

    /// A macOS filesystem operation failed — writing/removing the LaunchAgent plist or the
    /// `Services/*.workflow` bundle under `~/Library`, or reading an app bundle's
    /// `Info.plist`. Distinct from [`PlatformError::Registry`] so the surfaced message is
    /// honest per platform (that variant already owns the blanket `From<io::Error>`, so
    /// `macos.rs` constructs this one explicitly via `.map_err`).
    #[error("filesystem operation failed: {0}")]
    Filesystem(std::io::Error),

    /// Couldn't resolve the path of the running executable to register in the shell.
    #[error("could not determine the current executable path: {0}")]
    ExePath(std::io::Error),
}

/// Convenience alias for the shell-integration surface.
pub type PlatformResult<T> = std::result::Result<T, PlatformError>;

/// OS integration surface. One implementation per platform, selected at compile time.
pub trait Platform: Send + Sync {
    /// Per-user configuration directory (`%APPDATA%/anvil` on Windows,
    /// `~/Library/Application Support/anvil` on macOS).
    fn config_dir(&self) -> PathBuf;

    /// Per-user cache directory for the LRU intermediate-render cache
    /// (`%LOCALAPPDATA%/anvil` on Windows, `~/Library/Caches/anvil` on macOS).
    fn cache_dir(&self) -> PathBuf;

    /// Short platform name, for diagnostics and logs.
    fn name(&self) -> &'static str;

    /// Add a "Master with Cleanroom" verb to the Explorer context menu for Cleanroom's
    /// supported audio/video file types (M2 lane D). Per-user (`HKCU`) only — no admin
    /// rights required — and registered under `SystemFileAssociations` so it never
    /// touches a type's default "Open" handler. Idempotent.
    fn register_context_menu(&self) -> PlatformResult<()>;

    /// Remove the context-menu verb added by [`Platform::register_context_menu`].
    /// Idempotent — safe to call when nothing is registered.
    fn unregister_context_menu(&self) -> PlatformResult<()>;

    /// Register Cleanroom as an "Open with" handler for its supported audio/video file
    /// types. Per-user, additive only — it never changes a type's default handler.
    fn register_file_associations(&self) -> PlatformResult<()>;

    /// Remove the "Open with" registration added by
    /// [`Platform::register_file_associations`]. Idempotent.
    fn unregister_file_associations(&self) -> PlatformResult<()>;

    /// Enable or disable launching the Cleanroom tray/watch-folder agent at login
    /// (default off). Owns only the registry plumbing — the agent itself is M2 lane E.
    fn set_autostart(&self, enabled: bool) -> PlatformResult<()>;

    /// Whether [`Platform::set_autostart`] is currently enabled.
    fn is_autostart_enabled(&self) -> PlatformResult<bool>;
}

/// The concrete [`Platform`] for the current target. Its capability *implementations*
/// diverge per OS via cfg-gated methods added in later milestones; the directory logic
/// is shared through the `dirs` crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct CurrentPlatform;

impl Platform for CurrentPlatform {
    fn config_dir(&self) -> PathBuf {
        dirs::config_dir().unwrap_or_default().join("cleanroom")
    }

    fn cache_dir(&self) -> PathBuf {
        dirs::cache_dir().unwrap_or_default().join("cleanroom")
    }

    fn name(&self) -> &'static str {
        #[cfg(windows)]
        {
            "windows"
        }
        #[cfg(target_os = "macos")]
        {
            "macos"
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            "other"
        }
    }

    fn register_context_menu(&self) -> PlatformResult<()> {
        #[cfg(windows)]
        {
            windows::register_context_menu()
        }
        #[cfg(target_os = "macos")]
        {
            macos::register_context_menu()
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            stub::register_context_menu()
        }
    }

    fn unregister_context_menu(&self) -> PlatformResult<()> {
        #[cfg(windows)]
        {
            windows::unregister_context_menu()
        }
        #[cfg(target_os = "macos")]
        {
            macos::unregister_context_menu()
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            stub::unregister_context_menu()
        }
    }

    fn register_file_associations(&self) -> PlatformResult<()> {
        #[cfg(windows)]
        {
            windows::register_file_associations()
        }
        #[cfg(target_os = "macos")]
        {
            macos::register_file_associations()
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            stub::register_file_associations()
        }
    }

    fn unregister_file_associations(&self) -> PlatformResult<()> {
        #[cfg(windows)]
        {
            windows::unregister_file_associations()
        }
        #[cfg(target_os = "macos")]
        {
            macos::unregister_file_associations()
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            stub::unregister_file_associations()
        }
    }

    fn set_autostart(&self, enabled: bool) -> PlatformResult<()> {
        #[cfg(windows)]
        {
            windows::set_autostart(enabled)
        }
        #[cfg(target_os = "macos")]
        {
            macos::set_autostart(enabled)
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            stub::set_autostart(enabled)
        }
    }

    fn is_autostart_enabled(&self) -> PlatformResult<bool> {
        #[cfg(windows)]
        {
            windows::is_autostart_enabled()
        }
        #[cfg(target_os = "macos")]
        {
            macos::is_autostart_enabled()
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            stub::is_autostart_enabled()
        }
    }
}

/// Obtain the platform implementation for the current OS.
/// Remove the registry/shell footprint of the pre-rename **Cleanroom** identity, so an in-place
/// upgrade from an old Cleanroom install doesn't leave orphaned "Open with" / context-menu /
/// autostart entries (the rename changed their key names, so the installer never removes
/// them). Per-user, idempotent, best-effort. No-op on non-Windows.
pub fn remove_legacy_windows_identity() {
    #[cfg(windows)]
    if let Err(e) = windows::remove_legacy_identity() {
        tracing::warn!(error = %e, "failed to remove legacy Cleanroom shell integration");
    }
}

pub fn current() -> CurrentPlatform {
    CurrentPlatform
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirs_are_namespaced_under_anvil() {
        let p = current();
        assert!(p.config_dir().ends_with("cleanroom"));
        assert!(p.cache_dir().ends_with("cleanroom"));
        assert!(!p.name().is_empty());
    }
}
