#!/usr/bin/env bash
# Airplane-mode dependency posture (02 §Privacy & security, ADR-009).
#
# Network crates are permitted ONLY in the sanctioned `models`/updater modules. The `models`
# module now exists: `anvil-models` is the single HTTP-client home — hash-verified model
# downloads into a per-user, mac-safe dir — shared by the desktop Models screen and
# `anvil models pull`. The pure audio/AI/media engine crates below must still have ZERO
# HTTP-client deps: the structural guarantee that audio never leaves the machine. We check
# them explicitly and intentionally ignore the sanctioned network users — the Tauri shell
# (anvil-desktop: signed updater + LLM download), `anvil-models` itself, and `anvil-cli` (a
# user-facing model-download entry point that pulls via anvil-models, exactly like the desktop).
set -euo pipefail

# The pure engine crates: they process audio/text locally and must never gain an HTTP client.
ENGINE_CRATES=(
  anvil-core anvil-dsp anvil-ai anvil-asr
  anvil-llm anvil-media anvil-project
)

# HTTP client / transfer crates — their presence in the engine would mean audio could
# leave the machine. Refine (don't just delete) this when updater/models modules arrive.
PATTERN='^(reqwest|reqwest-middleware|hyper|h2|ureq|isahc|attohttpc|surf|curl|curl-sys|libcurl-sys) '

fail=0
for crate in "${ENGINE_CRATES[@]}"; do
  tree="$(cargo tree -p "$crate" -e normal --prefix none 2>/dev/null | sort -u || true)"
  hits="$(echo "$tree" | grep -E "$PATTERN" || true)"
  if [ -n "$hits" ]; then
    echo "::error::network crate found in engine crate '$crate':"
    echo "$hits" | sed 's/^/    /'
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  echo "Network crates are allowed only in the sanctioned model-download/updater users"
  echo "(anvil-models, anvil-desktop, anvil-cli) — not in the pure engine crates above."
  exit 1
fi
echo "OK: no HTTP-client crates in the engine dependency tree — airplane-mode posture holds."
