# 05 — Milestones & Execution Plan

Windows first (M0–M5), macOS port (M6), launch (M7). Estimates in **work-sessions** (one focused development session; ranges assume parallel work lanes). ∥ marks lanes that can run in parallel (worktree-isolated). Every milestone ends with: eval run green, STATE.md updated, demo script recorded as a checklist in the repo.

**Standing gates for every milestone:** CI green on Windows build+test AND macOS compile+test · clippy/fmt/eslint clean · no network calls outside updater/models modules (dep-tree check) · airplane-mode smoke test.

---

## M0 — Foundations (6–9 sessions)

Goal: app skeleton + media IO + playback + eval harness scaffold. No DSP yet.

| Lane | Tasks |
|---|---|
| A. Scaffold | git init, workspace, Tauri 2 app boots, CI matrix (06), STATE.md, ADR transcription (001–010 from 02), LICENSE/CONTRIBUTING/templates |
| B. Media IO ∥ | symphonia decode→48k f32 streams; ffmpeg sidecar manager (pinned LGPL build, hash check, progress parse); decode fallback; format zoo fixtures |
| C. Playback+waveform ∥ | cpal engine (WASAPI), transport, seek; peaks pyramid + Canvas waveform @60 fps; drop-to-waveform flow |
| D. Eval scaffold ∥ | `/eval` Python harness runner, corpus manifest format, metric stubs (LUFS via ffmpeg cross-check), begin corpus assembly (06 §1 — the 12 classes) |
| E. Project model ∥ | .anvilproj, settings, job system with cancellation+progress events |

**Exit:** drop wav/mp3/m4a/flac/mp4 → waveform < 5 s (1-h file) → plays, seeks, scrubs · `anvil analyze file --json` prints LUFS/TP matching `ffmpeg ebur128` within ±0.1 LU on 10 fixtures · 3-h file decodes streaming < 500 MB RAM · CI green both OSes.

## M1 — The Wedge (10–14 sessions) ← the product exists after this

Goal: one-click Master with Standard tier, A/B, export, Health Card. **Eval harness goes live BEFORE chain tuning.**

| Lane | Tasks |
|---|---|
| A. Analysis pass | 03 §1 complete + AnalysisReport + auto-decision table (03 §2) + unit fixtures |
| B. Chain v1 | DC/HPF → DFN3 (df crate) → leveler (03 §4.8 — budget the most listening time here) → two-pass loudness → TP limiter → dither; latency compensation; determinism harness |
| C. Eval live ∥ | DNSMOS + LUFS/TP conformance + regression hashes wired to corpus; thresholds from 06 §2; nightly CI job |
| D. UI ∥ | S2 Master tab: Health Card, Master button, module chips, before/after meters, A/B (sample-aligned), Export tab (WAV/MP3/FLAC first) |
| E. Preview | realtime graph mode for A/B + module toggle → downstream-only recompute (cache) |

**Exit:** Definition-of-AMAZING M1 rows: ≥5× RT Standard on 4-core · LUFS ±0.5 / TP never over on full corpus · DNSMOS uplift thresholds met (06) · blind listen: ≥ parity with Auphonic core processing on 6/10 classes (protocol 06 §3) · one-click path = 3 interactions on demo file · A/B < 50 ms · airplane test.

## M2 — Output Excellence & Scale (8–11 sessions)

Goal: everything around the master — formats, metadata, batch, watch, reports, CLI. Heavy ∥.

Lanes: **A.** encoders (MP3/Opus/Vorbis/FLAC/ALAC via sidecar; AAC via Media Foundation; simultaneous outputs; M4B) ∥ **B.** metadata/chapters editor + round-trip (lofty + mp4 chapters) ∥ **C.** video demux/remux (`-c:v copy`) ∥ **D.** batch queue + back-catalog mode + soak rig ∥ **E.** watch folders + tray agent + autostart ∥ **F.** presets manager + shipped presets (incl. ACX v1) ∥ **G.** compliance report (HTML/PDF, self-consistency test) ∥ **H.** CLI complete + docs recipes ∥ **I.** shell context menu (Win platform module).

**Exit:** 04 acceptance rows for batch/watch/metadata/report/CLI/shell all green · 100-file soak zero-crash · back-catalog demo (50 files overnight → consistent −16 ±0.5) · tag round-trip diff = zero loss.

## M3 — Speech Intelligence (10–14 sessions)

Goal: transcription, cutting, diarization, voice polish modules.

Lanes: **A.** whisper.cpp integration (model manager, word timestamps, language auto) ∥ **B.** transcript UI + exports ∥ **C.** silence/filler detection + cut-list engine + review UI (03 §5, EDL golden tests) ∥ **D.** diarization (sherpa-onnx) + per-speaker leveling + speaker names ∥ **E.** de-ess + AutoEQ + breath control + de-hum + mouth de-click (03 §4.2–4.7) ∥ **F.** models manager UI + pack downloads.

