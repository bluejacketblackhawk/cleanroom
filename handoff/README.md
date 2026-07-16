# PROJECT ANVIL — Build Handoff

**One-liner:** A free, open-source (MIT), 100% local desktop app that makes any podcast recording sound professionally mastered in one click — loudness normalization + AI denoise + adaptive leveling — with zero cloud, zero credits, zero subscription. The Auphonic / Adobe Podcast alternative that runs on *your* machine.

**Status:** Scoping complete. Nothing built. Empty repo. This folder is the complete specification.

**Codename:** ANVIL (internal only — final product name is an open decision, see [07-RISKS-LEGAL.md](07-RISKS-LEGAL.md) §Naming. Do NOT name it "Speechify" — that is an existing trademarked TTS company, despite this folder's name).

---

## Who executes this handoff

This handoff is written for a developer, or a small team working in parallel lanes, building on this Windows machine (`C:\Users\nbaro\speechify`).

**How the work splits by task type:**
- **Core/DSP work** — DSP algorithm implementation, audio engine architecture, AI model integration, anything where a subtle bug produces silent quality degradation.
- **Features work** — UI features, encoders/metadata, batch/queue plumbing, tests, eval harness code, CLI, installers.
- **Docs work** — docs, license files, changelogs, issue templates, mechanical refactors, translations of finished copy.

## Reading order

| File | What it contains | Read before |
|---|---|---|
| [01-PRODUCT.md](01-PRODUCT.md) | Vision, wedge, competitor teardown, full feature-parity matrix, new differentiator features, non-goals, launch plan | everything |
| [02-ARCHITECTURE.md](02-ARCHITECTURE.md) | Stack decision + rationale, repo layout, process model, AI model matrix, ffmpeg strategy, determinism rules, portability rules | any code |
| [03-DSP-SPEC.md](03-DSP-SPEC.md) | The audio science: analysis pass, processing chain module-by-module, multitrack, quality tiers, edge cases | M1 |
| [04-FEATURES-UX.md](04-FEATURES-UX.md) | Screen-by-screen UX spec, microcopy, acceptance criteria per feature, accessibility | M1 UI work |
| [05-MILESTONES.md](05-MILESTONES.md) | M0→M7 phased plan, task lanes with work assignments, exit criteria, demo scripts | starting any milestone |
| [06-QUALITY-EVAL.md](06-QUALITY-EVAL.md) | Golden corpus, objective metrics, perf budgets, CI matrix, release checklists | M0 (eval harness is built FIRST) |
| [07-RISKS-LEGAL.md](07-RISKS-LEGAL.md) | License table with verify-tasks, signing strategy, model redistribution, technical risks, open questions for the owner | M0, and again before each release |

## Non-negotiable constraints (from the product owner)

1. **Free. MIT-licensed. Open-source GitHub release. Non-commercial.** No license keys, no trials, no payments, no telemetry. Every dependency and bundled model must be compatible with MIT redistribution (see license table in 07).
2. **100% local processing.** Audio never leaves the machine. No cloud API calls, ever. The only network operations permitted: (a) optional update check against GitHub Releases, (b) optional model-pack downloads from GitHub Releases — both user-initiated/consented, both with the app remaining fully functional offline using bundled models. The app must pass an "airplane-mode test": every processing feature works with networking disabled.
3. **One-click wedge.** The default path is: drop file → press **Master** → export. Everything else is progressive disclosure. If a change adds a required decision to the default path, it's wrong.
4. **PC first.** Milestones M0–M5 are Windows (x64 primary, ARM64 stretch). macOS (Apple Silicon + Intel x64 — "the iMacs/x64s" — via universal2 binary) is M6, ported after Windows ships. **But:** the core is written portable from day 0 — macOS compile + unit tests run in CI from M0 so the port is a packaging exercise, not a rewrite. No Win32-specific code outside `#[cfg(windows)]` platform modules.
5. **Feature-complete vs Auphonic AND Adobe Podcast** (parity matrix in 01) **plus** the new differentiators. "Keep all the features" is a literal instruction — every parity-matrix row must land in a milestone or have a written justification in 07.
6. **Polished.** The quality bar is a commercial product that happens to be free, not a GitHub science project. See "Definition of AMAZING" below.
7. **Deterministic DSP.** Same input + same settings + same version → bit-identical output. This is what makes regression testing (and trust) possible. Rules in 02 §Determinism.
8. **Eval-gated audio changes.** No PR that touches the DSP or AI chain merges without the eval harness run (06). The harness is built in M0–M1 *before* the chain is tuned.

## Build workflow

- **State ledger:** Maintain `STATE.md` at repo root — current milestone, lane statuses, last eval scores, open blockers. Update it at the end of every session. It is the re-entry point for any future session (context will not survive; STATE.md must).
- **Decisions:** Architectural decisions go in `docs/adr/NNN-title.md` (one page each). The decisions already made in this handoff are ADRs 001–010 — transcribe them from 02 in M0.
- **Parallelism:** Within a milestone, lanes marked ∥ in 05 are independent — run them as parallel lanes with worktree isolation. Never run two lanes on the same crate at once.
- **Verification gates:** Every task's exit criteria in 05 are commands + expected results. A task is done when the gate passes, not when the code compiles. DSP tasks additionally require an eval-subset run.
- **Lane briefs:** Give each lane the relevant handoff file section verbatim, not a paraphrase. These specs are the source of truth.
- **When the spec is silent:** prefer (in order): the wedge (rule 3) → Auphonic's observed behavior → EBU/AES conventions → ask the owner in STATE.md's "Questions" section rather than blocking.

## First session script (M0 kickoff)

1. Read this README + 01 + 02 fully. Skim 05/06.
2. `git init`, commit this `handoff/` folder as the first commit.
3. Create repo skeleton per 02 §Repo layout; root `LICENSE` (MIT), `README.md` (stub from 01's one-liner), `STATE.md`, `docs/adr/`.
4. Scaffold Tauri 2 app + Rust workspace; verify `cargo build` + app window opens on Windows.
5. Set up GitHub Actions CI per 06 §CI matrix (Windows build+test, **macOS compile+test from day 0**, clippy/fmt, TS lint).
6. Start M0 lanes from 05.
7. End session: update STATE.md, commit.

## Definition of AMAZING (the polish bar)

Measured, not vibes — full budgets in 06:

- Cold start < 2 s; drop-a-1-hour-file → waveform visible + playable < 5 s.
- Default one-click path: **3 interactions** total (drop, Master, Export) — nothing else required.
- Mastering (Standard tier, no ASR) ≥ 5× realtime on a 4-core 2019 laptop CPU; a 1-hour episode masters in ≤ 12 min worst case, typically ~4 min.
- Blind listening: ≥ parity with Auphonic on 8/10 corpus classes; Studio tier ≥ parity with Adobe Enhance on 6/10 degraded-audio classes (protocol in 06).
- Loudness accuracy: integrated LUFS within ±0.5 LU of target; true peak never exceeds ceiling. Ever.
- Zero crashes in the 100-file batch soak; a 3-hour episode processes in < 1.5 GB RAM (streaming, never whole-file-in-RAM).
- UI: 60 fps waveform scrolling/zoom; every processing step cancellable; no beach-balls > 100 ms on the UI thread.
- The airplane-mode test passes on every release.
- A first-time podcaster can go from install → mastered episode without reading anything.
