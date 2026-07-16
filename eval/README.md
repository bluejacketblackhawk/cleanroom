# ANVIL eval harness (dev-only — never shipped)

The referee for every DSP/AI decision. No PR touching the processing chain merges without
an eval run (handoff/06-QUALITY-EVAL.md). Python is allowed here because this code never
ships in the app.

## Layout

```
run.py                     runner CLI: validate/smoke/fixtures/conformance (M0) plus
                            synth/master-eval/determinism/regress (M1 lane 2)
anvil_cli.py                subprocess wrapper for the `anvil` CLI binary (resolve it,
                            run analyze/master, detect the M0 "unimplemented" scaffold)
synth.py                    synthetic paired-degradation corpus generator (`run.py synth`)
metrics/                    objective metric implementations
  loudness.py                ebur128 self-measure + ffmpeg cross-check (M0)
  dnsmos.py                  DNSMOS P.835 SIG/BAK/OVRL (ONNX model, dev-only fetch)
  intrusive.py                PESQ-WB + STOI (paired synthetic set)
  leveler.py                  speech-gated short-term loudness variance + music-segment delta
models/                     DNSMOS ONNX model (gitignored — fetch once, see metrics/dnsmos.py)
corpus/                     manifests + clips (private clips gitignored; example committed)
  manifest.example.json      the manifest schema, with two synthetic redistributable clips
  fixtures/                   synthetic loudness fixtures from `run.py fixtures` (gitignored)
  synth/                      synthetic paired-degradation corpus from `run.py synth` (gitignored)
reports/                    generated metric reports (gitignored)
tests/                      pytest suite (parsers/generators/gate logic; no external tools required)
```

## Quick start

```sh
python run.py smoke                       # validate the example manifest + class coverage
python run.py validate --manifest path    # validate a real corpus manifest
python run.py fixtures                    # generate synthetic loudness fixtures (needs ffmpeg)
python run.py conformance                 # cross-check ffmpeg ebur128 vs `anvil analyze --json`
python run.py synth                       # generate a synthetic paired-degradation corpus
python run.py master-eval                 # run `anvil master` on fixtures, check the M1 gates
python run.py determinism                 # double-render `anvil master`, hash-compare
python run.py regress                     # compare current metrics/hashes vs a stored baseline
python -m pytest                          # run the test suite (needs requirements.txt installed)
```

`smoke`/`validate` are hermetic (stdlib only, no ffmpeg/deps/network) and run in CI as
the M0 eval-smoke gate. Every other command resolves its own dependency chain (an
explicit flag, an env var, PATH, then — for `anvil` — the standard cargo build dirs)
and prints `skipped: ... unavailable` + exits 0 instead of crashing when a dependency
isn't there, so each metric lights up automatically as its dependency lands:

| command | needs |
|---|---|
| `fixtures` / `conformance` | ffmpeg |
| `synth` | numpy, scipy, soundfile |
| `master-eval` | ffmpeg, the `anvil` CLI with `master` implemented; DNSMOS/PESQ/STOI checks additionally skip individually if onnxruntime+model / pesq / pystoi aren't installed |
| `determinism` | the `anvil` CLI with `master` implemented |
| `regress` | ffmpeg, the `anvil` CLI with `master` implemented |

As more metrics land, `smoke` grows into the 5-clip mini-corpus gate and a nightly
full-corpus job publishes trend HTML to an `eval-reports` branch (06 §5).

### Synthetic paired-degradation corpus (`run.py synth`)

`synth.py` manufactures a *paired* corpus — a known-clean signal plus a labeled
degradation (RIR convolution, additive noise at a known SNR, hum, clipping, bandwidth
limiting, level gaps, or a music bed), with the clean signal kept as the reference. That
pairing is what makes PESQ/STOI possible without real recordings (06 §1). It covers 10
of the 12 06 §1 failure classes; classes 10 (multitrack bleed) and 11 (non-English) need
real sourced content and are never synthesized (see `synth.py` module docstring).

```sh
python run.py synth                                   # all synthesizable classes, 3 variants each
python run.py synth --classes clean-studio,clipped     # a subset
python run.py synth --clean-dir path/to/clean/wavs     # use real clean speech instead of synthesizing it
python run.py synth --noise-dir path/to/noise/wavs     # use real noise instead of synthesizing it
```

Writes `eval/corpus/synth/<class>/<variant>.wav` (+ `.clean.wav` reference) and a
`manifest.json` in the same schema `validate`/`smoke` already understand. Gitignored —
regenerate on demand.

### `anvil master` gates (`master-eval` / `determinism` / `regress`)

These three all consume the same manifest (`corpus/synth/manifest.json` by default, or
`--fixtures path/to/manifest.json`) and call `anvil master <input> -o <out> --preset
NAME --tier fast|standard|studio --report path/to/report.json` per clip via
`anvil_cli.run_master`. `master` may not exist yet (`crates/anvil-cli` scaffolds it as
`emit_unimplemented`, exit code 70) — that's detected and reported as a clean skip
(`unavailable`), not a failure, so these commands go from "skips instantly" to "runs for
real" the moment another lane wires up the real DSP chain, with zero changes needed here.

