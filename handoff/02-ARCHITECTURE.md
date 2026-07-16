# 02 — Architecture

## Stack decision (ADR-001)

**Tauri 2 shell + Rust core + React/TypeScript UI.**

- **Rust core** (all audio/AI/IO): memory-safe DSP that agents can write with confidence; first-class crates for every need (see matrix); compiles natively to win-x64/win-arm64/mac-x64/mac-arm64; DeepFilterNet's reference realtime implementation is itself Rust.
- **Tauri 2** (MIT/Apache): ~10 MB shell using OS webview (WebView2 on Windows, WKWebView on macOS) → small installers, no Chromium ship; sidecar support for ffmpeg; built-in updater signed against GitHub Releases.
- **React 18 + Vite + Tailwind + Radix primitives, Zustand state**: the most-trodden path = fewest agent-generated-UI bugs. No SSR, no router beyond a simple view switch.

Rejected: **Electron** (100 MB+ runtime, native-module pain × 4 targets), **JUCE/C++** (right for a DAW; slower agentic iteration, dated consumer UI), **Flutter** (audio FFI everywhere, weakest desktop file/drag-drop story), **pure native ×2** (duplicate everything, PC-first→Mac port becomes a rewrite — violates constraint 4).

### Non-obvious consequences (enforce these)

- **Audio playback NEVER goes through the webview.** The Rust side owns playback via `cpal` (WASAPI/CoreAudio); the UI is a remote control (play/seek/AB events over Tauri IPC; playhead position streamed back at 30 Hz). This gives sample-accurate A/B switching and kills a whole class of webview audio quirks.
- **Waveforms:** Rust computes min/max peak pyramids (like audiowaveform) → binary payload → UI renders to Canvas/WebGL. Never ship raw PCM to the webview.
- **All heavy work in Rust worker threads** (a job system with cancellation tokens + progress events). Tauri commands return immediately with a job id; `job://progress` events drive the UI. Nothing blocking on the IPC thread.

## Repo layout (monorepo)

```
anvil/
  apps/desktop/            # Tauri app: src-tauri (shell, commands, jobs) + src (React UI)
  crates/
    anvil-core/            # session/project model, processing graph, job system, analysis orchestration
    anvil-dsp/             # filters, leveler, limiter, loudness (ebur128), de-esser, resample (rubato)
    anvil-ai/              # model runtime: DFN3 (df/tract), ort sessions (VAD, enhance, diarize), device probe
    anvil-asr/             # whisper.cpp (whisper-rs), word timestamps, language detect
    anvil-llm/             # llama.cpp bindings, prompt templates, shownotes/chapters pipelines
    anvil-media/           # decode (symphonia + ffmpeg fallback), encode (ffmpeg sidecar + OS AAC), metadata (lofty), video remux
    anvil-project/         # .anvilproj format, presets, cut-lists (EDL), settings, migrations
    anvil-cli/             # `anvil` binary: analyze/master/transcribe/batch/watch — headless, JSON output
  eval/                    # Python eval harness + corpus manifests (dev-only, not shipped) — see 06
  installer/               # NSIS config, winget/scoop/brew manifests, signing scripts
  docs/                    # mkdocs site + adr/
  STATE.md
```

Rule: `apps/desktop` may depend on crates; crates never depend on Tauri. `anvil-cli` proves the core is UI-independent (and is the QA automation surface).

## Processing pipeline (ADR-002)

- **Internal format: f32, 48 kHz** (DFN3 native rate), interleaved-free (planar per channel). Original sample rate/bit depth preserved in metadata; output resampled as requested (rubato, windowed-sinc).
- **Streaming, chunked execution.** Fixed hop of 480 samples (10 ms) grouped into blocks (4800 = 100 ms) flowing through a pull-graph. A 3-hour file never fully resides in RAM (budget: <1.5 GB). Modules declare latency; the graph auto-compensates so A/B stays sample-aligned.
- **Two passes maximum.** Pass 1 = analysis (measurements, VAD, classifiers, loudness); Pass 2 = render with parameters frozen from pass 1 (this is how two-pass loudness normalization is exact). Preview uses the same graph in realtime mode with pass-1 data already cached.
- **Intermediate cache:** per-module output can be cached to disk (`%LOCALAPPDATA%/anvil/cache`, LRU-capped) so toggling one module re-renders only downstream stages.

