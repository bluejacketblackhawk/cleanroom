//! Non-Windows stub for the shell-integration surface (M2 lane D).
//!
//! ADR-006 requires every target to compile — macOS CI must stay green even though
//! Finder Quick Action / macOS file associations aren't built until M6 lane B. Rather
//! than silently no-op, every method here returns a typed
//! [`super::PlatformError::NotSupported`] so callers (CLI, desktop settings UI) can
//! surface an honest "not available on this OS yet" message instead of pretending the
//! registration succeeded.
//!
//! This whole file is `#[cfg(not(windows))]` via its `mod stub;` declaration in
//! `platform/mod.rs` — the only place ADR-006 allows that split.

use super::{PlatformError, PlatformResult};

fn not_supported<T>(what: &'static str) -> PlatformResult<T> {
    Err(PlatformError::NotSupported(what))
}

pub(super) fn register_context_menu() -> PlatformResult<()> {
    not_supported("the Explorer context menu")
}

pub(super) fn unregister_context_menu() -> PlatformResult<()> {
    not_supported("the Explorer context menu")
}

pub(super) fn register_file_associations() -> PlatformResult<()> {
    not_supported("file associations")
}

pub(super) fn unregister_file_associations() -> PlatformResult<()> {
    not_supported("file associations")
}

pub(super) fn set_autostart(_enabled: bool) -> PlatformResult<()> {
    not_supported("autostart")
}

pub(super) fn is_autostart_enabled() -> PlatformResult<bool> {
    not_supported("autostart")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_returns_not_supported() {
        assert!(matches!(
            register_context_menu(),
            Err(PlatformError::NotSupported(_))
        ));
        assert!(matches!(
            unregister_context_menu(),
            Err(PlatformError::NotSupported(_))
        ));
        assert!(matches!(
            register_file_associations(),
            Err(PlatformError::NotSupported(_))
        ));
        assert!(matches!(
            unregister_file_associations(),
            Err(PlatformError::NotSupported(_))
        ));
        assert!(matches!(
            set_autostart(true),
            Err(PlatformError::NotSupported(_))
        ));
        assert!(matches!(
            is_autostart_enabled(),
            Err(PlatformError::NotSupported(_))
        ));
    }
}