```sh
python run.py master-eval                              # loudness/TP/DNSMOS/PESQ-STOI/leveler gates (06 §2)
python run.py determinism                               # double-render + sha256 compare
python run.py regress --update-baseline                 # snapshot current metrics as the baseline
python run.py regress                                    # compare current run vs that baseline
```

`master-eval` asserts, per clip: integrated loudness within ±0.5 LU of `--target-lufs`
(ffmpeg ebur128 cross-check on the actual rendered output, not the tool's own self-
report) and true peak ≤ `--true-peak-ceiling` with zero tolerance; DNSMOS SIG/BAK/OVRL
uplift per the 06 §2 class rules; PESQ +≥0.4 (STOI report-only) against the clip's paired
clean reference where one exists; and, class-specific, leveler variance reduction ≥50%
on `level-gaps` fixtures or music-segment loudness change ≤2 LU on `music-plus-speech`
fixtures (needs `ground_truth.music_segments_s` on the clip). Prints a human table and
writes a machine verdict JSON (default `eval/reports/master-eval/master-eval.json`).

`regress` records a sha256 hash (traceability only — DSP tuning changes it on every
commit, so it's not itself the pass/fail signal) plus integrated LUFS / true peak /
DNSMOS OVRL per clip; `--update-baseline` snapshots those as the new baseline, and a
plain run fails if any metric moves beyond its noise band (LUFS ±0.2, true peak
±0.1 dBTP, DNSMOS OVRL ±0.1) vs the stored baseline.

### DNSMOS model (dev-only, fetched once)

`metrics/dnsmos.py` needs the published DNSMOS P.835 ONNX model, which is NOT in this
repo (gitignored — `*.onnx` and `eval/models/` in the root `.gitignore`). Fetch it once:

```powershell
New-Item -ItemType Directory -Force -Path eval\models | Out-Null
Invoke-WebRequest -Uri https://raw.githubusercontent.com/microsoft/DNS-Challenge/master/DNSMOS/DNSMOS/sig_bak_ovr.onnx -OutFile eval\models\sig_bak_ovr.onnx
```

sha256 `269fbebdb513aa23cddfbb593542ecc540284a91849ac50516870e1ac78f6edd` (1,157,965
bytes) — `metrics.dnsmos.verify_model_hash()` checks it. Without the model (or without
`onnxruntime` installed), `metrics.dnsmos.dnsmos_available()` returns False and every
caller (`master-eval`, `regress`) skips the DNSMOS check cleanly rather than crash.

### Loudness conformance (M0 exit gate)

`06-QUALITY-EVAL.md` §2 requires ANVIL's own loudness measurement (`anvil analyze
<file> --json`, the Rust engine) to match an independent ffmpeg `ebur128` measurement
within +/-0.1 LU on 10 fixtures before M0 exits, plus a zero-tolerance true-peak
ceiling check. `run.py conformance` is that comparison harness:

```sh
python run.py fixtures                                        # build local self-test fixtures
python run.py conformance                                     # ffmpeg-only (no cross-check yet)
python run.py conformance --anvil-json anvil-measurements.json # cross-check against ANVIL
```

`--fixtures` accepts either a directory of `*.wav` files or a JSON manifest (the
`fixtures.json` `run.py fixtures` writes, or the golden corpus manifest format);
`--anvil-json` is a JSON object keyed by fixture id, each value shaped exactly like
what `anvil analyze --json` is expected to emit for one file:

```json
{
  "syn-fixture-16lufs": {
    "integrated_lufs": -16.03,
    "true_peak_dbtp": -3.02,
    "loudness_range_lu": 1.10
  }
}
```

Build that file by running `anvil analyze <path> --json` once per fixture and
assembling the per-file objects into a dict keyed by fixture id. `conformance` prints
a human-readable table plus a machine-readable verdict JSON (`--json-out` to save it
under `reports/`) and exits non-zero if any fixture fails either check.

## Corpus

12 failure classes x 3-4 clips + long-form perf fixtures (06 §1). Real bad recordings are
the biggest quality lever (07 §6.6, owner-supplied). Private clips stay out of the repo;
the synthetic/redistributable subset publishes as **anvil-bench** for the launch benchmark.

## Metrics & gates

See handoff/06-QUALITY-EVAL.md §2 for the full CI-enforced table. Live as of M1 lane 2:
loudness +/-0.5 LU + true-peak zero-tolerance (`master-eval`), DNSMOS uplift
(`metrics/dnsmos.py`), PESQ/STOI (`metrics/intrusive.py`), leveler variance /
music-segment stability (`metrics/leveler.py`), determinism double-render (`determinism`),
and metric-delta regression tracking (`regress`). Not yet wired: WER, filler
precision/recall, diarization DER (later lanes).

## Dependencies

`run.py smoke`/`validate` need only the Python standard library. `fixtures`/`conformance`
additionally need an ffmpeg binary (no pip package). `synth`/`master-eval`/`regress`
need numpy/scipy/soundfile at minimum, plus onnxruntime (DNSMOS)/pesq/pystoi for their
respective optional checks — install into a local `.venv`:

```sh
python -m venv .venv && .venv/Scripts/pip install -r requirements.txt
```
