# 07 — Risks, Legal & Open Questions

App license: **MIT** (owner decision, fixed). Everything bundled must be MIT-redistribution-compatible. `cargo-deny` + npm license-checker allowlists enforce in CI (06 §5).

## 1. License table (☐ = verify exact text/version before first public release — findings recorded in `docs/licenses.md`)

| Component | License | Status |
|---|---|---|
| Tauri, React, Radix, Tailwind, Zustand, Vite | MIT/Apache | ✔ safe |
| cpal, rubato, lofty, ort, whisper-rs, llama.cpp, whisper.cpp, ONNX Runtime | Apache/MIT | ✔ safe |
| symphonia | MPL-2.0 | ✔ ok in MIT app (file-level copyleft; don't modify, or upstream mods) ☐ note in licenses.md |
| **DeepFilterNet code + weights** | MIT/Apache dual (verified Jul 2026, incl. weights) | ✔ the load-bearing one — pin commit ☐ re-verify at pin |
| RNNoise | BSD-3 | ✔ |
| Silero VAD v5 | MIT | ☐ confirm v5 model file license = repo license |
| GTCRN | MIT | ☐ confirm weights |
| Whisper weights | MIT | ✔ |
| Parakeet-TDT (optional pack) | CC-BY-4.0 | ✔ with attribution in About + docs ☐ wording |
| sherpa-onnx | Apache-2.0 | ✔ |
| pyannote segmentation-3.0 ONNX | MIT per model card but HF-gated download | ☐ **redistribution question — resolve before M3.** Fallbacks if not cleanly redistributable: sherpa's converted assets (check their license note), 3D-Speaker/WeSpeaker segmentation route (Apache), or train/adapt an open segmentation model. Diarization must not silently become a cloud/gated dependency. |
| Qwen2.5-7B & 1.5B Instruct | Apache-2.0 | ✔ **never 3B/72B (different licenses)** ☐ pin exact repos |
| MossFormer2-SE/SR (ClearerVoice) | Apache-2.0 | ☐ verify weights license = repo; ONNX export legality fine |
| resemble-enhance / VoiceFixer | MIT | ☐ verify weights |
| ffmpeg sidecar | **LGPL-2.1+ build** | ☐ see §2 |
| DirectML redistributable | Microsoft proprietary-but-redistributable | ☐ **verify redist terms for OSS bundling; fallback = ort CPU + whisper Vulkan (open) if terms are awkward** |
| LAME (in ffmpeg) | LGPL | ✔ via sidecar |
| DNSMOS models, PESQ impl, datasets (VoiceBank-DEMAND, DNS, LibriSpeech, MUSAN, AMI) | various | ✔ eval-only, never redistributed in app; ☐ mark non-redistributable clips in corpus manifest |

## 2. ffmpeg compliance recipe (M0 lane B)

Use an **LGPL-only build** (no `--enable-gpl` components: no x264/x265/etc., no libfdk). Run strictly as a **separate sidecar process** (never link). Ship: its license text, exact configure line, and a pinned source pointer (fork the source tarball to our GH org = permanent source offer). Startup hash-check; document build reproduction in `docs/ffmpeg.md`. AAC **encode** via OS encoders (Media Foundation / AudioToolbox) as primary — quality + zero patent-license surface for us; ffmpeg native AAC only as fallback. Clip Studio H.264 encode: OS encoders (Media Foundation H.264 on Win, VideoToolbox on Mac); if unavailable, fall back to **OpenH264 (Cisco, BSD + their patent-covered binary)** ☐ or VP9/webm — decide in M4.E; never bundle GPL x264.

## 3. Code signing & distribution (owner decisions needed, see §6)

- **Windows:** unsigned = SmartScreen scare = death for a polished free app. Options: **SignPath.io free OSS signing** (recommended, ☐ apply early — approval takes time) or Azure Trusted Signing (~$10/mo). Decide by M5.B.
- **macOS:** notarization requires Apple Developer Program (**$99/yr — the project's only unavoidable cost** if we want normal double-click install). Without it: right-click-open instructions (hurts "polished"). Recommend paying. Decide by M6.D.
- Tauri updater keys: generate offline, store with owner (password manager), CI signs manifests only — document key ceremony in `docs/release.md`.

## 4. Naming & trademark

- Folder is `speechify` — **Speechify is an existing TTS company; cannot be the product name.** Codename Cleanroom internally.
- M7 task: candidate list → knockout search (USPTO TESS quick screen, GitHub/npm/domain availability, podcast-tool name collisions). Candidates to seed: Mainroom, ClearTake, Loudline, Masterly, StudioZero, Hearth Audio, LocalCast. Non-commercial OSS lowers (not eliminates) trademark risk — avoid anything confusable with Sound Forge, Auphonic, Adobe, Descript.
- Public copy never says "X killer"; benchmarks presented reproducibly (eval repo) and respectfully.

## 5. Technical risks & mitigations

| Risk | Mitigation |
|---|---|
| Studio-tier model bake-off finds no ONNX-exportable Adobe-quality model | Start spike early (M2/M3 idle, per 05 sequencing). Fallbacks: (a) chain DFN3 + separate dereverb net + SR net instead of one monolith; (b) ship Studio v1 = "max repair" without BWE, add BWE in 1.1. The wedge (M1) never depended on it. |
| ort CoreML EP flaky on dynamic shapes (Intel Macs esp.) | Benchmarked fallback chain: CoreML → CPU w/ Accelerate; whisper uses its own Metal path; budget floor set for CPU-only Intel iMac (06 §4). |
| DirectML redist terms awkward for MIT bundle | §1 fallback: CPU EP + Vulkan whisper; GPU enhance becomes "bring-your-own onnxruntime-dml" power-user doc. Decide at ☐ verify. |
| WebView2 missing/old on Win10 | Tauri bootstrapper installs Evergreen runtime; offline installer flavor bundles it. Test on fresh VM (06 §6). |
| Long-file memory blowups | Streaming architecture is load-bearing (02); 3-h fixture in CI soak with RAM assert. |
| whisper hallucination on silence/music | VAD-chunked ASR (skip non-speech), temperature fallback off, hallucination heuristics (repeat n-gram suppression) — known-issues doc for transcripts. |
| GPU driver zoo crashes | First-inference canary per device at startup; any failure → CPU for the session + Settings notice; never crash on a driver. |
| Leveler artifacts (pumping) = product-killer reputation | It gets the most eval attention: dedicated corpus class, blind-listen gate every release, dynamics-preservation param, conservative defaults. |
| Cut/crossfade artifacts in filler removal | Golden EDL render tests + spectral-discontinuity gate (06 §2) + per-instance review UX (never silent auto-apply by default). |
| Auphonic/Adobe ship something big mid-build | Quarterly competitive re-check task in STATE.md; parity matrix is versioned — additions become roadmap items, not scope creep mid-milestone. |
| Corpus too weak → overfit tuning | Grow corpus from every beta complaint (06 §7); synthetic degradation generator keeps paired refs honest. |

## 6. Open questions for the owner (answer in STATE.md when ready; none block M0–M4)

1. **Final product name** (needed M7; candidates in §4).
2. **Apple Developer $99/yr** for notarization — yes/no (needed M6).
3. **Windows signing**: OK to apply for SignPath OSS cert under your GitHub org? (needed M5; apply during M2–M3 — lead time.)
4. **Mac hardware access** for M6 QA (which machines do you have? Intel iMac RAM?). CI covers builds; final QA needs real ears on real hardware.
5. **GitHub org/repo name** to create (also affects updater endpoints).
6. Can you record/collect **real bad recordings** for the corpus (your voice, your rooms, Zoom calls with consenting friends)? Biggest quality lever available.
7. **win-arm64**: care or cut? (Stretch in M5.)
8. Post-1.0 priority vote: Linux vs REST daemon vs publishing integrations vs episode assembly.
