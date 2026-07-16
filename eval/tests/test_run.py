"""Tests for eval/run.py: manifest validation/coverage (the existing M0 scaffold)
and the conformance/fixtures machinery added for the M0 exit gate.

All hermetic — `measure_lufs_ffmpeg` is monkeypatched wherever the conformance
comparison logic is exercised, so nothing here needs a real ffmpeg binary.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import pytest

import run
from anvil_cli import AnvilRunResult
from metrics.loudness import LoudnessMeasurement


# --- manifest validation (pre-existing behavior, still covered) ---------------------


def test_smoke_manifest_is_valid() -> None:
    manifest = run.load_manifest(run.HERE / "corpus" / "manifest.example.json")
    assert run.validate(manifest) == []


def test_validate_flags_missing_fields() -> None:
    manifest = {"clips": [{"id": "x", "class": "clean-studio"}]}
    errors = run.validate(manifest)
    assert any("missing fields" in e for e in errors)


def test_validate_flags_duplicate_id() -> None:
    clip = {
        "id": "dup",
        "class": "clean-studio",
        "path": "a.wav",
        "license": "CC0-1.0",
        "redistributable": True,
    }
    manifest = {"clips": [clip, dict(clip)]}
    errors = run.validate(manifest)
    assert any("duplicate id" in e for e in errors)


def test_validate_flags_unknown_class() -> None:
    manifest = {
        "clips": [
            {
                "id": "x",
                "class": "not-a-real-class",
                "path": "a.wav",
                "license": "CC0-1.0",
                "redistributable": True,
            }
        ]
    }
    errors = run.validate(manifest)
    assert any("unknown class" in e for e in errors)


def test_validate_flags_non_bool_redistributable() -> None:
    manifest = {
        "clips": [
            {
                "id": "x",
                "class": "clean-studio",
                "path": "a.wav",
                "license": "CC0-1.0",
                "redistributable": "yes",
            }
        ]
    }
    errors = run.validate(manifest)
    assert any("must be a boolean" in e for e in errors)


def test_class_coverage_counts_known_classes_only() -> None:
    manifest = {
        "clips": [
            {"class": "clean-studio"},
            {"class": "clean-studio"},
            {"class": "not-a-class"},
        ]
    }
    coverage = run.class_coverage(manifest)
    assert coverage["clean-studio"] == 2
    assert "not-a-class" not in coverage
    assert coverage["hum-50-60hz"] == 0


# --- fixtures/manifest loading --------------------------------------------------


def test_load_fixtures_from_directory(tmp_path) -> None:
    (tmp_path / "b-fixture.wav").write_bytes(b"")
    (tmp_path / "a-fixture.wav").write_bytes(b"")
    (tmp_path / "not-audio.txt").write_bytes(b"")

    fixtures = run.load_fixtures(tmp_path, default_ceiling=-1.0)

    assert [f.id for f in fixtures] == ["a-fixture", "b-fixture"]
    assert all(f.ceiling_dbtp == -1.0 for f in fixtures)


def test_load_fixtures_from_manifest_applies_ceiling_override(tmp_path) -> None:
    (tmp_path / "loud.wav").write_bytes(b"")
    manifest = {
        "clips": [
            {"id": "loud-clip", "path": "loud.wav", "true_peak_ceiling_dbtp": -2.0, "synthetic": True},
        ]
    }
    manifest_path = tmp_path / "fixtures.json"
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")

    fixtures = run.load_fixtures(manifest_path, default_ceiling=-1.0)

    assert len(fixtures) == 1
    fx = fixtures[0]
    assert fx.id == "loud-clip"
    assert fx.ceiling_dbtp == -2.0
    assert fx.synthetic is True
    assert fx.wav_path == (tmp_path / "loud.wav").resolve()


def test_load_fixtures_missing_path_raises(tmp_path) -> None:
    with pytest.raises(FileNotFoundError):
        run.load_fixtures(tmp_path / "nope", default_ceiling=-1.0)


def test_load_anvil_measurements(tmp_path) -> None:
    path = tmp_path / "anvil.json"
    path.write_text(
        json.dumps(
            {
                "clip-a": {
                    "integrated_lufs": -16.03,
                    "true_peak_dbtp": -3.02,
                    "loudness_range_lu": 1.1,
                }
            }
        ),
        encoding="utf-8",
    )
    measurements = run.load_anvil_measurements(path)
    assert measurements["clip-a"] == LoudnessMeasurement(
        integrated_lufs=-16.03, true_peak_dbtp=-3.02, loudness_range_lu=1.1
    )


# --- conformance verdict logic (the M0 exit gate comparison) ------------------------


def test_conformance_verdict_passes_within_tolerance(monkeypatch: pytest.MonkeyPatch) -> None:
    fake = LoudnessMeasurement(integrated_lufs=-16.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0)
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: fake)

    fixture = run.Fixture(id="clip-a", wav_path="clip-a.wav", ceiling_dbtp=-1.0)
    anvil = {"clip-a": LoudnessMeasurement(-16.05, -3.05, 1.0)}  # 0.05 LU off, under 0.1 tolerance

    verdict = run._conformance_verdict([fixture], "ffmpeg", anvil, tolerance=0.1, fixtures_source="x")

    assert verdict["overall_pass"] is True
    assert verdict["results"][0]["lufs_within_tolerance"] is True
    assert verdict["results"][0]["true_peak_ok"] is True


def test_conformance_verdict_fails_outside_tolerance(monkeypatch: pytest.MonkeyPatch) -> None:
    fake = LoudnessMeasurement(integrated_lufs=-16.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0)
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: fake)

    fixture = run.Fixture(id="clip-a", wav_path="clip-a.wav", ceiling_dbtp=-1.0)
    anvil = {"clip-a": LoudnessMeasurement(-16.5, -3.0, 1.0)}  # 0.5 LU off, over 0.1 tolerance

    verdict = run._conformance_verdict([fixture], "ffmpeg", anvil, tolerance=0.1, fixtures_source="x")

    assert verdict["overall_pass"] is False
    assert verdict["results"][0]["lufs_within_tolerance"] is False
    assert verdict["results"][0]["pass"] is False


def test_conformance_verdict_true_peak_ceiling_gate(monkeypatch: pytest.MonkeyPatch) -> None:
    fake = LoudnessMeasurement(integrated_lufs=-16.0, true_peak_dbtp=-0.5, loudness_range_lu=1.0)
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: fake)

    fixture = run.Fixture(id="clip-a", wav_path="clip-a.wav", ceiling_dbtp=-1.0)

    # No anvil JSON provided: still gated on ffmpeg's own true-peak reading vs ceiling.
    verdict = run._conformance_verdict([fixture], "ffmpeg", {}, tolerance=0.1, fixtures_source="x")

    assert verdict["results"][0]["true_peak_ok"] is False
    assert verdict["results"][0]["pass"] is False
    assert verdict["overall_pass"] is False


def test_conformance_verdict_without_anvil_json_only_checks_true_peak(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake = LoudnessMeasurement(integrated_lufs=-16.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0)
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: fake)

    fixture = run.Fixture(id="clip-a", wav_path="clip-a.wav", ceiling_dbtp=-1.0)
    verdict = run._conformance_verdict([fixture], "ffmpeg", {}, tolerance=0.1, fixtures_source="x")

    assert verdict["anvil_json_provided"] is False
    assert verdict["results"][0]["lufs_within_tolerance"] is None
    assert verdict["results"][0]["pass"] is True
    assert verdict["overall_pass"] is True


def test_conformance_verdict_measurement_error_fails_that_fixture(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def boom(path, ffmpeg=None):
        raise RuntimeError("input file not found: nope.wav")

    monkeypatch.setattr(run, "measure_lufs_ffmpeg", boom)
    fixture = run.Fixture(id="missing", wav_path="nope.wav", ceiling_dbtp=-1.0)

    verdict = run._conformance_verdict([fixture], "ffmpeg", {}, tolerance=0.1, fixtures_source="x")

    assert verdict["overall_pass"] is False
    assert "error" in verdict["results"][0]
    assert verdict["results"][0]["pass"] is False


# --- load_eval_clips / _sha256_file / _default_eval_manifest (master-eval plumbing) -


def _write_manifest(path: Path, clips: list[dict]) -> None:
    path.write_text(json.dumps({"version": 1, "clips": clips}), encoding="utf-8")


def test_load_eval_clips_resolves_paths_relative_to_manifest(tmp_path: Path) -> None:
    (tmp_path / "clean-studio").mkdir()
    (tmp_path / "clean-studio" / "a.wav").write_bytes(b"")
    (tmp_path / "clean-studio" / "a.clean.wav").write_bytes(b"")
    manifest_path = tmp_path / "manifest.json"
    _write_manifest(
        manifest_path,
        [
            {
                "id": "syn-a", "class": "clean-studio", "path": "clean-studio/a.wav",
                "license": "CC0-1.0", "redistributable": True,
                "ground_truth": {"reference_wav": "clean-studio/a.clean.wav"},
            }
        ],
    )
    clips = run.load_eval_clips(manifest_path)
    assert len(clips) == 1
    assert clips[0]["_abs_path"] == (tmp_path / "clean-studio" / "a.wav").resolve()
    assert clips[0]["_abs_reference_path"] == (tmp_path / "clean-studio" / "a.clean.wav").resolve()


def test_load_eval_clips_no_ground_truth_gives_none_reference(tmp_path: Path) -> None:
    (tmp_path / "a.wav").write_bytes(b"")
    manifest_path = tmp_path / "manifest.json"
    _write_manifest(
        manifest_path,
        [{"id": "x", "class": "clean-studio", "path": "a.wav", "license": "CC0-1.0", "redistributable": True}],
    )
    clips = run.load_eval_clips(manifest_path)
    assert clips[0]["_abs_reference_path"] is None


def test_default_eval_manifest_prefers_synth_when_present(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(run, "HERE", tmp_path)
    synth_manifest = tmp_path / "corpus" / "synth" / "manifest.json"
    synth_manifest.parent.mkdir(parents=True)
    synth_manifest.write_text("{}", encoding="utf-8")
    assert run._default_eval_manifest() == synth_manifest


def test_default_eval_manifest_falls_back_to_example(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(run, "HERE", tmp_path)  # no corpus/synth/manifest.json here
    assert run._default_eval_manifest() == tmp_path / "corpus" / "manifest.example.json"


def test_sha256_file_matches_hashlib(tmp_path: Path) -> None:
    import hashlib

    path = tmp_path / "data.bin"
    path.write_bytes(b"anvil eval harness" * 1000)
    assert run._sha256_file(path) == hashlib.sha256(path.read_bytes()).hexdigest()


# --- cmd_synth --------------------------------------------------------------------------


def test_cmd_synth_skips_cleanly_when_deps_unavailable(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(run, "_synth_deps_available", lambda: False)
    args = argparse.Namespace(
        out=str(tmp_path / "out"), classes=None, variants_per_class=1, duration=1.0,
        seed=1, clean_dir=None, noise_dir=None,
    )
    assert run.cmd_synth(args) == 0
    assert not (tmp_path / "out").exists()


def test_cmd_synth_rejects_unknown_class(tmp_path: Path) -> None:
    pytest.importorskip("numpy")
    pytest.importorskip("scipy")
    pytest.importorskip("soundfile")
    args = argparse.Namespace(
        out=str(tmp_path / "out"), classes="not-a-real-class", variants_per_class=1, duration=1.0,
        seed=1, clean_dir=None, noise_dir=None,
    )
    assert run.cmd_synth(args) == 1


# --- _master_eval_one (the per-clip gate logic behind `master-eval`) ---------------------


def _clip(
    clip_id: str = "syn-x",
    class_name: str = "constant-broadband-noise",
    reference: Path | None = None,
    speech_source: str = "real",
) -> dict:
    # Default `speech_source="real"` so the DNSMOS/PESQ gate-logic tests below exercise
    # the gating path; the report-only path (synthetic speech) is covered explicitly by
    # test_master_eval_one_*_report_only_on_synthetic_speech.
    return {
        "id": clip_id, "class": class_name,
        "_abs_path": Path("input.wav"), "_abs_reference_path": reference,
        "_speech_source": speech_source,
        "ground_truth": {},
    }


def _ok_master_result(report: dict | None = None) -> AnvilRunResult:
    return AnvilRunResult(ok=True, unavailable=False, unavailable_reason=None, returncode=0, stdout="", stderr="", report=report)


def _unavailable_master_result() -> AnvilRunResult:
    return AnvilRunResult(
        ok=False, unavailable=True, unavailable_reason="anvil CLI reports 'master' unimplemented (exit 70)",
        returncode=70, stdout="", stderr="", report=None,
    )


def _target_loudness_measurement() -> LoudnessMeasurement:
    return LoudnessMeasurement(integrated_lufs=-16.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0)


def _fake_run_master_writes_output(anvil_bin, input_path, out_path, preset=None, tier=None, report_path=None):
    """A `run_master` stand-in for tests that need `_sha256_file` to succeed on the
    "rendered" output (e.g. `cmd_regress`), without a real anvil binary."""
    Path(out_path).write_bytes(b"rendered")
    return _ok_master_result()


def test_master_eval_one_reports_unavailable(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(run, "run_master", lambda *a, **k: _unavailable_master_result())
    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(), "podcast-stereo", "standard", -16.0, -1.0, tmp_path, None, False
    )
    assert entry["unavailable"] is True
    assert entry["overall_pass"] is None
    assert "checks" not in entry


def test_master_eval_one_real_failure_is_not_unavailable(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    failure = AnvilRunResult(ok=False, unavailable=False, unavailable_reason=None, returncode=1, stdout="", stderr="boom", report=None)
    monkeypatch.setattr(run, "run_master", lambda *a, **k: failure)
    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(), "podcast-stereo", "standard", -16.0, -1.0, tmp_path, None, False
    )
    assert entry["unavailable"] is False
    assert entry["overall_pass"] is False
    assert "boom" in entry["error"]


def test_master_eval_one_passes_loudness_and_true_peak(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result())
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())

    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(), "podcast-stereo", "standard", -16.0, -1.0, tmp_path, None, False
    )
    assert entry["checks"]["loudness"]["status"] == "pass"
    assert entry["checks"]["true_peak"]["status"] == "pass"
    assert entry["checks"]["dnsmos"]["status"] == "skip"
    assert entry["checks"]["intrusive"]["status"] == "skip"
    assert entry["overall_pass"] is True


def test_master_eval_one_fails_loudness_outside_tolerance(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    off_target = LoudnessMeasurement(integrated_lufs=-14.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0)  # 2 LU off
    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result())
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: off_target)

    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(), "podcast-stereo", "standard", -16.0, -1.0, tmp_path, None, False
    )
    assert entry["checks"]["loudness"]["status"] == "fail"
    assert entry["overall_pass"] is False


def test_master_eval_one_fails_true_peak_over_ceiling(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    hot = LoudnessMeasurement(integrated_lufs=-16.0, true_peak_dbtp=-0.2, loudness_range_lu=1.0)  # over -1.0 ceiling
    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result())
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: hot)

    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(), "podcast-stereo", "standard", -16.0, -1.0, tmp_path, None, False
    )
    assert entry["checks"]["true_peak"]["status"] == "fail"
    assert entry["overall_pass"] is False


def test_master_eval_one_runs_dnsmos_gate_when_model_given(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    import metrics.dnsmos as dnsmos_mod

    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result())
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())

    class FakeModel:
        def score(self, path):
            return dnsmos_mod.DnsmosResult(sig=2.0, bak=1.0, ovrl=1.5, sig_raw=2.0, bak_raw=1.0, ovrl_raw=1.5, num_segments=1)

    fake_verdict = dnsmos_mod.DnsmosGateVerdict(
        fixture_class="constant-broadband-noise", is_clean_control=False,
        sig_delta=0.0, bak_delta=1.5, ovrl_delta=0.5,
        sig_ok=True, bak_ok=True, ovrl_ok=True, clean_control_ok=None, overall_pass=True,
    )
    monkeypatch.setattr(dnsmos_mod, "dnsmos_gate", lambda cls, before, after: fake_verdict)

    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(), "podcast-stereo", "standard", -16.0, -1.0, tmp_path, FakeModel(), False
    )
    assert entry["checks"]["dnsmos"]["status"] == "pass"
    assert entry["checks"]["dnsmos"]["bak_delta"] == 1.5


def test_master_eval_one_skips_intrusive_without_reference(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result())
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())

    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(reference=None), "podcast-stereo", "standard", -16.0, -1.0, tmp_path, None, True
    )
    assert entry["checks"]["intrusive"]["status"] == "skip"
    assert "reference" in entry["checks"]["intrusive"]["reason"]


def test_master_eval_one_runs_intrusive_gate_with_reference(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    import metrics.intrusive as intrusive_mod

    reference = tmp_path / "ref.wav"
    reference.write_bytes(b"")
    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result())
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())
    monkeypatch.setattr(intrusive_mod, "intrusive_available", lambda: True)

    fake_iv = intrusive_mod.IntrusiveGateVerdict(
        pesq_before=1.0, pesq_after=1.6, pesq_delta=0.6, pesq_ok=True,
        stoi_before=0.5, stoi_after=0.6, stoi_delta=0.1, stoi_trend="improved",
    )
    monkeypatch.setattr(intrusive_mod, "intrusive_gate", lambda ref, before, after: fake_iv)

    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(reference=reference), "podcast-stereo", "standard", -16.0, -1.0, tmp_path, None, True
    )
    assert entry["checks"]["intrusive"]["status"] == "pass"
    assert entry["checks"]["intrusive"]["pesq_delta"] == 0.6


def test_master_eval_one_clean_control_intrusive_uses_lenient_threshold(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    # clean-studio has nothing to fix, so it can't gain +0.4 PESQ the way a genuinely
    # degraded clip can — it should be held to a "must not regress much" bar instead
    # (mirroring DNSMOS's clean-control handling), not the standard uplift gate.
    import metrics.intrusive as intrusive_mod

    reference = tmp_path / "ref.wav"
    reference.write_bytes(b"")
    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result())
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())
    monkeypatch.setattr(intrusive_mod, "intrusive_available", lambda: True)

    # A small, expected dip (e.g. from resampling) that would fail the standard +0.4
    # uplift gate (pesq_ok=False) but should pass the clean-control leniency (-0.02 >= -0.05).
    fake_iv = intrusive_mod.IntrusiveGateVerdict(
        pesq_before=4.6, pesq_after=4.58, pesq_delta=-0.02, pesq_ok=False,
        stoi_before=0.99, stoi_after=0.98, stoi_delta=-0.01, stoi_trend="regressed",
    )
    monkeypatch.setattr(intrusive_mod, "intrusive_gate", lambda ref, before, after: fake_iv)

    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(class_name="clean-studio", reference=reference),
        "podcast-stereo", "standard", -16.0, -1.0, tmp_path, None, True,
    )
    assert entry["checks"]["intrusive"]["status"] == "pass"


def test_master_eval_one_clean_control_intrusive_fails_on_real_regression(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    import metrics.intrusive as intrusive_mod

    reference = tmp_path / "ref.wav"
    reference.write_bytes(b"")
    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result())
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())
    monkeypatch.setattr(intrusive_mod, "intrusive_available", lambda: True)

    fake_iv = intrusive_mod.IntrusiveGateVerdict(
        pesq_before=4.6, pesq_after=4.0, pesq_delta=-0.6, pesq_ok=False,
        stoi_before=0.99, stoi_after=0.9, stoi_delta=-0.09, stoi_trend="regressed",
    )
    monkeypatch.setattr(intrusive_mod, "intrusive_gate", lambda ref, before, after: fake_iv)

    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(class_name="clean-studio", reference=reference),
        "podcast-stereo", "standard", -16.0, -1.0, tmp_path, None, True,
    )
    assert entry["checks"]["intrusive"]["status"] == "fail"


def test_master_eval_one_dnsmos_report_only_on_synthetic_speech(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    # DNSMOS is not calibrated for the synth_speech_like proxy, so on a synthetic-speech
    # fixture the check is COMPUTED (numbers retained for the trend line) but non-gating:
    # a would-be "fail" verdict must surface as "report", not fail the run.
    import metrics.dnsmos as dnsmos_mod

    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result())
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())

    class FakeModel:
        def score(self, path):
            return dnsmos_mod.DnsmosResult(sig=2.0, bak=1.0, ovrl=1.5, sig_raw=2.0, bak_raw=1.0, ovrl_raw=1.5, num_segments=1)

    failing_verdict = dnsmos_mod.DnsmosGateVerdict(
        fixture_class="constant-broadband-noise", is_clean_control=False,
        sig_delta=-0.5, bak_delta=-0.3, ovrl_delta=-0.4,
        sig_ok=False, bak_ok=False, ovrl_ok=False, clean_control_ok=None, overall_pass=False,
    )
    monkeypatch.setattr(dnsmos_mod, "dnsmos_gate", lambda cls, before, after: failing_verdict)

    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(speech_source="synthetic"),
        "podcast-stereo", "standard", -16.0, -1.0, tmp_path, FakeModel(), False,
    )
    assert entry["checks"]["dnsmos"]["status"] == "report"
    assert entry["checks"]["dnsmos"]["gated"] is False
    assert entry["checks"]["dnsmos"]["bak_delta"] == -0.3  # numbers still recorded
    assert entry["overall_pass"] is True  # report-only never fails the synthetic run


def test_master_eval_one_intrusive_report_only_on_synthetic_speech(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    import metrics.intrusive as intrusive_mod

    reference = tmp_path / "ref.wav"
    reference.write_bytes(b"")
    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result())
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())
    monkeypatch.setattr(intrusive_mod, "intrusive_available", lambda: True)

    # A hard PESQ collapse (the synthetic-fixture signature) that WOULD fail a real gate.
    fake_iv = intrusive_mod.IntrusiveGateVerdict(
        pesq_before=4.64, pesq_after=1.15, pesq_delta=-3.49, pesq_ok=False,
        stoi_before=1.0, stoi_after=0.76, stoi_delta=-0.24, stoi_trend="regressed",
    )
    monkeypatch.setattr(intrusive_mod, "intrusive_gate", lambda ref, before, after: fake_iv)

    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(class_name="clean-studio", reference=reference, speech_source="synthetic"),
        "podcast-stereo", "standard", -16.0, -1.0, tmp_path, None, True,
    )
    assert entry["checks"]["intrusive"]["status"] == "report"
    assert entry["checks"]["intrusive"]["gated"] is False
    assert entry["checks"]["intrusive"]["pesq_after"] == 1.15  # numbers still recorded
    assert entry["overall_pass"] is True


def test_master_eval_one_level_gaps_runs_leveler_variance(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    import metrics.leveler as leveler_mod

    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result())
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())

    fake_verdict = leveler_mod.LevelerVarianceVerdict(std_before=4.0, std_after=1.0, reduction_fraction=0.75, pass_=True)
    monkeypatch.setattr(leveler_mod, "leveler_variance_reduction", lambda before, after: fake_verdict)

    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(class_name=run.LEVEL_GAP_CLASS), "podcast-stereo", "standard",
        -16.0, -1.0, tmp_path, None, False,
    )
    assert entry["checks"]["leveler_variance"]["status"] == "pass"
    assert entry["checks"]["leveler_variance"]["reduction_fraction"] == 0.75


def test_master_eval_one_music_class_skips_without_segments(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result())
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())

    entry = run._master_eval_one(
        "anvil", "ffmpeg", _clip(class_name=run.MUSIC_CLASS), "podcast-stereo", "standard",
        -16.0, -1.0, tmp_path, None, False,
    )
    assert entry["checks"]["music_segment_loudness"]["status"] == "skip"


def test_master_eval_one_music_class_runs_segment_delta_when_present(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    import metrics.leveler as leveler_mod

    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result())
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())

    clip = _clip(class_name=run.MUSIC_CLASS)
    clip["ground_truth"] = {"music_segments_s": [[0.0, 4.0]]}

    seg = leveler_mod.MusicSegmentDelta(
        start_s=0.0, end_s=4.0, before=_target_loudness_measurement(), after=_target_loudness_measurement(),
        delta_lu=0.5, program_delta_lu=0.0, relative_delta_lu=0.5, pass_=True,
    )
    captured: dict = {}

    def _capture(*a, **k):
        captured.update(k)
        return [seg]

    monkeypatch.setattr(leveler_mod, "music_segment_delta", _capture)

    # A report whose whole-program loudness moved +4 LU (−20 → −16) — the master's
    # normalization gain, which the class-7 gate must factor out per-segment.
    report = {"before": {"integrated_lufs": -20.0}, "after": {"integrated_lufs": -16.0}}
    monkeypatch.setattr(run, "run_master", lambda *a, **k: _ok_master_result(report))

    entry = run._master_eval_one(
        "anvil", "ffmpeg", clip, "podcast-stereo", "standard", -16.0, -1.0, tmp_path, None, False
    )
    assert entry["checks"]["music_segment_loudness"]["status"] == "pass"
    # run.py must pass the program-level delta through so the gate measures leveler
    # pumping, not the (intended, desirable) global normalization.
    assert captured["program_delta_lu"] == pytest.approx(4.0)
    assert entry["checks"]["music_segment_loudness"]["program_delta_lu"] == pytest.approx(4.0)


# --- cmd_master_eval ----------------------------------------------------------------


def test_cmd_master_eval_skips_when_anvil_unavailable(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: None)
    args = argparse.Namespace(
        anvil=None, fixtures=None, preset="podcast-stereo", tier="standard", classes=None, json_out=None,
        ffmpeg=None, target_lufs=-16.0, true_peak_ceiling=-1.0, dnsmos_model=None,
        skip_dnsmos=True, skip_intrusive=True, out_dir=str(tmp_path / "out"),
    )
    assert run.cmd_master_eval(args) == 0


def test_cmd_master_eval_skips_when_ffmpeg_unavailable(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: "anvil")
    monkeypatch.setattr(run, "resolve_ffmpeg", lambda explicit: None)
    args = argparse.Namespace(
        anvil=None, fixtures=None, preset="podcast-stereo", tier="standard", classes=None, json_out=None,
        ffmpeg=None, target_lufs=-16.0, true_peak_ceiling=-1.0, dnsmos_model=None,
        skip_dnsmos=True, skip_intrusive=True, out_dir=str(tmp_path / "out"),
    )
    assert run.cmd_master_eval(args) == 0


def test_cmd_master_eval_missing_manifest_fails(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: "anvil")
    monkeypatch.setattr(run, "resolve_ffmpeg", lambda explicit: "ffmpeg")
    args = argparse.Namespace(
        anvil=None, fixtures=str(tmp_path / "nope.json"), preset="podcast-stereo", tier="standard",
        classes=None, json_out=None, ffmpeg=None, target_lufs=-16.0, true_peak_ceiling=-1.0,
        dnsmos_model=None, skip_dnsmos=True, skip_intrusive=True, out_dir=str(tmp_path / "out"),
    )
    assert run.cmd_master_eval(args) == 1


def test_cmd_master_eval_stops_cleanly_on_first_unavailable_clip(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    manifest_path = tmp_path / "manifest.json"
    (tmp_path / "a.wav").write_bytes(b"")
    (tmp_path / "b.wav").write_bytes(b"")
    _write_manifest(
        manifest_path,
        [
            {"id": "a", "class": "clean-studio", "path": "a.wav", "license": "CC0-1.0", "redistributable": True},
            {"id": "b", "class": "clean-studio", "path": "b.wav", "license": "CC0-1.0", "redistributable": True},
        ],
    )
    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: "anvil")
    monkeypatch.setattr(run, "resolve_ffmpeg", lambda explicit: "ffmpeg")

    calls = []

    def fake_master_eval_one(anvil_bin, ffmpeg, clip, *a, **k):
        calls.append(clip["id"])
        return {"id": clip["id"], "class": clip["class"], "unavailable": True, "unavailable_reason": "master unimplemented"}

    monkeypatch.setattr(run, "_master_eval_one", fake_master_eval_one)
    args = argparse.Namespace(
        anvil=None, fixtures=str(manifest_path), preset="podcast-stereo", tier="standard", classes=None,
        json_out=None, ffmpeg=None, target_lufs=-16.0, true_peak_ceiling=-1.0, dnsmos_model=None,
        skip_dnsmos=True, skip_intrusive=True, out_dir=str(tmp_path / "out"),
    )
    assert run.cmd_master_eval(args) == 0
    assert calls == ["a"]  # stopped after the first clip, didn't touch "b"


def test_cmd_master_eval_writes_verdict_and_fails_on_any_fail(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    manifest_path = tmp_path / "manifest.json"
    (tmp_path / "a.wav").write_bytes(b"")
    (tmp_path / "b.wav").write_bytes(b"")
    _write_manifest(
        manifest_path,
        [
            {"id": "a", "class": "clean-studio", "path": "a.wav", "license": "CC0-1.0", "redistributable": True},
            {"id": "b", "class": "clean-studio", "path": "b.wav", "license": "CC0-1.0", "redistributable": True},
        ],
    )
    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: "anvil")
    monkeypatch.setattr(run, "resolve_ffmpeg", lambda explicit: "ffmpeg")

    results_by_id = {
        "a": {"id": "a", "class": "clean-studio", "unavailable": False, "checks": {}, "overall_pass": True},
        "b": {"id": "b", "class": "clean-studio", "unavailable": False, "checks": {}, "overall_pass": False},
    }
    monkeypatch.setattr(run, "_master_eval_one", lambda anvil_bin, ffmpeg, clip, *a, **k: results_by_id[clip["id"]])

    json_out = tmp_path / "verdict.json"
    args = argparse.Namespace(
        anvil=None, fixtures=str(manifest_path), preset="podcast-stereo", tier="standard", classes=None,
        json_out=str(json_out), ffmpeg=None, target_lufs=-16.0, true_peak_ceiling=-1.0, dnsmos_model=None,
        skip_dnsmos=True, skip_intrusive=True, out_dir=str(tmp_path / "out"),
    )
    assert run.cmd_master_eval(args) == 1
    verdict = json.loads(json_out.read_text(encoding="utf-8"))
    assert verdict["passed"] == 1
    assert verdict["failed"] == 1
    assert verdict["overall_pass"] is False


# --- cmd_determinism ------------------------------------------------------------------


def test_cmd_determinism_skips_when_anvil_unavailable(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: None)
    args = argparse.Namespace(
        anvil=None, fixtures=None, preset="podcast-stereo", tier="standard", classes=None, json_out=None,
        limit=None, out_dir=str(tmp_path / "out"),
    )
    assert run.cmd_determinism(args) == 0


def test_cmd_determinism_identical_renders_pass(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    manifest_path = tmp_path / "manifest.json"
    (tmp_path / "a.wav").write_bytes(b"")
    _write_manifest(
        manifest_path,
        [{"id": "a", "class": "clean-studio", "path": "a.wav", "license": "CC0-1.0", "redistributable": True}],
    )
    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: "anvil")

    def fake_run_master(anvil_bin, input_path, out_path, preset=None, tier=None, report_path=None):
        Path(out_path).write_bytes(b"identical content")
        return _ok_master_result()

    monkeypatch.setattr(run, "run_master", fake_run_master)
    args = argparse.Namespace(
        anvil=None, fixtures=str(manifest_path), preset="podcast-stereo", tier="standard", classes=None,
        json_out=None, limit=None, out_dir=str(tmp_path / "out"),
    )
    assert run.cmd_determinism(args) == 0


def test_cmd_determinism_different_renders_fail(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    manifest_path = tmp_path / "manifest.json"
    (tmp_path / "a.wav").write_bytes(b"")
    _write_manifest(
        manifest_path,
        [{"id": "a", "class": "clean-studio", "path": "a.wav", "license": "CC0-1.0", "redistributable": True}],
    )
    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: "anvil")

    call_count = {"n": 0}

    def fake_run_master(anvil_bin, input_path, out_path, preset=None, tier=None, report_path=None):
        call_count["n"] += 1
        Path(out_path).write_bytes(f"content-{call_count['n']}".encode())
        return _ok_master_result()

    monkeypatch.setattr(run, "run_master", fake_run_master)
    args = argparse.Namespace(
        anvil=None, fixtures=str(manifest_path), preset="podcast-stereo", tier="standard", classes=None,
        json_out=None, limit=None, out_dir=str(tmp_path / "out"),
    )
    assert run.cmd_determinism(args) == 1


def test_cmd_determinism_skips_when_master_reports_unavailable(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    manifest_path = tmp_path / "manifest.json"
    (tmp_path / "a.wav").write_bytes(b"")
    _write_manifest(
        manifest_path,
        [{"id": "a", "class": "clean-studio", "path": "a.wav", "license": "CC0-1.0", "redistributable": True}],
    )
    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: "anvil")
    monkeypatch.setattr(run, "run_master", lambda *a, **k: _unavailable_master_result())
    args = argparse.Namespace(
        anvil=None, fixtures=str(manifest_path), preset="podcast-stereo", tier="standard", classes=None,
        json_out=None, limit=None, out_dir=str(tmp_path / "out"),
    )
    assert run.cmd_determinism(args) == 0


# --- cmd_regress ----------------------------------------------------------------------


def _regress_args(
    tmp_path: Path, manifest_path: Path, baseline: Path, update: bool = False, classes: str | None = None
) -> argparse.Namespace:
    return argparse.Namespace(
        anvil=None, fixtures=str(manifest_path), preset="podcast-stereo", tier="standard", classes=classes,
        json_out=None, ffmpeg=None, baseline=str(baseline), update_baseline=update, out_dir=str(tmp_path / "out"),
    )


def test_cmd_regress_respects_classes_filter(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    manifest_path = tmp_path / "manifest.json"
    (tmp_path / "a.wav").write_bytes(b"")
    (tmp_path / "b.wav").write_bytes(b"")
    _write_manifest(
        manifest_path,
        [
            {"id": "a", "class": "clean-studio", "path": "a.wav", "license": "CC0-1.0", "redistributable": True},
            {"id": "b", "class": "level-gaps", "path": "b.wav", "license": "CC0-1.0", "redistributable": True},
        ],
    )
    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: "anvil")
    monkeypatch.setattr(run, "resolve_ffmpeg", lambda explicit: "ffmpeg")
    monkeypatch.setattr("metrics.dnsmos.dnsmos_available", lambda model_path=None: False)
    monkeypatch.setattr(run, "run_master", _fake_run_master_writes_output)
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())

    baseline_path = tmp_path / "baseline.json"
    args = _regress_args(tmp_path, manifest_path, baseline_path, update=True, classes="clean-studio")
    assert run.cmd_regress(args) == 0
    baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
    assert set(baseline["clips"]) == {"a"}  # "b" (level-gaps) excluded by --classes


def test_cmd_regress_skips_when_anvil_unavailable(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: None)
    args = _regress_args(tmp_path, tmp_path / "manifest.json", tmp_path / "baseline.json")
    assert run.cmd_regress(args) == 0


def test_cmd_regress_update_baseline_writes_file(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    manifest_path = tmp_path / "manifest.json"
    (tmp_path / "a.wav").write_bytes(b"")
    _write_manifest(
        manifest_path,
        [{"id": "a", "class": "clean-studio", "path": "a.wav", "license": "CC0-1.0", "redistributable": True}],
    )
    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: "anvil")
    monkeypatch.setattr(run, "resolve_ffmpeg", lambda explicit: "ffmpeg")
    monkeypatch.setattr("metrics.dnsmos.dnsmos_available", lambda model_path=None: False)

    def fake_run_master(anvil_bin, input_path, out_path, preset=None, tier=None, report_path=None):
        Path(out_path).write_bytes(b"rendered")
        return _ok_master_result()

    monkeypatch.setattr(run, "run_master", fake_run_master)
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())

    baseline_path = tmp_path / "baseline.json"
    args = _regress_args(tmp_path, manifest_path, baseline_path, update=True)
    assert run.cmd_regress(args) == 0
    assert baseline_path.exists()
    baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
    assert "a" in baseline["clips"]
    assert baseline["clips"]["a"]["integrated_lufs"] == -16.0


def test_cmd_regress_detects_metric_regression_beyond_noise_band(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    manifest_path = tmp_path / "manifest.json"
    (tmp_path / "a.wav").write_bytes(b"")
    _write_manifest(
        manifest_path,
        [{"id": "a", "class": "clean-studio", "path": "a.wav", "license": "CC0-1.0", "redistributable": True}],
    )
    baseline_path = tmp_path / "baseline.json"
    baseline_path.write_text(
        json.dumps({"clips": {"a": {"hash": "abc", "integrated_lufs": -16.0, "true_peak_dbtp": -3.0}}}),
        encoding="utf-8",
    )

    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: "anvil")
    monkeypatch.setattr(run, "resolve_ffmpeg", lambda explicit: "ffmpeg")
    monkeypatch.setattr("metrics.dnsmos.dnsmos_available", lambda model_path=None: False)
    monkeypatch.setattr(run, "run_master", _fake_run_master_writes_output)
    # Loudness moved by 1.0 LU, well beyond the 0.2 LU noise band.
    regressed = LoudnessMeasurement(integrated_lufs=-17.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0)
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: regressed)

    args = _regress_args(tmp_path, manifest_path, baseline_path, update=False)
    assert run.cmd_regress(args) == 1


def test_cmd_regress_passes_within_noise_band(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    manifest_path = tmp_path / "manifest.json"
    (tmp_path / "a.wav").write_bytes(b"")
    _write_manifest(
        manifest_path,
        [{"id": "a", "class": "clean-studio", "path": "a.wav", "license": "CC0-1.0", "redistributable": True}],
    )
    baseline_path = tmp_path / "baseline.json"
    baseline_path.write_text(
        json.dumps({"clips": {"a": {"hash": "abc", "integrated_lufs": -16.0, "true_peak_dbtp": -3.0}}}),
        encoding="utf-8",
    )

    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: "anvil")
    monkeypatch.setattr(run, "resolve_ffmpeg", lambda explicit: "ffmpeg")
    monkeypatch.setattr("metrics.dnsmos.dnsmos_available", lambda model_path=None: False)
    monkeypatch.setattr(run, "run_master", _fake_run_master_writes_output)
    # Only 0.05 LU off — comfortably inside the 0.2 LU noise band.
    close_enough = LoudnessMeasurement(integrated_lufs=-16.05, true_peak_dbtp=-3.0, loudness_range_lu=1.0)
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: close_enough)

    args = _regress_args(tmp_path, manifest_path, baseline_path, update=False)
    assert run.cmd_regress(args) == 0


def test_cmd_regress_missing_baseline_without_update_fails(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    manifest_path = tmp_path / "manifest.json"
    (tmp_path / "a.wav").write_bytes(b"")
    _write_manifest(
        manifest_path,
        [{"id": "a", "class": "clean-studio", "path": "a.wav", "license": "CC0-1.0", "redistributable": True}],
    )
    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: "anvil")
    monkeypatch.setattr(run, "resolve_ffmpeg", lambda explicit: "ffmpeg")
    monkeypatch.setattr("metrics.dnsmos.dnsmos_available", lambda model_path=None: False)
    monkeypatch.setattr(run, "run_master", _fake_run_master_writes_output)
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())

    args = _regress_args(tmp_path, manifest_path, tmp_path / "does-not-exist.json", update=False)
    assert run.cmd_regress(args) == 1


def test_cmd_regress_new_clip_without_baseline_entry_is_not_a_regression(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    manifest_path = tmp_path / "manifest.json"
    (tmp_path / "a.wav").write_bytes(b"")
    _write_manifest(
        manifest_path,
        [{"id": "a", "class": "clean-studio", "path": "a.wav", "license": "CC0-1.0", "redistributable": True}],
    )
    baseline_path = tmp_path / "baseline.json"
    baseline_path.write_text(json.dumps({"clips": {}}), encoding="utf-8")  # empty: "a" is new

    monkeypatch.setattr(run, "resolve_anvil_bin", lambda explicit: "anvil")
    monkeypatch.setattr(run, "resolve_ffmpeg", lambda explicit: "ffmpeg")
    monkeypatch.setattr("metrics.dnsmos.dnsmos_available", lambda model_path=None: False)
    monkeypatch.setattr(run, "run_master", _fake_run_master_writes_output)
    monkeypatch.setattr(run, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: _target_loudness_measurement())

    args = _regress_args(tmp_path, manifest_path, baseline_path, update=False)
    assert run.cmd_regress(args) == 0
