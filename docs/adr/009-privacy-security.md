# ADR-009: Privacy & Security Posture

- Status: Accepted (from the build handoff)
- Date: 2026-07-13
- Source: handoff/02-ARCHITECTURE.md § Privacy & security posture

## Context

Cleanroom is marketed as "100% local" — users want confidence that their podcast audio (often unpublished, sensitive, or confidential) never leaves their machine. Any network call outside offline-only updater/downloads undermines trust and violates the core value prop. Security model must prevent model/code injection and detect corruption.

## Decision

**Network policy:**
- Zero network at runtime except:
  - **Updater:** Tauri signed-manifest against GitHub Releases, user-controllable, fails safe offline
  - **Model-pack downloads:** user-initiated, size shown before download, fail silent offline
- No telemetry, no crash upload, no behavioral analytics. Crash dumps saved locally with a "copy to clipboard for a GitHub issue" button (manual, not automatic).

**Enforcement and verification:**
- `reqwest`/network deps allowed ONLY in the `updater` and `models` modules. CI check greps the dependency tree and fails if network deps appear elsewhere.
- Sidecars (ffmpeg) spawned with stdin/out pipes only. No file access outside the project folder or temp directories.
- Model files hash-verified before load (compare against `models.json` manifest SHA-256).
- No dynamic code download, no `eval`, no script loading from files.
- Document the guarantee in README with firewall-sniffing instructions ("here's what Cleanroom reaches out to, verify with Wireshark").

## Consequences

**Enables:** Genuine offline operation; user trust ("100% local" is verifiable); GDPR/HIPAA compliance for regulated workflows; no cloud infrastructure cost; fails gracefully when offline

**Constrains:**
- Enforce: Any new feature that requires network must be gated to updater/models modules; feature review must verify this.
- Model manifest (SHA-256 hashes) is the trust anchor; outdated manifest = security regression (update it with every model release).
- Logs/crash dumps are local-only and may contain audio metadata (file paths, timestamps); user controls whether to share with support.
- Air-gapped deployment: ensure all model packs are pre-downloaded; updater disabled in policy or config knob.
