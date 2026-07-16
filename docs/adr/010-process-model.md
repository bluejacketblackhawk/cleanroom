# ADR-010: Process Model — Playback and Heavy Work Off the Webview

- Status: Accepted (from the build handoff)
- Date: 2026-07-13
- Source: handoff/02-ARCHITECTURE.md § Non-obvious consequences

## Context

Audio applications require sample-accurate playback, real-time DSP, and responsive waveform visualization. Webview (JavaScript + WebGL) is not reliable for these tasks: audio APIs are asynchronous and latency-prone, waveform data scales poorly (raw PCM → browser memory), and long operations block the UI. ANVIL must separate concerns: Rust owns realtime audio and heavy work; the UI is a remote control.

## Decision

**Audio playback owned by Rust.** The UI is a remote control:
- Playback implemented via `cpal` (WASAPI on Windows, CoreAudio on macOS)
- Play/seek/AB events sent to Rust over Tauri IPC (non-blocking, message-based)
- Playhead position streamed back to UI at 30 Hz for waveform scrubber update
- Results: sample-accurate A/B switching, no webview audio quirks, realtime responsiveness

**Waveforms computed as min/max peak pyramids in Rust:**
- Rust computes (like audiowaveform) min/max peaks at multiple zoom levels
- Peaks encoded as binary payload, streamed to UI
- UI renders to Canvas/WebGL
- Consequence: instant zoom at any level, O(n log n) space, never ship raw PCM to webview

**All heavy work in Rust worker threads:**
- Job system with cancellation tokens + progress events
- Tauri commands return immediately with a job id
- `job://progress` events (rendered on a progress bar) notify UI of completion
- Consequence: UI never blocked, long renders feel responsive, user can cancel anytime

## Consequences

**Enables:** Sample-accurate A/B switching; responsive UI during long renders; memory-efficient waveforms at scale; deterministic batch processing (no async race conditions); real-time playback quality

**Constrains:**
- Enforce: Any audio playback MUST go through Rust playback service, never the webview. Feature review must flag any attempt to play audio in JS.
- Waveforms as data, not raw audio. Peak pyramids must be computed for every loaded file; cache them in `.anvilpeaks`.
- Tauri IPC is the UI-to-Rust boundary; all heavy operations (encode, AI inference, loudness measurement) must be jobs with progress streaming, not blocking commands.
- UI must handle job cancellation gracefully (model inference interrupted mid-pass, render stopped mid-file).
