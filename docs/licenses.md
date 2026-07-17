# Cleanroom License Compliance

Cleanroom is released under the **MIT License**. Every bundled dependency and model must be MIT-redistribution-compatible (MIT, Apache-2.0, BSD-3, or equivalent permissive license with no GPLv2+ viral clauses).

**CI Enforcement:** Dependency licenses are verified automatically:
- Rust: `cargo-deny` checks `Cargo.lock` and enforced in CI (see `deny.toml`)
- npm: `license-checker` scans `package-lock.json` and enforced in CI
- Any non-compliant or unverified dependency blocks the build

Items marked ☐ are open verification tasks to resolve before first public release. Findings are recorded below.

## Dependency License Matrix

| Component | License | Status |
|---|---|---|
| **Frontend & Framework** | | |
| React, Radix, Tailwind, Zustand, Vite | MIT/Apache-2.0 | ✔ safe |
| **Audio/DSP Libraries** | | |
| cpal | MIT/Apache-2.0 | ✔ safe |
| rubato | MIT | ✔ safe |
| lofty | MIT/Apache-2.0 | ✔ safe |
| **Model Runtime** | | |
| ort (ONNX Runtime) | Apache-2.0 | ✔ safe |
| whisper-rs | MIT | ✔ safe |
| llama.cpp | MIT | ✔ safe |
| whisper.cpp | MIT | ✔ safe |
| **Decode/Encode** | | |
| symphonia | MPL-2.0 | ✔ ok in MIT app (file-level copyleft; don't modify, or upstream mods) |
| **Model Weights** | | |
| DeepFilterNet3 (code + weights) | MIT/Apache-2.0 dual | ✔ load-bearing model — pin commit, re-verify at pin |
| RNNoise | BSD-3 | ✔ safe |
| Silero VAD v5 | MIT | ☐ confirm v5 model file license = repo license |
| GTCRN | MIT | ☐ confirm weights license |
| Whisper (large-v3-turbo, small, base) | MIT | ✔ safe |
| Parakeet-TDT (optional ASR pack) | CC-BY-4.0 | ✔ safe with attribution in About + docs (☐ wording TBD) |
| sherpa-onnx | Apache-2.0 | ✔ safe |
| pyannote-segmentation-3.0 ONNX | MIT per model card (but HF-gated download) | ☐ **redistribution question — must resolve before M3.** If not cleanly redistributable, fallback: sherpa-onnx converted assets, 3D-Speaker/WeSpeaker segmentation (Apache), or train/adapt open model. Diarization must not silently become gated/cloud dependency. |
| Qwen2.5-7B-Instruct & 1.5B-Instruct | Apache-2.0 | ✔ safe (**never 3B or 72B — different, non-compatible licenses**; pin exact repos) |
| MossFormer2-SE/SR-48K (ClearerVoice) | Apache-2.0 | ☐ verify weights license matches repo license; ONNX export legality confirmed |
| resemble-enhance | MIT | ☐ verify weights license |
| VoiceFixer | MIT | ☐ verify weights license |
| **Platform & Tooling** | | |
| Tauri | MIT/Apache-2.0 | ✔ safe |
| ffmpeg sidecar build (LGPL-only) | LGPL-2.1+ | ☐ see compliance recipe in 07-RISKS-LEGAL §2; build must be LGPL-only (no GPL, no libfdk); run as separate sidecar process; ship license text + exact configure line + source pointer fork |
| DirectML redistributable | Microsoft proprietary-but-redistributable | ☐ **verify OSS bundling terms; fallback = ort CPU + whisper Vulkan if redist terms are awkward** |
| LAME (in ffmpeg) | LGPL | ✔ safe via sidecar (never direct link) |
| OpenH264 (Cisco, optional for Clip Studio H.264 encode) | BSD + patent-covered binary | ☐ decide in M4.E: OpenH264 vs VP9/webm vs fall back to OS encoders; never bundle GPL x264 |

## Notes

### symphonia (MPL-2.0)

symphonia is file-level copyleft under the Mozilla Public License 2.0. This is compatible with MIT redistribution: if you use symphonia unmodified, you can bundle it in an MIT-licensed app. **Rule:** Do not modify symphonia code, or any modification must be upstreamed. If you must fork, the fork becomes MPL-2.0 and cannot be re-licensed to MIT.

### Verification Tasks (Before M0 Release)

- ☐ **Silero VAD v5:** Confirm model file license is MIT (or equivalent) — currently only repo is verified.
- ☐ **GTCRN weights:** Confirm weights have the same license as the model code (MIT).
- ☐ **pyannote-segmentation-3.0:** Resolve HuggingFace gating and redistribution licensing. If gated to academic use or cannot be redistributed, document fallback plan (sherpa-onnx or 3D-Speaker route).
- ☐ **MossFormer2-SE, resemble-enhance, VoiceFixer:** Verify weights licenses (currently only code is MIT-tagged).
- ☐ **DirectML:** Confirm Microsoft's redistributable license allows bundling in open-source MIT apps.
- ☐ **Parakeet-TDT:** Draft attribution wording for About dialog and docs.
- ☐ **ffmpeg sidecar:** Prepare LGPL compliance package (build instructions, configure line, source tarball fork, license text).

## CI Enforcement

Every commit runs:
- `cargo-deny check` → blocks any non-allowlisted Rust dependency
- `npm license-checker` → blocks non-compliant npm packages
- Manual spot-check: model licenses match `docs/licenses.md` (code review checklist)