## Determinism (ADR-003)

Same input + settings + version ⇒ bit-identical output, required for regression tests and sound-version pinning (feature #11):

1. Fixed block sizes and processing order; no data-dependent thread scheduling affecting the mix (parallelism only across independent tracks/files, never inside one stream's sample math — or use deterministic reduction).
2. No `fast-math`; pinned ONNX Runtime version; models addressed by SHA-256 in a manifest.
3. Any RNG (dither) seeded from content hash.
4. Project files record `{chain_version, model_hashes, params}`; the engine keeps old chain versions callable behind a version switch for at least one major release.
5. CI regression: golden corpus → output hashes compared per version (06).

## AI model matrix (ADR-004)

| Task | Model | Runtime | Size | License | Ship |
|---|---|---|---|---|---|
| Denoise Standard | DeepFilterNet3 | `df` crate (tract, pure Rust) | ~5 MB | MIT/Apache-2.0 (weights too) | **bundled** |
| Denoise Fast | RNNoise (fallback) or GTCRN | rnnoise-c / ort | <1 MB | BSD / MIT | bundled |
| VAD | Silero VAD v5 | ort | ~2 MB | MIT | **bundled** |
| Speech/music classifier | small ONNX (train or adapt open model, e.g. YAMNet-distill) | ort | ~5 MB | verify | bundled |
| Studio Enhance (dereverb + BWE + separation sliders) | **bake-off in M4**: MossFormer2-SE/SR-48K (ClearerVoice, Apache-2.0) vs resemble-enhance (MIT) vs VoiceFixer (MIT) | ort (needs ONNX export spike) | 100–400 MB | per-model | model-pack download |
| ASR | whisper.cpp — large-v3-turbo (GPU), small/base (CPU); optional Parakeet-TDT via sherpa-onnx for fast English | whisper-rs / sherpa | 75 MB–1.6 GB | MIT (whisper) / CC-BY-4.0 (parakeet, attribute) | pack download (base bundled in "full" installer) |
| Diarization | sherpa-onnx pipeline: pyannote-segmentation-3.0 ONNX + WeSpeaker/3D-Speaker embeddings + clustering | sherpa-onnx (Apache-2.0) | ~30 MB | MIT model / Apache — **verify redistribution, see 07** | pack download |
| Shownotes LLM | Qwen2.5-7B-Instruct Q4_K_M (default), Qwen2.5-1.5B-Instruct (low-RAM) — **both Apache-2.0. Do NOT use Qwen2.5-3B (research license)** | llama.cpp | 4.7 GB / 1 GB | Apache-2.0 | pack download |
| Eval only (not shipped) | DNSMOS P.835 ONNX | eval harness | — | MIT repo, verify model | dev-only |

**Execution providers:** CPU everywhere (baseline, must be good); Windows GPU via **DirectML** EP (covers NVIDIA/AMD/Intel; verify redistribution license, 07); macOS via CoreML EP where it actually works, else CPU+Accelerate; whisper.cpp uses its own Vulkan (Win) / Metal (Mac) backends. Capability probe at startup → per-model device choice with automatic CPU fallback on first inference failure (never crash on a driver).

**Model packs:** `models.json` manifest (name, version, sha256, size, license, url→GitHub Release asset). Downloads user-initiated with size shown; resume support; hash-verified. Two installer flavors: **Standard** (~150 MB: DFN3, VAD, classifiers, RNNoise, ffmpeg) and **Studio Bundle** (~2–6 GB offline installer with whisper-small, enhance, diarization — the true airplane-mode install).

## Media IO (ADR-005)

- **Decode:** symphonia (MPL-2.0, fine with MIT app) for wav/flac/mp3/aac/ogg/alac; **ffmpeg sidecar fallback** for everything else and all video containers.
- **Encode:** ffmpeg sidecar for MP3 (LAME), Opus, Vorbis, FLAC; **AAC via OS encoders** — Media Foundation (Win) / AudioToolbox (Mac) — sidesteps ffmpeg-AAC quality and patent-license questions; ffmpeg native AAC as last-resort fallback.
- **ffmpeg build: LGPL-only** (no GPL components, no libfdk), run as a **separate process** (sidecar), never linked — compliance = ship license text + exact build source/offer (07). Parse `-progress pipe:1` for job progress. Pin the build; hash-check at startup.
- **Video:** demux audio → process → **remux with `-c:v copy`** (never re-encode video). Container support target: mp4/mov/mkv/webm.
- **Metadata/chapters:** lofty (MIT/Apache) for ID3v2.3/2.4 (incl. CHAP), Vorbis comments, MP4 atoms; ffmpeg `-map_metadata`/ffmetadata for MP4 chapter atoms + M4B; cover art read/write. Round-trip rule: never drop tags the user didn't touch.

## Project & data model

- `.anvilproj` = a folder: `project.json` (schema-versioned), source file references (+content hashes), analysis cache, cut-list (EDL JSON), chapter/metadata docs, render history. Autosave every 30 s + on close; crash-safe via write-temp-then-rename.
- Presets = JSON documents (chain params + target + outputs), user dir + shipped defaults; importable/exportable (shareable files → community).
- Settings: `%APPDATA%/anvil/settings.json` (Win) / `~/Library/Application Support/anvil` (Mac).
- **Peaks pyramid file** (`.anvilpeaks`) computed on import, memory-mapped for instant waveform at any zoom.

## Platform layer (ADR-006 — the PC-first-without-a-porting-cliff rule)

All OS-specific code lives in `anvil-core::platform` behind one trait set: file dialogs/associations, tray, notifications, autostart (watch-folder agent), shell context menu (Win registry / Mac Services+Quick Action), taskbar/dock progress, OS AAC encoder. `#[cfg(windows)]`/`#[cfg(target_os="macos")]` only inside this module. **CI compiles and unit-tests macOS from M0** — any Windows-ism outside the platform module breaks the build the day it's written, not in M6.

Windows targets: Windows 10 1809+ x64 (AVX2 recommended, SSE4.2 minimum — runtime dispatch), ARM64 stretch goal (M5). macOS 12+ universal2 (arm64 + x86_64; Intel iMacs are a first-class QA target incl. 8 GB RAM machines).

## Privacy & security posture

- Zero network at runtime except: updater (Tauri signed-manifest against GitHub Releases, user-controllable) and model-pack downloads (user-initiated). Both fail silent-and-safe offline. No telemetry, no crash upload (crash dumps saved locally with a "copy to clipboard for a GitHub issue" button).
- Enforcement: `reqwest`/network deps allowed ONLY in the `updater` and `models` modules; CI check greps the dependency tree and fails otherwise. Document the guarantee in README ("verify with a firewall — here's what you'll see").
- Sidecars (ffmpeg) spawned with stdin/out pipes only. Model files hash-verified before load. No dynamic code download, no eval.

## CLI (ADR-007)

`anvil` binary, JSON-first output, stable exit codes — the automation surface and the QA harness's engine:

```
anvil analyze in.wav --json
anvil master in.wav --preset podcast-stereo -o out/ [--tier fast|standard|studio] [--report out/report.html]
anvil transcribe in.wav --model small --srt --vtt --diarize
anvil batch ./drop --preset show-a --watch
anvil models list|pull <pack>
```
