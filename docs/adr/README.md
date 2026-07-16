# Architecture Decision Records (ADR)

This directory contains architecture decision records for ANVIL (codename for the local podcast mastering app). Each ADR documents a major decision, its context, the decision made, and its consequences.

## ADRs

| # | Title | Summary |
|---|-------|---------|
| [001](001-stack.md) | Stack Decision | Tauri 2 + Rust core + React/TypeScript UI; audio playback off the webview |
| [002](002-processing-pipeline.md) | Processing Pipeline | f32/48kHz streaming, 480-sample hop, two passes, intermediate cache |
| [003](003-determinism.md) | Determinism | Same input + settings + version ⇒ bit-identical output (required for regression tests and version pinning) |
| [004](004-ai-model-matrix.md) | AI Model Matrix | Bundled models (DFN3, VAD, RNNoise); optional packs (ASR, enhance, diarization, LLM) |
| [005](005-media-io.md) | Media IO | symphonia + ffmpeg sidecar (decode); OS AAC + ffmpeg (encode); lofty for metadata |
| [006](006-platform-layer.md) | Platform Layer | PC-first-without-a-porting-cliff: all OS code in `anvil-core::platform` trait; CI compiles macOS from M0 |
| [007](007-cli.md) | CLI | `anvil` binary, JSON-first output, stable exit codes; analyze/master/transcribe/batch/models surface |
| [008](008-project-data-model.md) | Project & Data Model | `.anvilproj` folder format, schema-versioned `project.json`, cached peaks, presets as JSON |
| [009](009-privacy-security.md) | Privacy & Security | Zero network (updater/models only, both fail-safe offline); no telemetry; model hash verification |
| [010](010-process-model.md) | Process Model | Playback and heavy work off webview; audio via cpal, waveforms as peak pyramids, jobs with cancellation |
| [011](011-windows-packaging.md) | Windows Packaging, Updater & Uninstall Hygiene | NSIS (currentUser) + portable zip; `--uninstall-cleanup` unregisters shell integration; updater wired with owner-TODO endpoint/pubkey; signing method deferred to owner (07 §3) |
| [012](012-mac-packaging.md) | macOS Packaging (per-arch `.app`/`.dmg`) | Per-arch (not universal2) for the 6-artifact ship; sidecars staged to `../Resources/` matching `locate()`; exec-bit re-assert + `verify-mac-bundle.mjs` gate; one shared onnxruntime dylib serves sherpa + Intel in-process ort; unsigned (right-click-open) until Apple Dev owner decision |

## How to add a new ADR

New ADRs get the next number (e.g., if 010 is the last, the next is 011). Follow the template:

```markdown
# ADR-0NN: <Title>

- Status: Accepted (or Proposed/Rejected/Superseded)
- Date: YYYY-MM-DD
- Source: [reference to requirement or prior discussion]

## Context
[2–5 sentences on the problem and forces]

## Decision
[The decision and concrete specifics]

## Consequences
[What it enables and constrains; enforcement rules]
```

Commit new ADRs to the same directory; link them in this README.
