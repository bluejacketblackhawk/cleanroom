# Contributing to ANVIL

Thanks for helping build a free, private, local podcast mastering tool. This project is
pre-release; the foundation is still landing. The full spec lives in [`handoff/`](handoff/)
and architectural decisions in [`docs/adr/`](docs/adr/) — read those before a substantial PR.

## Non-negotiable constraints (every PR is checked against these)

1. **100% local.** No audio ever leaves the machine. Network access is allowed **only**
   in the `updater` and `models` modules; a CI hygiene job greps the dependency tree and
   fails if a network crate appears anywhere else. The app must pass the **airplane-mode
   test**: every processing feature works with networking disabled.
2. **Deterministic DSP.** Same input + same settings + same version ⇒ bit-identical output
   (ADR-003). No `fast-math`; models pinned by SHA-256.
3. **Eval-gated audio changes.** No PR that touches the DSP or AI chain merges without the
   eval harness run (see [`eval/`](eval/) and `handoff/06-QUALITY-EVAL.md`).
4. **MIT-compatible dependencies only.** Enforced by `cargo deny` and the npm license check.
   New bundled deps/models need an entry in `docs/licenses.md`.
5. **One-click wedge.** The default path stays: drop → Master → export. A change that adds
   a required decision to the default path is wrong.

## Repo layout

```
apps/desktop/   Tauri 2 app: src-tauri (Rust shell/commands/jobs) + src (React UI)
crates/         anvil-core, -dsp, -ai, -asr, -llm, -media, -project, -cli (portable Rust)
eval/           Python eval harness + corpus manifests (dev-only, never shipped)
installer/      NSIS / winget / scoop / brew manifests
docs/           mkdocs site + adr/ (architecture decision records)
```

Rule: `apps/desktop` may depend on crates; **crates never depend on Tauri.** All
OS-specific code lives behind traits in `anvil-core::platform` (`#[cfg(...)]` only there).

## Dev setup

- [Rust](https://rustup.rs) stable (the repo pins the toolchain via `rust-toolchain.toml`)
- [Node.js](https://nodejs.org) 20+
- Windows: MSVC C++ Build Tools + Windows SDK. macOS: Xcode Command Line Tools.

```sh
cargo build --workspace --exclude anvil-desktop   # core + CLI
cd apps/desktop && npm install && npm run tauri dev
```

## Before you push

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --exclude anvil-desktop
cd apps/desktop && npm run lint && npm run typecheck
```

CI runs these on Windows **and** macOS (compile+test) on every PR — a Windows-ism outside
`anvil-core::platform` breaks the macOS job the day it lands, not at port time.

## Commits & PRs

- Conventional-ish, imperative subject lines (`feat(dsp): …`, `fix(media): …`, `docs: …`).
- Keep PRs lane-scoped; never touch two crates' internals in one PR without reason.
- DSP/AI PRs must paste the relevant eval-subset numbers in the description.

## Reporting bugs

Crashes save a local diagnostics bundle with a "copy for GitHub issue" button — attach it.
We never auto-upload anything. A great bug report often becomes a new corpus clip.
