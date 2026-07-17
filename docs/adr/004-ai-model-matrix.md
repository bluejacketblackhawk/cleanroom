# ADR-004: AI Model Matrix

- Status: Accepted (from the build handoff)
- Date: 2026-07-13
- Source: handoff/02-ARCHITECTURE.md § AI model matrix (ADR-004)

## Context

Cleanroom integrates specialized AI models for denoise, VAD, speech/music classification, audio enhancement, speech recognition, speaker diarization, and chapter/show-notes generation. Each task has multiple candidates differing in quality, latency, size, license, and compute device (CPU/GPU/NPU). A principled matrix defines what ships bundled vs. on-demand and which models are acceptable.

## Decision

| Task | Model | Runtime | Size | License | Ship |
|---|---|---|---|---|---|
| Denoise Standard | DeepFilterNet3 | `df` crate (tract, pure Rust) | ~5 MB | MIT/Apache-2.0 (weights too) | **bundled** |
| Denoise Fast | RNNoise (fallback) or GTCRN | rnnoise-c / ort | <1 MB | BSD / MIT | bundled |
| VAD | Silero VAD v5 | ort | ~2 MB | MIT | **bundled** |
| Speech/music classifier | small ONNX (train or adapt open model, e.g. YAMNet-distill) | ort | ~5 MB | verify | bundled |
| Studio Enhance (dereverb + BWE + separation sliders) | **bake-off in M4**: MossFormer2-SE/SR-48K (ClearerVoice, Apache-2.0) vs resemble-enhance (MIT) vs VoiceFixer (MIT) | ort (needs ONNX export spike) | 100–400 MB | per-model | model-pack download |
| ASR | whisper.cpp — large-v3-turbo (GPU), small/base (CPU); optional Parakeet-TDT via sherpa-onnx for fast English | whisper-rs / sherpa | 75 MB–1.6 GB | MIT (whisper) / CC-BY-4.0 (parakeet, attribute) | pack download (base bundled in "full" installer) |
| Diarization | sherpa-onnx pipeline: pyannote-segmentation-3.0 ONNX + WeSpeaker/3D-Speaker embeddings + clustering | sherpa-onnx (Apache-2.0) | ~30 MB | MIT model / Apache — verify redistribution | pack download |
| Shownotes LLM | Qwen2.5-7B-Instruct Q4_K_M (default), Qwen2.5-1.5B-Instruct (low-RAM) — **both Apache-2.0. Do NOT use Qwen2.5-3B (research license)** | llama.cpp | 4.7 GB / 1 GB | Apache-2.0 | pack download |

**Execution providers:**
- CPU everywhere (baseline, must be good)
- Windows GPU via **DirectML** EP (covers NVIDIA/AMD/Intel; verify redistribution license in 07)
- macOS via CoreML EP where it actually works, else CPU+Accelerate
- whisper.cpp uses its own Vulkan (Win) / Metal (Mac) backends
- Capability probe at startup → per-model device choice with automatic CPU fallback on first inference failure (never crash on a driver)

**Model packs:** `models.json` manifest (name, version, sha256, size, license, url→GitHub Release asset). Downloads user-initiated with size shown; resume support; hash-verified. Two installer flavors: **Standard** (~150 MB: DFN3, VAD, classifiers, RNNoise, ffmpeg) and **Studio Bundle** (~2–6 GB offline installer with whisper-small, enhance, diarization — true airplane-mode install).

## Consequences

**Enables:** Offline-first mastering for Standard workflow; optional Studio tier with best-in-class enhancement; fast fallback chains (RNNoise if GPU unavailable); model-pack downloads separate from core app; asymmetric costs (DFN3 cheap, Studio Enhance large)

**Constrains:**
- Enforce: **Never use Qwen2.5-3B or 72B** — research license, not Apache-2.0. Only 7B and 1.5B Instruct models.
- All models must be MIT/Apache-2.0 or equivalent MIT-redistribution-compatible license. Verify before bundling.
- Diarization and Studio Enhance models: redistribution status must be verified before M3; fallback plans documented if licensing is ambiguous.
- GPU execution providers (DirectML, CoreML) must have graceful CPU fallback; never crash on an unsupported or missing driver.
- Model files hash-verified before load; CI enforcement that model list is up-to-date and hashes match repo artifacts.
