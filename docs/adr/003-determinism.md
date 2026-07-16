# ADR-003: Determinism

- Status: Accepted (from the build handoff)
- Date: 2026-07-13
- Source: handoff/02-ARCHITECTURE.md § Determinism (ADR-003)

## Context

Users need to pin outputs to a version for archival and quality auditing. Regression tests require identical outputs for identical inputs. Sharing presets across versions (or documenting why they differ) requires bit-identical reproduction. Ad hoc or probabilistic processing prevents this.

## Decision

Same input + settings + version ⇒ bit-identical output. The five enforcement rules:

1. **Fixed block sizes and processing order.** No data-dependent thread scheduling affecting the mix. Parallelism only across independent tracks/files, never inside one stream's sample math — or use deterministic reduction.
2. **No `fast-math`.** Pinned ONNX Runtime version. Models addressed by SHA-256 in a manifest.
3. **Any RNG (dither) seeded from content hash**, not system entropy or wallclock.
4. **Project files record `{chain_version, model_hashes, params}`.** The engine keeps old chain versions callable behind a version switch for at least one major release.
5. **CI regression test suite:** golden corpus → output hashes compared per version (see 06 §Testing).

## Consequences

**Enables:** Version-pinned rendering for archival; exact reproduction for debugging; regression tests in CI; sound design versioning (feature #11); shareable presets that are semantically equivalent across versions

**Constrains:**
- Enforce: Any dither, initialization, or randomness must be seeded from content (file hash or deterministic state), not system entropy.
- Model runtime library versions (ONNX Runtime, llama.cpp) must be pinned; version upgrades require explicit decision and hash comparison.
- Changes to the DSP chain (module reordering, parameter defaults, algorithm choice) must be versioned; old versions remain available for at least one major release to support reproduction.
- CI must maintain and run a golden corpus (fixtures with known-good output hashes) on every commit; regressions surface before merge.
