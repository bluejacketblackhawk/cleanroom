"""Tests for eval/anvil_cli.py: resolving the `anvil` binary and parsing its output.

All hermetic — `subprocess.run` is monkeypatched wherever a real invocation would be
needed, and `shutil.which`/filesystem checks are exercised against `tmp_path`. Nothing
here needs a real `anvil` binary (which doesn't exist in most dev/CI environments until
another lane wires up `master`).
"""

from __future__ import annotations

import json

import pytest

import anvil_cli
from anvil_cli import (
    AnvilRunResult,
    resolve_anvil_bin,
    run_analyze,
    run_master,
)


class _FakeCompletedProcess:
    def __init__(self, returncode: int, stdout: str = "", stderr: str = "") -> None:
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


# --- resolve_anvil_bin ----------------------------------------------------------------


def test_resolve_prefers_explicit_path(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(anvil_cli.shutil, "which", lambda cmd: f"/resolved/{cmd}")
    monkeypatch.setenv("ANVIL_BIN", "env-anvil")
    assert resolve_anvil_bin("explicit-anvil") == "/resolved/explicit-anvil"


def test_resolve_falls_back_to_env_var(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(anvil_cli.shutil, "which", lambda cmd: f"/resolved/{cmd}")
    monkeypatch.setenv("ANVIL_BIN", "env-anvil")
    assert resolve_anvil_bin(None) == "/resolved/env-anvil"


def test_resolve_env_var_as_plain_file_path(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    # ANVIL_BIN given as a path that `which` can't resolve but exists as a file.
    fake_bin = tmp_path / "anvil-custom"
    fake_bin.write_bytes(b"")
    monkeypatch.setattr(anvil_cli.shutil, "which", lambda cmd: None)
    monkeypatch.setenv("ANVIL_BIN", str(fake_bin))
    assert resolve_anvil_bin(None) == str(fake_bin)


def test_resolve_falls_back_to_path(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("ANVIL_BIN", raising=False)

    def fake_which(cmd: str) -> str | None:
        return "/usr/bin/anvil" if cmd == "anvil" else None

    monkeypatch.setattr(anvil_cli.shutil, "which", fake_which)
    assert resolve_anvil_bin(None) == "/usr/bin/anvil"


def test_resolve_falls_back_to_cargo_build_dir(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    monkeypatch.delenv("ANVIL_BIN", raising=False)
    monkeypatch.setattr(anvil_cli.shutil, "which", lambda cmd: None)
    monkeypatch.setattr(anvil_cli, "REPO_ROOT", tmp_path)
    monkeypatch.setattr(anvil_cli, "_BIN_NAMES", ("anvil.exe",))

    debug_dir = tmp_path / "target" / "debug"
    debug_dir.mkdir(parents=True)
    bin_path = debug_dir / "anvil.exe"
    bin_path.write_bytes(b"")

    assert resolve_anvil_bin(None) == str(bin_path)


def test_resolve_returns_none_when_nothing_found(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    monkeypatch.delenv("ANVIL_BIN", raising=False)
    monkeypatch.setattr(anvil_cli.shutil, "which", lambda cmd: None)
    monkeypatch.setattr(anvil_cli, "REPO_ROOT", tmp_path)  # empty dir, no target/
    assert resolve_anvil_bin(None) is None
    assert anvil_cli.anvil_available(None) is False


# --- run_analyze ------------------------------------------------------------------------


def test_run_analyze_parses_json(monkeypatch: pytest.MonkeyPatch) -> None:
    payload = {"integrated_lufs": -16.0, "true_peak_dbtp": -3.0, "loudness_range_lu": 1.0}
    fake_proc = _FakeCompletedProcess(0, stdout=json.dumps(payload))
    monkeypatch.setattr(anvil_cli.subprocess, "run", lambda *a, **k: fake_proc)

    result = run_analyze("anvil", "clip.wav")
    assert result.ok is True
    assert result.unavailable is False
    assert result.report == payload


def test_run_analyze_detects_unimplemented(monkeypatch: pytest.MonkeyPatch) -> None:
    body = {"status": "unimplemented", "command": "analyze", "chain_version": "0.1.0"}
    fake_proc = _FakeCompletedProcess(70, stdout=json.dumps(body))
    monkeypatch.setattr(anvil_cli.subprocess, "run", lambda *a, **k: fake_proc)

    result = run_analyze("anvil", "clip.wav")
    assert result.unavailable is True
    assert result.ok is False


def test_run_analyze_nonzero_exit_is_a_real_failure(monkeypatch: pytest.MonkeyPatch) -> None:
    fake_proc = _FakeCompletedProcess(1, stdout="", stderr="decode error")
    monkeypatch.setattr(anvil_cli.subprocess, "run", lambda *a, **k: fake_proc)

    result = run_analyze("anvil", "clip.wav")
    assert result.ok is False
    assert result.unavailable is False
    assert result.returncode == 1


def test_run_analyze_bad_json_is_a_failure_not_a_crash(monkeypatch: pytest.MonkeyPatch) -> None:
    fake_proc = _FakeCompletedProcess(0, stdout="not json")
    monkeypatch.setattr(anvil_cli.subprocess, "run", lambda *a, **k: fake_proc)

    result = run_analyze("anvil", "clip.wav")
    assert result.ok is False
    assert result.report is None


# --- run_master -------------------------------------------------------------------------


def test_run_master_builds_expected_command(monkeypatch: pytest.MonkeyPatch) -> None:
    captured = {}

    def fake_run(cmd, **kwargs):
        captured["cmd"] = cmd
        return _FakeCompletedProcess(0)

    monkeypatch.setattr(anvil_cli.subprocess, "run", fake_run)
    run_master("anvil", "in.wav", "out.wav", preset="podcast-stereo", tier="standard", report_path="report.json")

    assert captured["cmd"] == [
        "anvil", "master", "in.wav", "-o", "out.wav",
        "--preset", "podcast-stereo", "--tier", "standard", "--report", "report.json",
    ]


def test_run_master_omits_optional_flags(monkeypatch: pytest.MonkeyPatch) -> None:
    captured = {}

    def fake_run(cmd, **kwargs):
        captured["cmd"] = cmd
        return _FakeCompletedProcess(0)

    monkeypatch.setattr(anvil_cli.subprocess, "run", fake_run)
    run_master("anvil", "in.wav", "out.wav")

    assert captured["cmd"] == ["anvil", "master", "in.wav", "-o", "out.wav"]


def test_run_master_detects_unimplemented_scaffold(monkeypatch: pytest.MonkeyPatch) -> None:
    body = {"status": "unimplemented", "command": "master", "chain_version": "0.1.0", "detail": {}}
    fake_proc = _FakeCompletedProcess(70, stdout=json.dumps(body))
    monkeypatch.setattr(anvil_cli.subprocess, "run", lambda *a, **k: fake_proc)

    result = run_master("anvil", "in.wav", "out.wav")
    assert result.unavailable is True
    assert result.ok is False
    assert "unimplemented" in result.unavailable_reason
    assert "0.1.0" in result.unavailable_reason


def test_run_master_unimplemented_without_parseable_json_still_detected(monkeypatch: pytest.MonkeyPatch) -> None:
    fake_proc = _FakeCompletedProcess(70, stdout="not json at all")
    monkeypatch.setattr(anvil_cli.subprocess, "run", lambda *a, **k: fake_proc)

    result = run_master("anvil", "in.wav", "out.wav")
    assert result.unavailable is True


def test_run_master_real_failure_is_not_unavailable(monkeypatch: pytest.MonkeyPatch) -> None:
    fake_proc = _FakeCompletedProcess(1, stderr="panicked")
    monkeypatch.setattr(anvil_cli.subprocess, "run", lambda *a, **k: fake_proc)

    result = run_master("anvil", "in.wav", "out.wav")
    assert result.ok is False
    assert result.unavailable is False
    assert result.returncode == 1


def test_run_master_reads_back_report_json(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    report_path = tmp_path / "report.json"
    report_body = {
        "analysis": {}, "before": {"integrated_lufs": -25.0}, "after": {"integrated_lufs": -16.0},
        "preset": "podcast-stereo", "tier": "standard", "modules": [], "health_card": [],
    }
    report_path.write_text(json.dumps(report_body), encoding="utf-8")
    monkeypatch.setattr(anvil_cli.subprocess, "run", lambda *a, **k: _FakeCompletedProcess(0))

    result = run_master("anvil", "in.wav", "out.wav", report_path=report_path)
    assert result.ok is True
    assert result.report == report_body


def test_run_master_missing_report_file_is_not_fatal(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    # anvil exited 0 but, e.g., didn't write the report for some reason: don't crash.
    monkeypatch.setattr(anvil_cli.subprocess, "run", lambda *a, **k: _FakeCompletedProcess(0))
    result = run_master("anvil", "in.wav", "out.wav", report_path=tmp_path / "missing.json")
    assert result.ok is True
    assert result.report is None
