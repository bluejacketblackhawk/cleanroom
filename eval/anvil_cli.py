"""Subprocess wrapper around the Rust `anvil` CLI (crates/anvil-cli).

`master` is scaffolded but returns `emit_unimplemented` (exit code 70, a JSON body with
`"status": "unimplemented"`) until another lane wires up the real DSP chain. Everything
in this module is written so that state is a *clean skip*, not a crash: resolve the
binary the same way `metrics.loudness.resolve_ffmpeg` resolves ffmpeg (explicit path ->
env var -> PATH -> common cargo build dirs), and treat exit-70/status-unimplemented as
"unavailable", not "failed" — so `run.py master-eval`/`determinism` light up
automatically the moment the real command lands, with zero changes needed here.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent

EXIT_UNIMPLEMENTED = 70

# Where `cargo build [--release] --bin anvil` puts the binary, relative to the repo
# root, on each platform. Checked in order after explicit/env/PATH resolution fails.
_CANDIDATE_BUILD_DIRS = ("target/debug", "target/release")
_BIN_NAMES = ("cleanroom.exe", "cleanroom") if os.name == "nt" else ("cleanroom",)


def resolve_anvil_bin(explicit: str | os.PathLike[str] | None = None) -> str | None:
    """Resolve the `anvil` CLI binary, or return None if nothing usable is found.

    Resolution order: an explicit path (e.g. a `--anvil` CLI flag), the `ANVIL_BIN` env
    var, `anvil`/`anvil.exe` on PATH, then the standard cargo build output dirs
    (`target/debug`, `target/release`) under the repo root. Never raises.
    """
    for candidate in (explicit, os.environ.get("ANVIL_BIN")):
        if not candidate:
            continue
        found = shutil.which(str(candidate))
        if found:
            return found
        # An explicit path that isn't found via PATH lookup but exists as a plain file
        # (e.g. a full path with no execute bit checks on Windows) still counts.
        p = Path(candidate)
        if p.is_file():
            return str(p)

    found = shutil.which("cleanroom")
    if found:
        return found

    for build_dir in _CANDIDATE_BUILD_DIRS:
        for name in _BIN_NAMES:
            candidate_path = REPO_ROOT / build_dir / name
            if candidate_path.is_file():
                return str(candidate_path)

    return None


def anvil_available(explicit: str | os.PathLike[str] | None = None) -> bool:
    return resolve_anvil_bin(explicit) is not None


@dataclass
class AnvilRunResult:
    """Result of one `anvil <subcommand>` invocation."""

    ok: bool  # command ran and exited 0
    unavailable: bool  # command is scaffolded-but-unimplemented (exit 70) or binary missing
    unavailable_reason: str | None
    returncode: int | None
    stdout: str
    stderr: str
    report: dict | None  # parsed --report JSON, if requested and produced


def run_analyze(
    anvil_bin: str,
    input_path: str | os.PathLike[str],
    timeout_s: float = 120.0,
) -> AnvilRunResult:
    """`anvil analyze <input> --json` -> parsed AnalysisReport dict (via stdout)."""
    proc = subprocess.run(
        [anvil_bin, "analyze", str(input_path), "--json"],
        capture_output=True,
        text=True,
        check=False,
        timeout=timeout_s,
    )
    if proc.returncode == EXIT_UNIMPLEMENTED:
        return AnvilRunResult(
            ok=False,
            unavailable=True,
            unavailable_reason="anvil CLI reports 'analyze' unimplemented (exit 70)",
            returncode=proc.returncode,
            stdout=proc.stdout,
            stderr=proc.stderr,
            report=None,
        )
    if proc.returncode != 0:
        return AnvilRunResult(
            ok=False,
            unavailable=False,
            unavailable_reason=None,
            returncode=proc.returncode,
            stdout=proc.stdout,
            stderr=proc.stderr,
            report=None,
        )
    try:
        report = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        return AnvilRunResult(
            ok=False,
            unavailable=False,
            unavailable_reason=None,
            returncode=proc.returncode,
            stdout=proc.stdout,
            stderr=f"{proc.stderr}\n(failed to parse --json stdout: {exc})",
            report=None,
        )
    return AnvilRunResult(
        ok=True,
        unavailable=False,
        unavailable_reason=None,
        returncode=proc.returncode,
        stdout=proc.stdout,
        stderr=proc.stderr,
        report=report,
    )


def run_master(
    anvil_bin: str,
    input_path: str | os.PathLike[str],
    out_path: str | os.PathLike[str],
    preset: str | None = None,
    tier: str | None = None,
    report_path: str | os.PathLike[str] | None = None,
    timeout_s: float = 600.0,
) -> AnvilRunResult:
    """`anvil master <input> -o <out> [--preset NAME] [--tier fast|standard|studio]
    [--report PATH]`.

    Detects the M0 scaffold's "unimplemented" response (exit 70, JSON
    `{"status": "unimplemented", ...}` on stdout) and reports it via
    `unavailable=True` rather than `ok=False`, so callers can skip the gate cleanly
    instead of treating it as a real failure. If `report_path` is given and the command
    succeeds, the MasterReport JSON is read back and attached as `.report`.
    """
    cmd = [anvil_bin, "master", str(input_path), "-o", str(out_path)]
    if preset:
        cmd += ["--preset", preset]
    if tier:
        cmd += ["--tier", tier]
    if report_path:
        cmd += ["--report", str(report_path)]

    proc = subprocess.run(cmd, capture_output=True, text=True, check=False, timeout=timeout_s)

    if proc.returncode == EXIT_UNIMPLEMENTED:
        reason = "anvil CLI reports 'master' unimplemented (exit 70)"
        try:
            body = json.loads(proc.stdout)
            if body.get("status") == "unimplemented":
                reason = (
                    "anvil CLI reports 'master' unimplemented "
                    f"(chain_version={body.get('chain_version')!r})"
                )
        except json.JSONDecodeError:
            pass
        return AnvilRunResult(
            ok=False,
            unavailable=True,
            unavailable_reason=reason,
            returncode=proc.returncode,
            stdout=proc.stdout,
            stderr=proc.stderr,
            report=None,
        )

    if proc.returncode != 0:
        return AnvilRunResult(
            ok=False,
            unavailable=False,
            unavailable_reason=None,
            returncode=proc.returncode,
            stdout=proc.stdout,
            stderr=proc.stderr,
            report=None,
        )

    report = None
    if report_path is not None:
        rp = Path(report_path)
        if rp.is_file():
            try:
                report = json.loads(rp.read_text(encoding="utf-8"))
            except json.JSONDecodeError as exc:
                return AnvilRunResult(
                    ok=False,
                    unavailable=False,
                    unavailable_reason=None,
                    returncode=proc.returncode,
                    stdout=proc.stdout,
                    stderr=f"{proc.stderr}\n(failed to parse --report JSON: {exc})",
                    report=None,
                )

    return AnvilRunResult(
        ok=True,
        unavailable=False,
        unavailable_reason=None,
        returncode=proc.returncode,
        stdout=proc.stdout,
        stderr=proc.stderr,
        report=report,
    )
