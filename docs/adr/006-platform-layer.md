# ADR-006: Platform Layer

- Status: Accepted (from the build handoff)
- Date: 2026-07-13
- Source: handoff/02-ARCHITECTURE.md § Platform layer (ADR-006) — the PC-first-without-a-porting-cliff rule

## Context

Cleanroom targets Windows and macOS with identical features and quality. Porting from Windows to macOS (or vice versa) should not require architectural rewrites. Platform differences exist (file dialogs, notifications, OS encoders, shell integration) but must be isolated to prevent feature drift and build-time surprises. Compile-time enforcement (CI checks macOS builds from M0) surfaces platform isms immediately.

## Decision

All OS-specific code lives in `anvil-core::platform` behind one trait set:
- File dialogs/associations
- Tray/dock integration
- Notifications
- Autostart (watch-folder agent)
- Shell context menu (Win registry / Mac Services+Quick Action)
- Taskbar/dock progress reporting
- OS AAC encoder (Media Foundation / AudioToolbox)

**Compile-time rule:** `#[cfg(windows)]`/`#[cfg(target_os="macos")]` allowed ONLY inside this module. Any Windows-ism outside `platform` breaks the build immediately (enforce in clippy lint or CI script).

**CI requirement:** Compiles and unit-tests macOS from M0 (even if not binary-shipped initially). Any platform mismatch surfaces before merge, not in M6.

**Target matrix:**
- Windows: Windows 10 1809+ x64 (AVX2 recommended, SSE4.2 minimum — runtime dispatch), ARM64 stretch goal (M5)
- macOS: 12+ universal2 (arm64 + x86_64); Intel iMacs (first-class QA target incl. 8 GB RAM machines)

## Consequences

**Enables:** Unified feature set across OSes; platform porting = trait impl + tests, not core rewrites; build-time verification of platform isolation; Mac target viable from day one; easy feature backport across platforms

**Constrains:**
- Enforce: Platform-specific logic MUST go behind the trait layer. Code review: check that cfg blocks only appear in `platform` module.
- Platform trait must be comprehensive: if a new feature needs OS-specific logic, add it to the trait before feature impl.
- CI must run on macOS (M0 or self-hosted M-series build) every commit; blocking build failure if macOS fails.
- Performance tuning (e.g. GPU dispatch, Accelerate vs generic Rust) handled in platform layer; no per-OS code scattered across DSP modules.
