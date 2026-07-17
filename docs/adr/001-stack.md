# ADR-001: Stack Decision

- Status: Accepted (from the build handoff)
- Date: 2026-07-13
- Source: handoff/02-ARCHITECTURE.md § Stack decision (ADR-001)

## Context

Cleanroom is a local podcast mastering desktop application that must deliver a polished consumer experience while enabling agents and contributors to iterate rapidly on audio/AI processing logic. The stack choice determines the developer velocity, shipping timeline, and user experience baseline. The app must compile to Windows (x64/ARM64) and macOS (Intel/Apple Silicon) with minimal platform-specific code duplication and no vendor lock-in.

## Decision

**Tauri 2 shell + Rust core + React/TypeScript UI**

- **Rust core** (all audio/AI/IO): memory-safe DSP that agents can write with confidence; first-class crates for every need. DeepFilterNet's reference realtime implementation is itself Rust. Native compilation to win-x64/win-arm64/mac-x64/mac-arm64.
- **Tauri 2** (MIT/Apache): ~10 MB shell using OS webview (WebView2 on Windows, WKWebView on macOS) → small installers, no Chromium ship. Built-in sidecar support for ffmpeg; signed updater against GitHub Releases.
- **React 18 + Vite + Tailwind + Radix primitives, Zustand state**: the most-trodden path = fewest agent-generated UI bugs. No SSR, no router beyond simple view switch.

### Rejected alternatives

- **Electron**: 100 MB+ runtime, native-module pain across 4 targets
- **JUCE/C++**: right for a DAW; slower agentic iteration, dated consumer UI
- **Flutter**: audio FFI everywhere, weakest desktop file/drag-drop story
- **Pure native (×2 codebases)**: duplicate everything, PC-first→Mac port becomes a rewrite — violates the PC-first-without-a-porting-cliff constraint

## Consequences

**Enables:** Small installers; unified Rust core + platform abstraction; rapid iteration; C++/C libraries via -sys crates; audio playback sample-accuracy without webview limitations; deterministic batch processing via CLI

**Constrains:**
- Audio playback NEVER goes through the webview. Rust owns playback via `cpal` (WASAPI/CoreAudio); the UI is a remote control (play/seek/AB events over Tauri IPC; playhead position streamed back at 30 Hz). Enforces: sample-accurate A/B switching, no webview audio quirks.
- Waveforms: Rust computes min/max peak pyramids (like audiowaveform) → binary payload → UI renders to Canvas/WebGL. Never ship raw PCM to the webview.
- All heavy work in Rust worker threads (a job system with cancellation tokens + progress events). Tauri commands return immediately with a job id; `job://progress` events drive the UI. Nothing blocking on the IPC thread.
- Dependency audit: Rust crates must be MIT/Apache/BSD-compatible; no GPL. Enforced in CI (cargo-deny).