**Exit:** WER sanity vs whisper reference implementation (≤ +1% absolute) · filler P/R gates (≥90/≥70) · cut-point artifact metric clean + 20-point listen · diarization DER ≤ 20% on AMI subset · de-ess/AutoEQ pass blind "no worse, usually better" panel · per-speaker leveling fixes quiet-guest fixture (Δmedian ≤ 1 LU post).

## M4 — Studio Tier & Intelligence (12–16 sessions) ← Adobe-parity + differentiators

Lanes: **A.** Studio enhance bake-off (03 §4.4: candidates, ONNX export spike, DirectML/CPU benchmarks, pick + integrate; speech/noise/music sliders) — the hardest lane, start first ∥ **B.** de-clip + de-crackle ∥ **C.** multitrack: alignment, crossgate, ducking, per-track chains, mixdown + S3 UI (03 §6) ∥ **D.** local LLM: llama.cpp, Qwen2.5-7B pack, shownotes/chapters/titles prompts + eval rubric, chapter suggestions (topic-drift fallback without LLM) ∥ **E.** Clip Studio ∥ **F.** Recording Guard ∥ **G.** Voice Memory (per-show speaker profiles feeding AutoEQ/leveler) ∥ **H.** ACX full check + conform.

**Exit:** Studio tier ≥ parity with Adobe Enhance blind on 6/10 degraded classes; RTF ≥1× on mid GPU, ≥0.3× CPU with honest warning UI · multitrack demo: 3-track double-ender → aligned, bleed-gated, ducked, mastered · shownotes rated usable-without-major-edit ≥80% on 20-episode rubric · clip renders with karaoke captions · airplane test still passes with all packs installed.

## M5 — Windows Ship (7–10 sessions)

Lanes: **A.** NSIS installer + portable zip + file associations + uninstall hygiene ∥ **B.** signing (07 decision: SignPath OSS or Azure Trusted Signing) + Tauri updater against GH Releases ∥ **C.** onboarding + demo file + docs site v1 ∥ **D.** perf hardening vs 06 budgets (startup, memory, GPU probe robustness) ∥ **E.** accessibility pass ∥ **F.** crash-recovery + logs/diagnostics ∥ **G.** `/security-review` on the whole repo + fix findings ∥ **H.** win-arm64 build (stretch — cut without guilt) ∥ **I.** private beta via GH pre-release (10–20 podcasters), triage.

**Exit:** release checklist 06 §6 fully green → tag `v1.0.0-win`. Fresh-VM install test (Win10 1809 + Win11): install → master → uninstall leaves no residue. SmartScreen story decided and documented.

## M6 — macOS Port (8–12 sessions; needs a Mac or MacStadium/CI runner + Apple ID decision from owner)

Lanes: **A.** universal2 build (arm64+x86_64), CI release lane ∥ **B.** platform module: AudioToolbox AAC/ALAC, CoreAudio via cpal, Finder Quick Action, menu-bar tray, dock progress, associations ∥ **C.** GPU: whisper Metal, ort CoreML-with-CPU-fallback benchmarks (Intel Macs: CPU path perf validation on 8 GB) ∥ **D.** DMG + notarization (needs $99 Apple dev — owner decision; else right-click-open docs) + updater ∥ **E.** mac QA sweep: full corpus + acceptance rows re-run on arm64 AND x64 ∥ **F.** Homebrew cask.

**Exit:** same eval scores as Windows (±measurement noise) on both mac archs · Intel iMac 8 GB: Standard tier ≥ 3× RT · all 04 acceptance rows green on mac → `v1.0.0`.

## M7 — Launch (4–6 sessions, can overlap M5/M6 tails)

Final name + trademark screen + rename sweep (owner decision) · README hero (airplane GIF), demo video script + capture · benchmark table vs Auphonic/Adobe on public corpus (honest, reproducible via `eval/`) · docs complete · winget/scoop/brew manifests merged · Show HN + Product Hunt + subreddit posts drafted for owner review · GitHub Discussions seeded (FAQ) · issue-triage playbook · roadmap board (post-1.0: Linux, REST daemon, publishing, episode assembly, translations).

**Exit:** owner clicks "publish release." Everything else already green.

---

## Sequencing notes

- Critical path: M0.B/C → M1.B → M1 exit → M4.A. Start M4.A's model bake-off spike EARLY (a research spike can pre-test ONNX exports during M2/M3 idle capacity — it's the riskiest unknown in the plan).
- M2 and M3 can interleave if lanes stay staffed; don't start M4.A until M1's eval harness is trusted (it's the judge).
- Corpus assembly (M0.D) is long-lead human-ish work — recruit the owner early for "real bad recordings" (07 §open questions).
- Cut list if schedule slips (in order): win-arm64 → Recording Guard → Clip Studio → Voice Memory. Never cut: eval gates, determinism, A/B, the 3-interaction path.
