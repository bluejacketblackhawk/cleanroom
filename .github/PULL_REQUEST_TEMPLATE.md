## Summary

<!-- One-line summary of what this PR does -->

## Linked Issue

Closes #<!-- issue number -->

## Checklist

**Code & Quality:**
- [ ] `cargo fmt` passed locally (`cargo fmt --check`)
- [ ] `cargo clippy` passed locally (no new warnings)
- [ ] Tests pass locally (`cargo test` and npm tests)
- [ ] No new network calls outside `updater/` and `models/` modules (enforce in code review)

**For DSP / AI Changes:**
- [ ] Eval subset numbers pasted (before/after metrics, model, date)
- [ ] Determinism double-render verified: same file + settings yields identical output hash
- [ ] (If loudness/dynamics changed): listened to A/B on at least two test files

**Documentation & Dependencies:**
- [ ] Docs updated (README, ADRs, user guide, if applicable)
- [ ] New dependencies added to `docs/licenses.md` with license verified
- [ ] All new deps are MIT/Apache-2.0/BSD-compatible (no GPL; no unlicensed models)

## Description

<!-- Longer explanation of the change -->

## Testing

<!-- How did you test this? What files/scenarios did you use? -->
