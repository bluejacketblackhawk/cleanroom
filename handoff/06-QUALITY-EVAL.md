# 06 — Quality, Eval & Release Engineering

The eval harness is the referee for every DSP/AI decision. Built in M0–M1 **before** chain tuning. Lives in `/eval` (Python allowed — dev-only, never shipped).

## 1. Golden corpus (~45 clips, 30 s–5 min + 3 long-form)

12 failure classes × 3–4 clips: (1) clean studio (must-not-degrade control) · (2) untreated-room echo · (3) constant broadband noise (fan/AC) · (4) dynamic noise (traffic, keyboard, dog) · (5) 50 Hz and 60 Hz hum · (6) quiet-guest/loud-host level gaps · (7) music+speech (intro/bed) · (8) clipped · (9) bandwidth-limited (Zoom/phone 8–16 k) · (10) multitrack with bleed (double-ender pair) · (11) non-English (de, es, ja minimum) · (12) laptop-mic + reverb + noise combined ("worst case"). Plus: 1-h and 3-h real episodes (perf/memory), silence-only, music-only fixtures.

Sources: record fresh (owner + agents can synthesize degradations: convolve clean speech with RIRs, add MUSAN noise at known SNRs — this also gives **paired references** for intrusive metrics), plus VoiceBank-DEMAND and DNS-Challenge test sets (eval-only, not redistributed), LibriSpeech test-clean/other (WER), AMI corpus subset (diarization/multitrack). Manifest per clip: class, license, ground truth (reference wav / transcript / RTTM / labeled fillers / labeled cuts). Corpus itself: keep private clips out of repo; publish the synthetic/redistributable subset as `anvil-bench` for the honest launch benchmark table.

## 2. Objective metrics & gates (CI-enforced)

| Metric | Tool | Gate (Standard tier unless noted) |
|---|---|---|
| Loudness accuracy | ebur128 self-measure + ffmpeg cross-check | integrated within ±0.5 LU of target, 100% of corpus |
| True peak | 4× oversampled measure on rendered output | ≤ ceiling, zero tolerance, 100% |
| Speech quality uplift | DNSMOS P.835 (SIG/BAK/OVRL) | BAK +≥1.0 on noisy classes; SIG −≤0.1 on ALL classes (never hurt the voice); OVRL +≥0.4 noisy classes; clean-control class: all deltas ≥ −0.05 |
| Intrusive quality (paired synthetic set) | PESQ-WB, STOI | PESQ +≥0.4, STOI never −; report-only trend lines |
| Leveler | speech-gated ST-loudness variance | σ reduced ≥50% on class 6; music-segment loudness change ≤ 2 LU on class 7 |
| Cut artifacts | spectral discontinuity at cut points (frame-boundary flux z-score) | 0 outliers over threshold on rendered cut fixtures |
| ASR | jiwer WER vs references | whisper-small ≤ reference-impl +1% abs |
| Fillers | labeled set P/R | P ≥ 0.90, R ≥ 0.70 |
| Diarization | DER (pyannote.metrics) on AMI subset | ≤ 20% |
| Determinism | double-render hash compare | identical, 100%, every CI run |
| Regression | per-version output hashes + metric deltas vs last release | any metric regression > noise band fails CI with report |

## 3. Blind listening protocol (subjective gate, run at M1/M4 exits + each release)

ABX-style sheet: per corpus clip, randomized pairs (ours vs Auphonic free-tier output; M4: Studio vs Adobe Enhance — outputs fetched manually once per round by the owner or a browser session, stored in `eval/competitor_refs/`, never automated against their ToS). Rubric per pair: noise (1–5), voice naturalness (1–5), artifacts (list), levels consistency (1–5), "which would you publish?" Panel: minimum owner + 2 recruited listeners + the maintainer's own structured listen. Pass = "publish ours" ≥ 50% on the class (parity) with zero "unusable" votes. Log sheets in repo.

## 4. Performance budgets (regression-tested on pinned reference hardware profiles)

| Scenario | Budget |
|---|---|
| Cold start → interactive | < 2 s |
| 1-h file drop → waveform+playable | < 5 s |
| Analysis pass | ≥ 20× RT (4-core) |
| Master Standard | ≥ 5× RT 4-core AVX2; ≥ 2× RT on 2-core/8 GB floor machine |
| Master Fast | ≥ 15× RT 4-core |
| Studio tier | ≥ 1× RT mid GPU (DirectML GTX1660-class); ≥ 0.3× CPU with warning |
| whisper small | ≥ 3× RT 8-core CPU; large-v3-turbo ≥ 1× on mid GPU |
| RAM | < 1.5 GB for 3-h stereo master; < 2.5 GB with ASR running |
| Waveform | 60 fps scroll/zoom on 3-h file (mmap'd peaks) |
| Batch | linear scaling to N-1 cores; UI responsive throughout |
| A/B toggle | < 50 ms |
| Installer | Standard ≤ 200 MB; app disk ≤ 500 MB w/o packs |

CI perf job: RTF + RAM tracked per commit on the CI runner (relative regressions >10% fail); absolute budgets verified on real hardware at milestone exits.

## 5. CI matrix (GitHub Actions, from M0)

- **windows-latest:** build, clippy/fmt, unit + integration, eval-smoke (5-clip mini-corpus, all gates), package NSIS (nightly artifact).
- **macos-latest (arm64) + macos-13 (x64):** build, clippy, unit tests — every PR from M0 (the anti-porting-cliff gate); full eval on mac from M6.
- **Nightly:** full corpus eval + perf + determinism + 100-file soak; publishes metric dashboard to a `eval-reports` branch (trend HTML).
- **Hygiene jobs:** dependency-tree network-crate check (02 §privacy), license-scan (cargo-deny + npm licence-checker against MIT-compat allowlist), model-manifest hash verify, TS lint/typecheck.
- Model/ffmpeg binaries cached by hash; repo stays < 100 MB (assets on GH Releases).

## 6. Release checklist (every release; M5/M6 exits)

Airplane-mode full-feature pass · fresh-VM install/uninstall (Win10 1809, Win11; mac: last 3 OS versions, Intel + AS) · full corpus eval green + blind panel logged · determinism double-render · 100-file soak + 3-h file + sleep/resume mid-batch · updater upgrade-from-previous test · signed binaries verified (signtool / spctl) · tag round-trip test · docs version bump · CHANGELOG · license/attribution screen current (incl. CC-BY Parakeet if shipped) · GH release draft with checksums · winget/scoop/brew PRs.

## 7. Beta & feedback (M5.I)

GitHub pre-releases + a pinned "beta feedback" Discussion; triage playbook: crashes (P0, diagnostics zip attached) > audio-quality regressions (P1, request 30 s sample when shareable — never require it, privacy brand) > UX > features. Weekly beta build cadence. Convert every quality complaint into a corpus clip or synthetic reproduction when possible — the corpus grows from real failures.
