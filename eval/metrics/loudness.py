"""Loudness / true-peak metrics.

Gate (06 QUALITY-EVAL.md §2): integrated loudness within +/-0.5 LU of target on 100% of
corpus; true peak <= ceiling with zero tolerance. In M1 this is measured two ways and
cross-checked: ANVIL's own ebur128 self-measure (the Rust `anvil analyze --json` engine)
vs ffmpeg's `ebur128` filter as an independent reference. This module owns the ffmpeg
side of that cross-check; `run.py conformance` drives the comparison.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path


@dataclass
class LoudnessMeasurement:
    """A single ebur128 measurement. Field names match what `anvil analyze --json`
    is expected to emit per file (integrated_lufs / true_peak_dbtp / loudness_range_lu),
    so ANVIL measurement JSON can be loaded straight into this shape."""

    integrated_lufs: float
    true_peak_dbtp: float
    loudness_range_lu: float


def resolve_ffmpeg(explicit: str | os.PathLike[str] | None = None) -> str | None:
    """Resolve an ffmpeg binary, or return None if nothing usable is found.

    Resolution order: an explicitly passed path (e.g. a `--ffmpeg` CLI flag), the
    `ANVIL_FFMPEG` env var, then whatever `ffmpeg` resolves to on PATH.
    `shutil.which` already does the right thing for both bare command names (PATH
    search) and full paths (existence + executability check), including on Windows
    where it resolves `ffmpeg` -> `ffmpeg.exe` via PATHEXT. Never raises, so callers
    can implement a clean "skipped: ffmpeg unavailable" exit rather than a crash.
    """
    for candidate in (explicit, os.environ.get("ANVIL_FFMPEG"), "ffmpeg"):
        if not candidate:
            continue
        found = shutil.which(str(candidate))
        if found:
            return found
    return None


def ffmpeg_available(explicit: str | os.PathLike[str] | None = None) -> bool:
    """True if `resolve_ffmpeg` can find a usable ffmpeg binary."""
    return resolve_ffmpeg(explicit) is not None


def parse_ebur128_summary(stderr_text: str) -> LoudnessMeasurement:
    """Parse integrated loudness / true peak / LRA out of ffmpeg ebur128 stderr.

    Split out from `measure_lufs_ffmpeg` so it can be unit-tested against a captured
    sample of real ffmpeg output without needing ffmpeg installed at all.

    Only the final "Summary:" block is parsed (via `rfind`, so the *last* occurrence
    wins). This matters: when the ebur128 filter's `framelog` isn't `quiet`, ffmpeg
    emits one progress line per ~100ms containing the same field labels (`I:`, `LRA:`,
    `TPK:`) ahead of the summary, and a naive first-match search would silently grab a
    mid-stream reading instead of the final integrated result. We always invoke ffmpeg
    with `framelog=quiet`, but the parser stays defensive regardless.
    """
    marker = "Summary:"
    idx = stderr_text.rfind(marker)
    if idx == -1:
        raise RuntimeError(
            "could not find an ebur128 'Summary:' block in ffmpeg output "
            f"(got {len(stderr_text)} bytes of stderr; is this valid audio input?)"
        )
    summary = stderr_text[idx:]

    def grab(pattern: str, label: str) -> float:
        m = re.search(pattern, summary)
        if not m:
            raise RuntimeError(f"could not parse {label} from ebur128 summary block")
        return float(m.group(1))

    return LoudnessMeasurement(
        integrated_lufs=grab(r"I:\s*(-?\d+\.?\d*)\s*LUFS", "integrated loudness (I)"),
        true_peak_dbtp=grab(r"Peak:\s*(-?\d+\.?\d*)\s*dBFS", "true peak"),
        loudness_range_lu=grab(r"LRA:\s*(-?\d+\.?\d*)\s*LU", "loudness range (LRA)"),
    )


def measure_lufs_ffmpeg(
    path: str | os.PathLike[str],
    ffmpeg: str | os.PathLike[str] | None = None,
) -> LoudnessMeasurement:
    """Cross-check reference via ffmpeg's ebur128 filter (I / LRA / true-peak).

    `ffmpeg` is resolved via `resolve_ffmpeg` (explicit path -> ANVIL_FFMPEG ->
    PATH). Raises RuntimeError with a clear message if ffmpeg can't be found, the
    input file doesn't exist, or the summary output can't be parsed — never lets a
    subprocess/regex exception leak out raw.
    """
    input_path = Path(path)
    if not input_path.exists():
        raise RuntimeError(f"input file not found: {input_path}")

    resolved = resolve_ffmpeg(ffmpeg)
    if resolved is None:
        raise RuntimeError(
            "ffmpeg not found: pass --ffmpeg, set the ANVIL_FFMPEG env var, "
            "or install ffmpeg on PATH"
        )

    proc = subprocess.run(
        [
            resolved,
            "-hide_banner",
            "-nostats",
            "-i",
            str(input_path),
            "-filter_complex",
            "ebur128=peak=true:framelog=quiet",
            "-f",
            "null",
            "-",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    # ffmpeg writes the ebur128 summary to stderr regardless of exit code; parse it
    # first so a real parse failure reports what ffmpeg actually said.
    try:
        return parse_ebur128_summary(proc.stderr)
    except RuntimeError as exc:
        if proc.returncode != 0:
            raise RuntimeError(
                f"ffmpeg exited {proc.returncode} measuring {input_path}: "
                f"{proc.stderr[-800:]}"
            ) from exc
        raise
