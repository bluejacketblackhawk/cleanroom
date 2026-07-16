#!/usr/bin/env python3
"""ANVIL eval harness runner (dev-only — never shipped).

The eval harness is the referee for every DSP/AI decision (handoff/06-QUALITY-EVAL.md).
It is built in M0-M1 *before* the processing chain is tuned. M0 established the corpus
manifest format, the ffmpeg ebur128 cross-check, and the runner CLI. M1 lane 2 (this
file) adds the rest: DNSMOS/PESQ/STOI, a synthetic paired-degradation corpus generator,
and the `anvil master` gate/determinism/regression harness.

Usage:
    python run.py smoke                       # validate the example manifest, report coverage
    python run.py validate --manifest FILE    # validate a corpus manifest
    python run.py fixtures                    # generate synthetic loudness fixtures (needs ffmpeg)
    python run.py conformance                 # cross-check ffmpeg ebur128 vs `anvil analyze --json`
    python run.py synth                       # generate a synthetic paired-degradation corpus
    python run.py master-eval                 # run `anvil master` on fixtures, check M1 gates
    python run.py determinism                 # double-render `anvil master`, hash-compare
    python run.py regress                     # compare current metrics/hashes vs a stored baseline

`smoke`/`validate` are hermetic (stdlib only). `fixtures`/`conformance` need ffmpeg;
`synth`/`master-eval`/`regress` also need numpy/scipy/soundfile (and DNSMOS/PESQ/STOI
optionally, for their respective gates); `master-eval`/`determinism`/`regress` need the
`anvil` CLI binary with `master` implemented. Every one of these resolves its
dependency (an explicit flag, an env var, then PATH/standard build dirs) and — if it
can't be found — prints "skipped: ... unavailable" and exits 0 rather than crashing, so
each metric lights up automatically as its dependency lands.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from dataclasses import asdict, dataclass
from pathlib import Path

from anvil_cli import resolve_anvil_bin, run_master
from metrics.loudness import (
    LoudnessMeasurement,
    measure_lufs_ffmpeg,
    resolve_ffmpeg,
)

HERE = Path(__file__).resolve().parent

# The 12 failure classes of the golden corpus (06 §1). Every class must have >=3 clips
# before the M1 blind-listening gate is meaningful.
KNOWN_CLASSES = {
    1: "clean-studio",
    2: "untreated-room-echo",
    3: "constant-broadband-noise",
    4: "dynamic-noise",
    5: "hum-50-60hz",
    6: "level-gaps",
    7: "music-plus-speech",
    8: "clipped",
    9: "bandwidth-limited",
    10: "multitrack-bleed",
    11: "non-english",
    12: "worstcase-laptop-reverb-noise",
}

REQUIRED_CLIP_FIELDS = {"id", "class", "path", "license", "redistributable"}


def load_manifest(path: Path) -> dict:
    with path.open(encoding="utf-8") as fh:
        return json.load(fh)


def validate(manifest: dict) -> list[str]:
    """Return a list of human-readable problems; empty means valid."""
    errors: list[str] = []
    clips = manifest.get("clips")
    if not isinstance(clips, list):
        return ["manifest has no 'clips' array"]

    seen_ids: set[str] = set()
    for i, clip in enumerate(clips):
        where = f"clip[{i}]"
        missing = REQUIRED_CLIP_FIELDS - clip.keys()
        if missing:
            errors.append(f"{where}: missing fields {sorted(missing)}")
            continue
        cid = clip["id"]
        if cid in seen_ids:
            errors.append(f"{where}: duplicate id {cid!r}")
        seen_ids.add(cid)
        if clip["class"] not in KNOWN_CLASSES.values():
            errors.append(f"{where} ({cid}): unknown class {clip['class']!r}")
        if not isinstance(clip["redistributable"], bool):
            errors.append(f"{where} ({cid}): 'redistributable' must be a boolean")
    return errors


def class_coverage(manifest: dict) -> dict[str, int]:
    counts = {name: 0 for name in KNOWN_CLASSES.values()}
    for clip in manifest.get("clips", []):
        cls = clip.get("class")
        if cls in counts:
            counts[cls] += 1
    return counts


def _report(manifest: dict) -> int:
    errors = validate(manifest)
    coverage = class_coverage(manifest)
    print(f"clips: {len(manifest.get('clips', []))}")
    print("class coverage (target >=3 each before M1 blind gate):")
    for name, n in coverage.items():
        flag = "" if n >= 3 else "  <- under-covered"
        print(f"  {name:<32} {n}{flag}")
    if errors:
        print("\nVALIDATION ERRORS:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        return 1
    print("\nmanifest valid.")
    return 0


def cmd_smoke(_args: argparse.Namespace) -> int:
    example = HERE / "corpus" / "manifest.example.json"
    if not example.exists():
        print(f"missing {example}", file=sys.stderr)
        return 1
    return _report(load_manifest(example))


def cmd_validate(args: argparse.Namespace) -> int:
    path = Path(args.manifest)
    if not path.exists():
        print(f"missing {path}", file=sys.stderr)
        return 1
    return _report(load_manifest(path))


# --- conformance: ffmpeg ebur128 vs `anvil analyze --json` cross-check --------------
#
# M0 exit gate (handoff/06-QUALITY-EVAL.md §2 loudness row): `anvil analyze <file>
# --json` must match ffmpeg's ebur128 measurement within +/-0.1 LU on 10 fixtures.
# ANVIL measurement JSON shape this command expects (see load_anvil_measurements):
# a JSON object keyed by fixture id, each value shaped like `LoudnessMeasurement` —
#   {"<fixture-id>": {"integrated_lufs": -16.03, "true_peak_dbtp": -3.02,
#                      "loudness_range_lu": 1.10}, ...}
# i.e. exactly the fields `anvil analyze --json` is expected to emit for one file;
# assemble the dict by running that command once per fixture and keying by id.

DEFAULT_TRUE_PEAK_CEILING_DBTP = -1.0  # matches Preset::default() (crates/anvil-project)
DEFAULT_LUFS_TOLERANCE = 0.1  # LU; the M0 conformance gate (stricter than the +/-0.5 LU corpus gate)


@dataclass
class Fixture:
    """A single audio fixture to cross-check, resolved from a directory or manifest."""

    id: str
    wav_path: Path
    ceiling_dbtp: float
    note: str | None = None
    synthetic: bool = False


def load_fixtures(fixtures_arg: str | Path, default_ceiling: float) -> list[Fixture]:
    """Resolve a `--fixtures` argument into a list of Fixture.

    Accepts either a directory of *.wav files (id = filename stem, ceiling = the
    global default) or a JSON manifest (the corpus manifest schema, or the
    `fixtures.json` written by `run.py fixtures`) whose clips carry an id/path and
    optionally a `true_peak_ceiling_dbtp` override.
    """
    path = Path(fixtures_arg)
    if path.is_dir():
        wavs = sorted(path.glob("*.wav"))
        return [Fixture(id=w.stem, wav_path=w, ceiling_dbtp=default_ceiling) for w in wavs]

    if path.is_file() and path.suffix == ".json":
        manifest = load_manifest(path)
        fixtures: list[Fixture] = []
        for clip in manifest.get("clips", []):
            wav_path = (path.parent / clip["path"]).resolve()
            fixtures.append(
                Fixture(
                    id=clip["id"],
                    wav_path=wav_path,
                    ceiling_dbtp=float(clip.get("true_peak_ceiling_dbtp", default_ceiling)),
                    note=clip.get("notes"),
                    synthetic=bool(clip.get("synthetic", False)),
                )
            )
        return fixtures

    raise FileNotFoundError(
        f"fixtures path is neither a directory nor a .json manifest: {path}"
    )


def load_anvil_measurements(path: str | Path) -> dict[str, LoudnessMeasurement]:
    """Load the `anvil analyze --json` measurements JSON described above."""
    with Path(path).open(encoding="utf-8") as fh:
        raw = json.load(fh)
    out: dict[str, LoudnessMeasurement] = {}
    for fixture_id, values in raw.items():
        out[fixture_id] = LoudnessMeasurement(
            integrated_lufs=float(values["integrated_lufs"]),
            true_peak_dbtp=float(values["true_peak_dbtp"]),
            loudness_range_lu=float(values["loudness_range_lu"]),
        )
    return out


def _conformance_verdict(
    fixtures: list[Fixture],
    ffmpeg: str,
    anvil_measurements: dict[str, LoudnessMeasurement],
    tolerance: float,
    fixtures_source: str,
) -> dict:
    results = []
    for fx in fixtures:
        entry: dict = {"id": fx.id, "path": str(fx.wav_path)}
        try:
            ff = measure_lufs_ffmpeg(fx.wav_path, ffmpeg=ffmpeg)
        except RuntimeError as exc:
            entry["error"] = str(exc)
            entry["pass"] = False
            results.append(entry)
            continue

        entry["ffmpeg"] = asdict(ff)
        entry["ceiling_dbtp"] = fx.ceiling_dbtp
        true_peak_ok = ff.true_peak_dbtp <= fx.ceiling_dbtp

        anvil = anvil_measurements.get(fx.id)
        if anvil is not None:
            delta = anvil.integrated_lufs - ff.integrated_lufs
            entry["anvil"] = asdict(anvil)
            entry["lufs_delta"] = round(delta, 4)
            entry["lufs_within_tolerance"] = abs(delta) <= tolerance
            true_peak_ok = true_peak_ok and anvil.true_peak_dbtp <= fx.ceiling_dbtp
        else:
            entry["anvil"] = None
            entry["lufs_delta"] = None
            entry["lufs_within_tolerance"] = None

        entry["true_peak_ok"] = true_peak_ok
        entry["pass"] = true_peak_ok and entry["lufs_within_tolerance"] in (None, True)
        results.append(entry)

    passed = sum(1 for r in results if r.get("pass"))
    return {
        "fixtures_source": fixtures_source,
        "tolerance_lu": tolerance,
        "default_ceiling_dbtp": DEFAULT_TRUE_PEAK_CEILING_DBTP,
        "anvil_json_provided": bool(anvil_measurements),
        "total": len(results),
        "passed": passed,
        "failed": len(results) - passed,
        "overall_pass": passed == len(results) and len(results) > 0,
        "results": results,
    }


def _print_conformance_table(verdict: dict) -> None:
    print(
        f"conformance: {verdict['fixtures_source']}  "
        f"(tolerance +/-{verdict['tolerance_lu']} LU, ceiling <= {verdict['default_ceiling_dbtp']} dBTP "
        f"unless a fixture overrides it)"
    )
    header = f"{'id':<28}{'ffmpeg I':>10}{'anvil I':>10}{'d LU':>8}{'TP ffmpeg':>11}{'ceiling':>9}  verdict"
    print(header)
    print("-" * len(header))
    for r in verdict["results"]:
        if "error" in r:
            print(f"{r['id']:<28}  ERROR: {r['error']}")
            continue
        ff_i = r["ffmpeg"]["integrated_lufs"]
        anvil = r.get("anvil")
        anvil_i = f"{anvil['integrated_lufs']:.2f}" if anvil else "-"
        delta = f"{r['lufs_delta']:+.2f}" if r["lufs_delta"] is not None else "-"
        tp = r["ffmpeg"]["true_peak_dbtp"]
        ceiling = r["ceiling_dbtp"]
        mark = "PASS" if r["pass"] else "FAIL"
        print(
            f"{r['id']:<28}{ff_i:>10.2f}{anvil_i:>10}{delta:>8}{tp:>11.2f}{ceiling:>9.2f}  {mark}"
        )
    tail = f"\n{verdict['passed']}/{verdict['total']} passed"
    if not verdict["overall_pass"]:
        tail += "  -- CONFORMANCE FAILED"
    print(tail)


def cmd_conformance(args: argparse.Namespace) -> int:
    ffmpeg = resolve_ffmpeg(args.ffmpeg)
    if ffmpeg is None:
        print("skipped: ffmpeg unavailable")
        return 0

    fixtures_path = Path(args.fixtures)
    if not fixtures_path.exists():
        print(
            f"missing fixtures path: {fixtures_path}\n"
            "  generate synthetic fixtures first with `python run.py fixtures`, "
            "or pass --fixtures pointing at a corpus manifest/directory",
            file=sys.stderr,
        )
        return 1

    try:
        fixtures = load_fixtures(fixtures_path, args.ceiling)
    except (FileNotFoundError, KeyError, json.JSONDecodeError) as exc:
        print(f"error loading fixtures from {fixtures_path}: {exc}", file=sys.stderr)
        return 1
    if not fixtures:
        print(f"no fixtures found under {fixtures_path}", file=sys.stderr)
        return 1

    anvil_measurements: dict[str, LoudnessMeasurement] = {}
    if args.anvil_json:
        try:
            anvil_measurements = load_anvil_measurements(args.anvil_json)
        except (OSError, KeyError, json.JSONDecodeError) as exc:
            print(f"error loading --anvil-json {args.anvil_json}: {exc}", file=sys.stderr)
            return 1

    verdict = _conformance_verdict(
        fixtures, ffmpeg, anvil_measurements, args.tolerance, str(fixtures_path)
    )
    _print_conformance_table(verdict)

    if args.json_out:
        out_path = Path(args.json_out)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(json.dumps(verdict, indent=2), encoding="utf-8")
        print(f"\nverdict JSON written to {out_path}")
    else:
        print("\n--- verdict JSON ---")
        print(json.dumps(verdict, indent=2))

    return 0 if verdict["overall_pass"] else 1


# --- fixtures: synthetic loudness clips at known targets, for self-testing ----------
#
# These exist so `conformance` can be exercised end-to-end before the real golden
# corpus (private, assembled separately) and before `anvil analyze` is implemented.
# Generated into eval/corpus/fixtures/, which is gitignored (eval/corpus/* pattern) —
# regenerate on demand with `python run.py fixtures`, never committed.

FIXTURE_DURATION_S = 12

FIXTURE_SPECS: list[dict] = [
    {
        "id": "syn-fixture-16lufs",
        "note": "pink noise loudness-normalized near -16 LUFS, comfortable true-peak headroom",
        "loudnorm_i": -16.0,
        "loudnorm_tp": -3.0,
        "loudnorm_lra": 7.0,
        "ceiling_dbtp": -1.0,
        "seed": 42,
    },
    {
        "id": "syn-fixture-neartp",
        "note": "pink noise pushed close to (just under) a -1.0 dBTP true-peak ceiling",
        "loudnorm_i": -12.0,
        "loudnorm_tp": -1.5,
        "loudnorm_lra": 4.0,
        "ceiling_dbtp": -1.0,
        "seed": 43,
    },
]


def _loudnorm_analysis_pass(
    ffmpeg: str, source_filter: str, target_i: float, target_tp: float, target_lra: float
) -> dict[str, float]:
    """Run loudnorm's analysis-only pass and return its measured `input_*` stats.

    First half of ffmpeg's documented two-pass loudnorm workflow: this pass makes no
    output, it just reports what the *source* measures as, so the second pass can
    apply an exact linear gain instead of loudnorm's single-pass dynamic-compressor
    estimate (which we've observed can land several dB off target in dynamic mode).
    """
    loudnorm = f"loudnorm=I={target_i}:TP={target_tp}:LRA={target_lra}:print_format=json"
    proc = subprocess.run(
        [ffmpeg, "-hide_banner", "-nostats", "-f", "lavfi", "-i", source_filter,
         "-af", loudnorm, "-f", "null", "-"],
        capture_output=True,
        text=True,
        check=False,
    )
    match = re.search(r"\{.*\}", proc.stderr, re.DOTALL)
    if not match:
        raise RuntimeError(
            f"loudnorm analysis pass produced no JSON block: {proc.stderr[-500:]}"
        )
    stats = json.loads(match.group(0))
    return {k: float(v) for k, v in stats.items() if k.startswith("input_")}


def _generate_fixture(ffmpeg: str, out_path: Path, spec: dict) -> None:
    """Render one synthetic fixture with ffmpeg: pink noise -> two-pass linear loudnorm.

    Two passes (analyze, then apply a linear gain from the measured stats) track the
    nominal I/TP/LRA target far more closely than a single loudnorm pass. Even so, we
    never trust loudnorm's own self-reported "Output ..." stats as the fixture's
    ground truth — the caller re-measures the actual rendered file with our own
    `measure_lufs_ffmpeg` and records *that* as the known value.
    """
    source = (
        f"anoisesrc=color=pink:duration={FIXTURE_DURATION_S}:"
        f"sample_rate=48000:seed={spec['seed']}"
    )
    target_i, target_tp, target_lra = spec["loudnorm_i"], spec["loudnorm_tp"], spec["loudnorm_lra"]
    measured = _loudnorm_analysis_pass(ffmpeg, source, target_i, target_tp, target_lra)
    loudnorm = (
        f"loudnorm=I={target_i}:TP={target_tp}:LRA={target_lra}:"
        f"measured_I={measured['input_i']}:measured_TP={measured['input_tp']}:"
        f"measured_LRA={measured['input_lra']}:measured_thresh={measured['input_thresh']}:"
        "linear=true"
    )
    # Deliberately mono, not stereo: ffmpeg's automatic mono->stereo channel-layout
    # conversion (`-ac 2`) applies an energy-preserving ~-3.01 dB pan law per channel,
    # which shifts the measured true peak well away from the linear gain loudnorm
    # just computed. Mono also matches a realistic single-speaker podcast fixture.
    proc = subprocess.run(
        [
            ffmpeg, "-y", "-hide_banner", "-nostats",
            "-f", "lavfi", "-i", source,
            "-af", loudnorm,
            "-ar", "48000", "-c:a", "pcm_s16le",
            str(out_path),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0 or not out_path.exists():
        raise RuntimeError(
            f"ffmpeg failed generating fixture {out_path.name} "
            f"(exit {proc.returncode}): {proc.stderr[-800:]}"
        )


def cmd_fixtures(args: argparse.Namespace) -> int:
    ffmpeg = resolve_ffmpeg(args.ffmpeg)
    if ffmpeg is None:
        print("skipped: ffmpeg unavailable")
        return 0

    out_dir = Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)

    clips = []
    for spec in FIXTURE_SPECS:
        wav_path = out_dir / f"{spec['id']}.wav"
        _generate_fixture(ffmpeg, wav_path, spec)
        measured = measure_lufs_ffmpeg(wav_path, ffmpeg=ffmpeg)
        clips.append(
            {
                "id": spec["id"],
                "path": wav_path.name,
                "synthetic": True,
                "notes": spec["note"],
                "true_peak_ceiling_dbtp": spec["ceiling_dbtp"],
                "measured_at_generation": asdict(measured),
            }
        )
        print(
            f"generated {wav_path.name}: I={measured.integrated_lufs:.2f} LUFS, "
            f"TP={measured.true_peak_dbtp:.2f} dBTP, LRA={measured.loudness_range_lu:.2f} LU"
        )

    manifest = {
        "$comment": (
            "Synthetic fixtures for self-testing eval/run.py conformance. Regenerate "
            "with `python run.py fixtures`. Not part of the golden corpus; gitignored."
        ),
        "synthetic": True,
        "generated_by": "eval/run.py fixtures",
        "clips": clips,
    }
    manifest_path = out_dir / "fixtures.json"
    manifest_path.write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(f"\nwrote {manifest_path} ({len(clips)} fixtures)")
    return 0


# --- synth: synthetic paired-degradation corpus generator (eval/synth.py) -----------


def _synth_deps_available() -> bool:
    try:
        import numpy  # noqa: F401
        import scipy  # noqa: F401
        import soundfile  # noqa: F401
    except ImportError:
        return False
    return True


def cmd_synth(args: argparse.Namespace) -> int:
    if not _synth_deps_available():
        print("skipped: numpy/scipy/soundfile unavailable (pip install -r requirements.txt)")
        return 0

    import synth as synth_module

    out_dir = Path(args.out)
    classes = [c.strip() for c in args.classes.split(",")] if args.classes else None
    clean_dir = Path(args.clean_dir) if args.clean_dir else None
    noise_dir = Path(args.noise_dir) if args.noise_dir else None

    try:
        manifest = synth_module.generate_corpus(
            out_dir,
            classes=classes,
            variants_per_class=args.variants_per_class,
            duration_s=args.duration,
            seed=args.seed,
            clean_dir=clean_dir,
            noise_dir=noise_dir,
        )
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    coverage = class_coverage(manifest)
    print(f"generated {len(manifest['clips'])} synthetic clips under {out_dir}")
    print("class coverage:")
    for name, n in coverage.items():
        if n:
            print(f"  {name:<32} {n}")
    print(f"\nmanifest: {out_dir / 'manifest.json'}")
    return 0


# --- shared fixture loading for master-eval / determinism / regress -----------------
#
# These all consume the same manifest schema (`smoke`/`validate`/`synth` all produce or
# check it): a `clips` array where each clip's `path` is relative to the manifest file,
# and an optional `ground_truth.reference_wav` (also manifest-relative) is the paired
# clean reference `synth` writes for intrusive metrics.


def _default_eval_manifest() -> Path:
    """Prefer the synthetic corpus if one has been generated; fall back to the
    committed example manifest (2 clips) so the commands have *something* to run
    against even before `python run.py synth` has ever been invoked."""
    synth_manifest = HERE / "corpus" / "synth" / "manifest.json"
    if synth_manifest.exists():
        return synth_manifest
    return HERE / "corpus" / "manifest.example.json"


def load_eval_clips(manifest_path: Path) -> list[dict]:
    """Load a manifest's clips with `path`/`ground_truth.reference_wav` resolved to
    absolute paths (added as `_abs_path` / `_abs_reference_path`, the latter `None` when
    the clip carries no paired reference)."""
    manifest = load_manifest(manifest_path)
    base = manifest_path.parent
    # Manifest-level default for the perceptual-gate applicability key (see
    # `_master_eval_one`): a real-recording-backed corpus gates DNSMOS/PESQ/STOI; a
    # synthetic-speech corpus treats them as report-only. Default to "synthetic" so an
    # unflagged/legacy manifest is never silently trusted for the MOS gates.
    manifest_speech_source = manifest.get("speech_source", "synthetic")
    clips: list[dict] = []
    for clip in manifest.get("clips", []):
        c = dict(clip)
        c["_abs_path"] = (base / clip["path"]).resolve()
        ground_truth = clip.get("ground_truth") or {}
        reference = ground_truth.get("reference_wav")
        c["_abs_reference_path"] = (base / reference).resolve() if reference else None
        c["_speech_source"] = clip.get("speech_source", manifest_speech_source)
        clips.append(c)
    return clips


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


# --- master-eval: run `anvil master` on fixtures, assert the M1 gates (06 §2) -------

DEFAULT_TARGET_LUFS = -16.0  # matches Preset::default() (crates/anvil-project): podcast-stereo
LOUDNESS_TOLERANCE_LU = 0.5  # 06 §2 corpus gate (looser than the M0 +/-0.1 LU conformance gate)

LEVEL_GAP_CLASS = "level-gaps"
MUSIC_CLASS = "music-plus-speech"


def _gate_check(status: str, **extra) -> dict:
    """One gate's result: status is 'pass' | 'fail' | 'skip'. 'skip' never fails the
    overall verdict (a missing optional dependency isn't a DSP regression)."""
    return {"status": status, **extra}


def _master_eval_one(
    anvil_bin: str,
    ffmpeg: str,
    clip: dict,
    preset: str,
    tier: str,
    target_lufs: float,
    ceiling_dbtp: float,
    out_dir: Path,
    dnsmos_model,
    run_intrusive: bool,
) -> dict:
    """Run `anvil master` on one clip and check every applicable 06 §2 gate against the
    rendered output. Returns a JSON-able result dict; never raises — measurement/gate
    errors are captured per-check so one bad clip doesn't abort the whole run."""
    clip_id = clip["id"]
    class_name = clip["class"]
    input_path: Path = clip["_abs_path"]
    reference_path = clip.get("_abs_reference_path")
    # DNSMOS/PESQ/STOI are perceptual MOS predictors trained on *real* speech; on the
    # synth_speech_like proxy they are structurally miscalibrated (empirically the clean
    # reference itself scores DNSMOS OVRL ~2.0 vs ~3.4 for real recorded speech, and PESQ
    # collapses on band-limited harmonic signals the chain legitimately adds presence to).
    # So we still COMPUTE and record them on synthetic-speech fixtures, but as report-only
    # trend lines that never fail the run — they only gate on real-speech corpora. The
    # loudness / true-peak / leveler / music gates are physical measurements and stay
    # authoritative on synthetic fixtures. See eval/synth.py docstring and README-MAC.md.
    perceptual_gated = clip.get("_speech_source", "synthetic") == "real"

    clip_dir = out_dir / clip_id
    clip_dir.mkdir(parents=True, exist_ok=True)
    out_wav = clip_dir / "mastered.wav"
    report_json = clip_dir / "report.json"

    result = run_master(
        anvil_bin, input_path, out_wav, preset=preset, tier=tier, report_path=report_json
    )
    entry: dict = {"id": clip_id, "class": class_name, "input": str(input_path)}

    if result.unavailable:
        entry["unavailable"] = True
        entry["unavailable_reason"] = result.unavailable_reason
        entry["overall_pass"] = None
        return entry

    entry["unavailable"] = False
    if not result.ok:
        entry["error"] = f"anvil master exited {result.returncode}: {result.stderr[-500:]}"
        entry["overall_pass"] = False
        return entry

    entry["master_report"] = result.report
    checks: dict = {}

    try:
        measured = measure_lufs_ffmpeg(out_wav, ffmpeg=ffmpeg)
        lufs_delta = measured.integrated_lufs - target_lufs
        checks["loudness"] = _gate_check(
            "pass" if abs(lufs_delta) <= LOUDNESS_TOLERANCE_LU else "fail",
            measured=asdict(measured),
            target_lufs=target_lufs,
            lufs_delta=round(lufs_delta, 4),
            tolerance_lu=LOUDNESS_TOLERANCE_LU,
        )
        checks["true_peak"] = _gate_check(
            "pass" if measured.true_peak_dbtp <= ceiling_dbtp else "fail",
            true_peak_dbtp=measured.true_peak_dbtp,
            ceiling_dbtp=ceiling_dbtp,
        )
    except RuntimeError as exc:
        checks["loudness"] = _gate_check("fail", error=str(exc))
        checks["true_peak"] = _gate_check("fail", error=str(exc))

    if dnsmos_model is not None:
        from metrics.dnsmos import dnsmos_gate

        try:
            before = dnsmos_model.score(input_path)
            after = dnsmos_model.score(out_wav)
            verdict = dnsmos_gate(class_name, before, after)
            # "report" (not "pass"/"fail") on synthetic-speech fixtures: computed for the
            # trend line but non-gating, since the model isn't calibrated for that input.
            status = ("pass" if verdict.overall_pass else "fail") if perceptual_gated else "report"
            checks["dnsmos"] = _gate_check(
                status,
                gated=perceptual_gated,
                before=asdict(before),
                after=asdict(after),
                sig_delta=verdict.sig_delta,
                bak_delta=verdict.bak_delta,
                ovrl_delta=verdict.ovrl_delta,
                sig_ok=verdict.sig_ok,
                bak_ok=verdict.bak_ok,
                ovrl_ok=verdict.ovrl_ok,
                clean_control_ok=verdict.clean_control_ok,
            )
        except Exception as exc:  # noqa: BLE001 — one bad clip must not abort the run
            checks["dnsmos"] = _gate_check("fail", error=str(exc))
    else:
        checks["dnsmos"] = _gate_check("skip", reason="onnxruntime/model unavailable")

    if reference_path is not None and reference_path.exists():
        from metrics.dnsmos import CLEAN_CONTROL_CLASS, CLEAN_CONTROL_DELTA_MIN
        from metrics.intrusive import intrusive_available, intrusive_gate

        if run_intrusive and intrusive_available():
            try:
                iv = intrusive_gate(reference_path, input_path, out_wav)
                # The clean-control class has nothing to fix, so it structurally can't
                # gain +0.4 PESQ the way a genuinely degraded clip can — hold it to the
                # same "must not regress" bar as DNSMOS's clean-control check instead
                # (06 §2 only spells out the +0.4 uplift gate for the noisy/degraded
                # paired set; a flat control clip isn't what that gate is aimed at).
                if class_name == CLEAN_CONTROL_CLASS:
                    pesq_pass = iv.pesq_delta >= CLEAN_CONTROL_DELTA_MIN
                else:
                    pesq_pass = iv.pesq_ok
                # Same rule as DNSMOS: PESQ/STOI gate only on real speech; on the
                # synthetic proxy they are report-only trend lines (06 §2 already treats
                # STOI as report-only — here the whole intrusive check is, for synth).
                status = ("pass" if pesq_pass else "fail") if perceptual_gated else "report"
                checks["intrusive"] = _gate_check(
                    status,
                    gated=perceptual_gated,
                    pesq_before=iv.pesq_before,
                    pesq_after=iv.pesq_after,
                    pesq_delta=iv.pesq_delta,
                    stoi_before=iv.stoi_before,
                    stoi_after=iv.stoi_after,
                    stoi_delta=iv.stoi_delta,
                    stoi_trend=iv.stoi_trend,
                )
            except Exception as exc:  # noqa: BLE001
                checks["intrusive"] = _gate_check("fail", error=str(exc))
        else:
            checks["intrusive"] = _gate_check("skip", reason="pesq/pystoi unavailable")
    else:
        checks["intrusive"] = _gate_check("skip", reason="no paired clean reference for this clip")

    if class_name == LEVEL_GAP_CLASS:
        from metrics.leveler import leveler_variance_reduction

        try:
            lv = leveler_variance_reduction(input_path, out_wav)
            checks["leveler_variance"] = _gate_check(
                "pass" if lv.pass_ else "fail",
                std_before=lv.std_before,
                std_after=lv.std_after,
                reduction_fraction=lv.reduction_fraction,
            )
        except Exception as exc:  # noqa: BLE001
            checks["leveler_variance"] = _gate_check("fail", error=str(exc))
    elif class_name == MUSIC_CLASS:
        segments = (clip.get("ground_truth") or {}).get("music_segments_s")
        if segments:
            from metrics.leveler import music_segment_delta

            try:
                # The class-7 gate is about the *leveler* not pumping/ducking the music
                # bed — NOT about absolute level, which the master intentionally shifts by
                # normalizing the whole program to target (the loudness gate already owns
                # that). Comparing raw before/after segment loudness just re-measures the
                # global make-up gain and can never pass a loudness-normalizing master.
                # So we subtract the program-level loudness change (anvil's own whole-file
                # before->after) and gate on the *residual* per-segment movement.
                report = result.report or {}
                prog_before = (report.get("before") or {}).get("integrated_lufs")
                prog_after = (report.get("after") or {}).get("integrated_lufs")
                program_delta = (
                    prog_after - prog_before
                    if isinstance(prog_before, (int, float)) and isinstance(prog_after, (int, float))
                    else 0.0
                )
                deltas = music_segment_delta(
                    input_path,
                    out_wav,
                    [tuple(s) for s in segments],
                    clip_dir / "segments",
                    ffmpeg=ffmpeg,
                    program_delta_lu=program_delta,
                )
                checks["music_segment_loudness"] = _gate_check(
                    "pass" if all(d.pass_ for d in deltas) else "fail",
                    program_delta_lu=round(program_delta, 4),
                    segments=[
                        {
                            "start_s": d.start_s,
                            "end_s": d.end_s,
                            "delta_lu": round(d.delta_lu, 4),
                            "relative_delta_lu": round(d.relative_delta_lu, 4),
                            "pass": d.pass_,
                        }
                        for d in deltas
                    ],
                )
            except Exception as exc:  # noqa: BLE001
                checks["music_segment_loudness"] = _gate_check("fail", error=str(exc))
        else:
            checks["music_segment_loudness"] = _gate_check(
                "skip", reason="no ground_truth.music_segments_s on this clip"
            )

    entry["checks"] = checks
    entry["overall_pass"] = all(c["status"] != "fail" for c in checks.values())
    return entry


def _print_master_eval_table(verdict: dict) -> None:
    print(
        f"master-eval: {verdict['fixtures_source']}  (preset={verdict['preset']}, "
        f"tier={verdict['tier']}, target={verdict['target_lufs']} LUFS, "
        f"ceiling<={verdict['true_peak_ceiling_dbtp']} dBTP)"
    )
    header = f"{'id':<30}{'class':<28}{'loud':>6}{'tp':>5}{'dnsmos':>8}{'intrus':>8}{'lvl':>6}  verdict"
    print(header)
    print("-" * len(header))

    def mark(checks: dict, name: str) -> str:
        c = checks.get(name)
        if c is None:
            return "-"
        # "report" = computed but non-gating (e.g. DNSMOS/PESQ on synthetic-speech
        # fixtures): shown as "rep" so the trend is visible without implying a verdict.
        return {"pass": "OK", "fail": "FAIL", "skip": "skip", "report": "rep"}[c["status"]]

    for r in verdict["results"]:
        if "error" in r:
            print(f"{r['id']:<30}{r['class']:<28}  ERROR: {r['error']}")
            continue
        checks = r.get("checks", {})
        lvl = mark(checks, "leveler_variance")
        if lvl == "-":
            lvl = mark(checks, "music_segment_loudness")
        overall = "PASS" if r.get("overall_pass") else "FAIL"
        print(
            f"{r['id']:<30}{r['class']:<28}{mark(checks, 'loudness'):>6}{mark(checks, 'true_peak'):>5}"
            f"{mark(checks, 'dnsmos'):>8}{mark(checks, 'intrusive'):>8}{lvl:>6}  {overall}"
        )
    tail = f"\n{verdict['passed']}/{verdict['total']} passed"
    if not verdict["overall_pass"]:
        tail += "  -- MASTER-EVAL FAILED"
    print(tail)


def cmd_master_eval(args: argparse.Namespace) -> int:
    anvil_bin = resolve_anvil_bin(args.anvil)
    if anvil_bin is None:
        print(
            "skipped: anvil binary unavailable (build with `cargo build --bin cleanroom`, "
            "pass --anvil PATH, or set ANVIL_BIN)"
        )
        return 0

    ffmpeg = resolve_ffmpeg(args.ffmpeg)
    if ffmpeg is None:
        print("skipped: ffmpeg unavailable (needed for the loudness/true-peak cross-check)")
        return 0

    manifest_path = Path(args.fixtures) if args.fixtures else _default_eval_manifest()
    if not manifest_path.exists():
        print(
            f"missing fixtures manifest: {manifest_path}\n"
            "  generate one first with `python run.py synth`, or pass --fixtures",
            file=sys.stderr,
        )
        return 1

    clips = load_eval_clips(manifest_path)
    if args.classes:
        wanted = {c.strip() for c in args.classes.split(",")}
        clips = [c for c in clips if c["class"] in wanted]
    if not clips:
        print(f"no matching clips found in {manifest_path}", file=sys.stderr)
        return 1

    dnsmos_model = None
    if not args.skip_dnsmos:
        from metrics.dnsmos import DnsmosModel, dnsmos_available

        if dnsmos_available(args.dnsmos_model):
            try:
                dnsmos_model = DnsmosModel(args.dnsmos_model)
            except RuntimeError as exc:
                print(f"warning: DNSMOS unavailable ({exc}); skipping DNSMOS checks", file=sys.stderr)

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    results = []
    for clip in clips:
        entry = _master_eval_one(
            anvil_bin,
            ffmpeg,
            clip,
            args.preset,
            args.tier,
            args.target_lufs,
            args.true_peak_ceiling,
            out_dir,
            dnsmos_model,
            not args.skip_intrusive,
        )
        results.append(entry)
        if entry.get("unavailable"):
            # `master` itself is unimplemented — every remaining clip would hit the
            # exact same wall, so skip the whole run cleanly rather than repeat it N
            # times (06 exit-code contract: 0, not a failure).
            print(f"skipped: {entry['unavailable_reason']}")
            return 0

    verdict = {
        "fixtures_source": str(manifest_path),
        "preset": args.preset,
        "tier": args.tier,
        "target_lufs": args.target_lufs,
        "true_peak_ceiling_dbtp": args.true_peak_ceiling,
        "dnsmos_ran": dnsmos_model is not None,
        "total": len(results),
        "passed": sum(1 for r in results if r.get("overall_pass")),
        "failed": sum(1 for r in results if r.get("overall_pass") is False),
        "results": results,
    }
    verdict["overall_pass"] = verdict["failed"] == 0 and verdict["total"] > 0

    _print_master_eval_table(verdict)

    json_out = Path(args.json_out) if args.json_out else (out_dir / "master-eval.json")
    json_out.parent.mkdir(parents=True, exist_ok=True)
    json_out.write_text(json.dumps(verdict, indent=2), encoding="utf-8")
    print(f"\nverdict JSON written to {json_out}")

    return 0 if verdict["overall_pass"] else 1


# --- determinism: double-render `anvil master`, hash-compare (06 §2) ----------------


def cmd_determinism(args: argparse.Namespace) -> int:
    anvil_bin = resolve_anvil_bin(args.anvil)
    if anvil_bin is None:
        print(
            "skipped: anvil binary unavailable (build with `cargo build --bin cleanroom`, "
            "pass --anvil PATH, or set ANVIL_BIN)"
        )
        return 0

    manifest_path = Path(args.fixtures) if args.fixtures else _default_eval_manifest()
    if not manifest_path.exists():
        print(
            f"missing fixtures manifest: {manifest_path}\n"
            "  generate one first with `python run.py synth`, or pass --fixtures",
            file=sys.stderr,
        )
        return 1

    clips = load_eval_clips(manifest_path)
    if args.classes:
        wanted = {c.strip() for c in args.classes.split(",")}
        clips = [c for c in clips if c["class"] in wanted]
    if args.limit:
        clips = clips[: args.limit]
    if not clips:
        print(f"no matching clips found in {manifest_path}", file=sys.stderr)
        return 1

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    results = []
    for clip in clips:
        clip_dir = out_dir / clip["id"]
        clip_dir.mkdir(parents=True, exist_ok=True)
        out_a, out_b = clip_dir / "render-a.wav", clip_dir / "render-b.wav"

        res_a = run_master(anvil_bin, clip["_abs_path"], out_a, preset=args.preset, tier=args.tier)
        if res_a.unavailable:
            print(f"skipped: {res_a.unavailable_reason}")
            return 0
        res_b = run_master(anvil_bin, clip["_abs_path"], out_b, preset=args.preset, tier=args.tier)

        entry: dict = {"id": clip["id"], "class": clip["class"]}
        if not res_a.ok or not res_b.ok:
            entry["error"] = f"render failed (a.ok={res_a.ok}, b.ok={res_b.ok})"
            entry["pass"] = False
        else:
            hash_a, hash_b = _sha256_file(out_a), _sha256_file(out_b)
            entry["hash_a"], entry["hash_b"] = hash_a, hash_b
            entry["pass"] = hash_a == hash_b
        results.append(entry)

    passed = sum(1 for r in results if r.get("pass"))
    verdict = {
        "fixtures_source": str(manifest_path),
        "preset": args.preset,
        "tier": args.tier,
        "total": len(results),
        "passed": passed,
        "failed": len(results) - passed,
        "overall_pass": passed == len(results) and len(results) > 0,
        "results": results,
    }

    print(f"determinism: {passed}/{len(results)} identical double-renders")
    for r in results:
        mark = "PASS" if r.get("pass") else "FAIL"
        detail = r.get("error") or r.get("hash_a", "")[:16]
        print(f"  {r['id']:<30} {mark}  {detail}")

    json_out = Path(args.json_out) if args.json_out else (out_dir / "determinism.json")
    json_out.parent.mkdir(parents=True, exist_ok=True)
    json_out.write_text(json.dumps(verdict, indent=2), encoding="utf-8")
    print(f"\nverdict JSON written to {json_out}")

    return 0 if verdict["overall_pass"] else 1


# --- regress: per-version output hashes + metric deltas vs a stored baseline (06 §2) -

DEFAULT_REGRESSION_BASELINE = HERE / "reports" / "regression-baseline.json"
NOISE_BAND_LUFS = 0.2
NOISE_BAND_DBTP = 0.1
NOISE_BAND_DNSMOS = 0.1

REGRESSION_METRIC_BANDS = {
    "integrated_lufs": NOISE_BAND_LUFS,
    "true_peak_dbtp": NOISE_BAND_DBTP,
    "dnsmos_ovrl": NOISE_BAND_DNSMOS,
}


def cmd_regress(args: argparse.Namespace) -> int:
    anvil_bin = resolve_anvil_bin(args.anvil)
    if anvil_bin is None:
        print(
            "skipped: anvil binary unavailable (build with `cargo build --bin cleanroom`, "
            "pass --anvil PATH, or set ANVIL_BIN)"
        )
        return 0

    ffmpeg = resolve_ffmpeg(args.ffmpeg)
    if ffmpeg is None:
        print("skipped: ffmpeg unavailable (needed for the loudness cross-check)")
        return 0

    manifest_path = Path(args.fixtures) if args.fixtures else _default_eval_manifest()
    if not manifest_path.exists():
        print(
            f"missing fixtures manifest: {manifest_path}\n"
            "  generate one first with `python run.py synth`, or pass --fixtures",
            file=sys.stderr,
        )
        return 1

    clips = load_eval_clips(manifest_path)
    if args.classes:
        wanted = {c.strip() for c in args.classes.split(",")}
        clips = [c for c in clips if c["class"] in wanted]
    if not clips:
        print(f"no matching clips found in {manifest_path}", file=sys.stderr)
        return 1

    dnsmos_model = None
    from metrics.dnsmos import DnsmosModel, dnsmos_available

    if dnsmos_available():
        try:
            dnsmos_model = DnsmosModel()
        except RuntimeError:
            dnsmos_model = None

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    current: dict[str, dict] = {}
    for clip in clips:
        clip_dir = out_dir / clip["id"]
        clip_dir.mkdir(parents=True, exist_ok=True)
        out_wav = clip_dir / "mastered.wav"

        res = run_master(anvil_bin, clip["_abs_path"], out_wav, preset=args.preset, tier=args.tier)
        if res.unavailable:
            print(f"skipped: {res.unavailable_reason}")
            return 0
        if not res.ok:
            current[clip["id"]] = {"error": f"exit {res.returncode}"}
            continue

        entry: dict = {"hash": _sha256_file(out_wav)}
        try:
            m = measure_lufs_ffmpeg(out_wav, ffmpeg=ffmpeg)
            entry["integrated_lufs"] = m.integrated_lufs
            entry["true_peak_dbtp"] = m.true_peak_dbtp
        except RuntimeError as exc:
            entry["loudness_error"] = str(exc)
        if dnsmos_model is not None:
            try:
                entry["dnsmos_ovrl"] = dnsmos_model.score(out_wav).ovrl
            except Exception as exc:  # noqa: BLE001
                entry["dnsmos_error"] = str(exc)
        current[clip["id"]] = entry

    baseline_path = Path(args.baseline)
    if args.update_baseline:
        baseline_path.parent.mkdir(parents=True, exist_ok=True)
        baseline_path.write_text(
            json.dumps({"clips": current}, indent=2), encoding="utf-8"
        )
        print(f"baseline updated: {baseline_path} ({len(current)} clips)")
        return 0

    if not baseline_path.exists():
        print(
            f"no baseline found at {baseline_path}; run with --update-baseline first",
            file=sys.stderr,
        )
        return 1
    baseline = json.loads(baseline_path.read_text(encoding="utf-8")).get("clips", {})

    rows = []
    for clip_id, cur in current.items():
        base = baseline.get(clip_id)
        if base is None:
            rows.append({"id": clip_id, "status": "NEW", "detail": "no baseline entry (not a regression)"})
            continue
        if "error" in cur:
            rows.append({"id": clip_id, "status": "FAIL", "detail": cur["error"]})
            continue

        issues = []
        for key, band in REGRESSION_METRIC_BANDS.items():
            if key in cur and key in base:
                delta = cur[key] - base[key]
                if abs(delta) > band:
                    issues.append(f"{key} moved {delta:+.3f} (band +/-{band})")

        if issues:
            rows.append({"id": clip_id, "status": "FAIL", "detail": "; ".join(issues)})
        else:
            hash_note = "unchanged" if cur.get("hash") == base.get("hash") else "changed (within noise band)"
            rows.append({"id": clip_id, "status": "PASS", "detail": f"hash {hash_note}"})

    regressions = sum(1 for r in rows if r["status"] == "FAIL")
    verdict = {
        "baseline": str(baseline_path),
        "fixtures_source": str(manifest_path),
        "total": len(current),
        "regressions": regressions,
        "overall_pass": regressions == 0,
        "rows": rows,
    }

    print(f"regress: {len(current)} clips vs baseline {baseline_path}")
    for r in rows:
        print(f"  {r['id']:<30} {r['status']:<5} {r['detail']}")

    json_out = Path(args.json_out) if args.json_out else (out_dir / "regress.json")
    json_out.parent.mkdir(parents=True, exist_ok=True)
    json_out.write_text(json.dumps(verdict, indent=2), encoding="utf-8")
    print(f"\nverdict JSON written to {json_out}")

    return 0 if verdict["overall_pass"] else 1


def main() -> int:
    parser = argparse.ArgumentParser(description="ANVIL eval harness")
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("smoke", help="validate the example manifest (CI eval-smoke)")

    v = sub.add_parser("validate", help="validate a corpus manifest")
    v.add_argument("--manifest", required=True)

    f = sub.add_parser(
        "fixtures", help="generate synthetic loudness fixtures for self-testing (needs ffmpeg)"
    )
    f.add_argument(
        "--out",
        default=str(HERE / "corpus" / "fixtures"),
        help="output directory (default eval/corpus/fixtures)",
    )
    f.add_argument("--ffmpeg", help="path to ffmpeg binary (else ANVIL_FFMPEG, else PATH)")

    c = sub.add_parser(
        "conformance",
        help="cross-check ffmpeg ebur128 vs `anvil analyze --json` on fixtures (needs ffmpeg)",
    )
    c.add_argument(
        "--fixtures",
        default=str(HERE / "corpus" / "fixtures"),
        help="directory of *.wav fixtures, or a manifest JSON (default eval/corpus/fixtures)",
    )
    c.add_argument(
        "--anvil-json",
        help="JSON of `anvil analyze --json` measurements keyed by fixture id "
        "(omit to only run the ffmpeg self-measure, without a cross-check)",
    )
    c.add_argument(
        "--ceiling",
        type=float,
        default=DEFAULT_TRUE_PEAK_CEILING_DBTP,
        help=f"true-peak ceiling in dBTP (default {DEFAULT_TRUE_PEAK_CEILING_DBTP})",
    )
    c.add_argument(
        "--tolerance",
        type=float,
        default=DEFAULT_LUFS_TOLERANCE,
        help=f"integrated-loudness cross-check tolerance in LU (default {DEFAULT_LUFS_TOLERANCE})",
    )
    c.add_argument("--ffmpeg", help="path to ffmpeg binary (else ANVIL_FFMPEG, else PATH)")
    c.add_argument("--json-out", help="write the machine-readable verdict JSON to this path")

    s = sub.add_parser(
        "synth",
        help="generate a synthetic paired-degradation corpus (needs numpy/scipy/soundfile)",
    )
    s.add_argument(
        "--out",
        default=str(HERE / "corpus" / "synth"),
        help="output directory (default eval/corpus/synth)",
    )
    s.add_argument(
        "--classes",
        help="comma-separated subset of failure classes (default: all synthesizable classes)",
    )
    s.add_argument("--variants-per-class", type=int, default=3, help="clips per class (default 3)")
    s.add_argument("--duration", type=float, default=20.0, help="clip duration in seconds (default 20)")
    s.add_argument("--seed", type=int, default=1234, help="base RNG seed (default 1234)")
    s.add_argument("--clean-dir", help="folder of real clean *.wav files to use instead of synthesizing speech")
    s.add_argument("--noise-dir", help="folder of real noise *.wav files to use instead of synthesizing noise")

    def _add_common_master_args(p: argparse.ArgumentParser) -> None:
        p.add_argument("--anvil", help="path to the anvil CLI binary (else ANVIL_BIN, else PATH/target dirs)")
        p.add_argument(
            "--fixtures",
            help="a corpus/synth manifest JSON (default: eval/corpus/synth/manifest.json if it "
            "exists, else eval/corpus/manifest.example.json)",
        )
        p.add_argument("--preset", default="podcast-stereo", help="preset name (default podcast-stereo)")
        p.add_argument(
            "--tier", default="standard", choices=["fast", "standard", "studio"], help="render tier (default standard)"
        )
        p.add_argument("--classes", help="comma-separated subset of failure classes to run")
        p.add_argument("--json-out", help="write the machine-readable verdict JSON to this path")

    me = sub.add_parser(
        "master-eval",
        help="run `anvil master` on fixtures and assert the M1 gates (06 §2; needs ffmpeg + anvil master)",
    )
    _add_common_master_args(me)
    me.add_argument("--ffmpeg", help="path to ffmpeg binary (else ANVIL_FFMPEG, else PATH)")
    me.add_argument(
        "--target-lufs", type=float, default=DEFAULT_TARGET_LUFS, help=f"loudness target (default {DEFAULT_TARGET_LUFS})"
    )
    me.add_argument(
        "--true-peak-ceiling",
        type=float,
        default=DEFAULT_TRUE_PEAK_CEILING_DBTP,
        help=f"true-peak ceiling in dBTP (default {DEFAULT_TRUE_PEAK_CEILING_DBTP})",
    )
    me.add_argument("--dnsmos-model", help="path to the DNSMOS ONNX model (default eval/models/sig_bak_ovr.onnx)")
    me.add_argument("--skip-dnsmos", action="store_true", help="skip the DNSMOS gate even if the model is available")
    me.add_argument(
        "--skip-intrusive", action="store_true", help="skip the PESQ/STOI gate even if pesq/pystoi are available"
    )
    me.add_argument(
        "--out-dir",
        default=str(HERE / "reports" / "master-eval"),
        help="where mastered wavs/reports go (default eval/reports/master-eval)",
    )

    d = sub.add_parser(
        "determinism",
        help="double-render `anvil master` on fixtures and hash-compare (06 §2; needs anvil master)",
    )
    _add_common_master_args(d)
    d.add_argument("--limit", type=int, help="only double-render the first N matching clips")
    d.add_argument(
        "--out-dir",
        default=str(HERE / "reports" / "determinism"),
        help="where double-rendered wavs go (default eval/reports/determinism)",
    )

    r = sub.add_parser(
        "regress",
        help="compare current `anvil master` output hashes/metrics vs a stored baseline (06 §2)",
    )
    _add_common_master_args(r)
    r.add_argument("--ffmpeg", help="path to ffmpeg binary (else ANVIL_FFMPEG, else PATH)")
    r.add_argument(
        "--baseline",
        default=str(DEFAULT_REGRESSION_BASELINE),
        help=f"baseline JSON path (default {DEFAULT_REGRESSION_BASELINE})",
    )
    r.add_argument("--update-baseline", action="store_true", help="write current metrics as the new baseline and exit")
    r.add_argument(
        "--out-dir",
        default=str(HERE / "reports" / "regress"),
        help="where rendered wavs go (default eval/reports/regress)",
    )

    args = parser.parse_args()
    if args.cmd == "smoke":
        return cmd_smoke(args)
    if args.cmd == "validate":
        return cmd_validate(args)
    if args.cmd == "fixtures":
        return cmd_fixtures(args)
    if args.cmd == "conformance":
        return cmd_conformance(args)
    if args.cmd == "synth":
        return cmd_synth(args)
    if args.cmd == "master-eval":
        return cmd_master_eval(args)
    if args.cmd == "determinism":
        return cmd_determinism(args)
    if args.cmd == "regress":
        return cmd_regress(args)
    parser.error("unknown command")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
