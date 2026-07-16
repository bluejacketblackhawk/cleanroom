"""Tests for eval/metrics/dnsmos.py.

`dnsmos_gate` is pure (operates on already-computed DnsmosResult values) so its 06 §2
gate logic is tested hermetically with fabricated scores — no ONNX model needed.
`onnxruntime_available`/`model_available` are tested via monkeypatch/tmp_path.

A couple of tests exercise the real ONNX model end-to-end (`DnsmosModel.score`) and are
skipped automatically (`pytest.importorskip` + a model-file existence check) in any
environment that doesn't have onnxruntime installed and the model fetched — exactly the
"skip cleanly if absent" contract the module promises.
"""

from __future__ import annotations

import pytest

from metrics.dnsmos import (
    CLEAN_CONTROL_CLASS,
    DnsmosResult,
    dnsmos_available,
    dnsmos_gate,
    model_available,
    onnxruntime_available,
)


def _result(sig: float, bak: float, ovrl: float) -> DnsmosResult:
    return DnsmosResult(sig=sig, bak=bak, ovrl=ovrl, sig_raw=sig, bak_raw=bak, ovrl_raw=ovrl, num_segments=1)


# --- dnsmos_gate: noisy classes ---------------------------------------------------------


def test_noisy_class_passes_when_all_thresholds_met() -> None:
    before = _result(sig=2.0, bak=1.5, ovrl=1.8)
    after = _result(sig=2.0, bak=2.6, ovrl=2.3)  # bak +1.1, ovrl +0.5, sig +0.0
    verdict = dnsmos_gate("constant-broadband-noise", before, after)
    assert verdict.overall_pass is True
    assert verdict.is_clean_control is False
    assert verdict.bak_ok is True
    assert verdict.ovrl_ok is True


def test_noisy_class_fails_when_bak_uplift_short() -> None:
    before = _result(sig=2.0, bak=1.5, ovrl=1.8)
    after = _result(sig=2.0, bak=2.3, ovrl=2.3)  # bak +0.8 < 1.0
    verdict = dnsmos_gate("constant-broadband-noise", before, after)
    assert verdict.bak_ok is False
    assert verdict.overall_pass is False


def test_noisy_class_fails_when_ovrl_uplift_short() -> None:
    before = _result(sig=2.0, bak=1.5, ovrl=1.8)
    after = _result(sig=2.0, bak=2.6, ovrl=2.1)  # ovrl +0.3 < 0.4
    verdict = dnsmos_gate("dynamic-noise", before, after)
    assert verdict.ovrl_ok is False
    assert verdict.overall_pass is False


def test_noisy_class_fails_when_sig_drops_too_much() -> None:
    before = _result(sig=3.0, bak=1.5, ovrl=1.8)
    after = _result(sig=2.8, bak=3.0, ovrl=2.5)  # sig -0.2 < -0.1 floor
    verdict = dnsmos_gate("hum-50-60hz", before, after)
    assert verdict.sig_ok is False
    assert verdict.overall_pass is False


def test_noisy_class_sig_floor_boundary_is_inclusive() -> None:
    before = _result(sig=3.0, bak=1.5, ovrl=1.8)
    after = _result(sig=2.9, bak=3.0, ovrl=2.5)  # sig delta exactly -0.1
    verdict = dnsmos_gate("hum-50-60hz", before, after)
    assert verdict.sig_ok is True


# --- dnsmos_gate: clean-control class ---------------------------------------------------


def test_clean_control_passes_with_small_deltas() -> None:
    before = _result(sig=4.0, bak=4.0, ovrl=4.0)
    after = _result(sig=3.96, bak=4.02, ovrl=3.97)  # all deltas >= -0.05
    verdict = dnsmos_gate(CLEAN_CONTROL_CLASS, before, after)
    assert verdict.is_clean_control is True
    assert verdict.overall_pass is True
    assert verdict.bak_ok is None and verdict.ovrl_ok is None  # noisy-class checks don't apply


def test_clean_control_fails_when_any_delta_drops_past_threshold() -> None:
    before = _result(sig=4.0, bak=4.0, ovrl=4.0)
    after = _result(sig=3.90, bak=4.0, ovrl=4.0)  # sig -0.10, worse than -0.05
    verdict = dnsmos_gate(CLEAN_CONTROL_CLASS, before, after)
    assert verdict.overall_pass is False
    assert verdict.clean_control_ok is False


def test_clean_control_does_not_require_uplift() -> None:
    # No uplift at all (flat deltas) should still pass — it's a must-not-degrade
    # control, not a noisy-class uplift target.
    before = _result(sig=4.0, bak=4.0, ovrl=4.0)
    after = _result(sig=4.0, bak=4.0, ovrl=4.0)
    verdict = dnsmos_gate(CLEAN_CONTROL_CLASS, before, after)
    assert verdict.overall_pass is True


# --- availability helpers ----------------------------------------------------------------


def test_onnxruntime_available_reflects_import(monkeypatch: pytest.MonkeyPatch) -> None:
    # onnxruntime is either genuinely importable or not in this environment; just check
    # the helper agrees with a real import attempt (no monkeypatch needed/possible for
    # an import-based check without faking sys.modules, which is out of scope here).
    try:
        import onnxruntime  # noqa: F401

        expected = True
    except ImportError:
        expected = False
    assert onnxruntime_available() is expected


def test_model_available_false_for_missing_path(tmp_path) -> None:
    assert model_available(tmp_path / "nope.onnx") is False


def test_model_available_true_for_existing_file(tmp_path) -> None:
    model_path = tmp_path / "sig_bak_ovr.onnx"
    model_path.write_bytes(b"not a real model, just needs to exist")
    assert model_available(model_path) is True


def test_dnsmos_available_requires_both(tmp_path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("metrics.dnsmos.onnxruntime_available", lambda: False)
    assert dnsmos_available(tmp_path / "whatever.onnx") is False


# --- optional real-model smoke test (skips cleanly without onnxruntime + model file) ---


def test_real_model_scores_silence_and_noise_differently() -> None:
    pytest.importorskip("onnxruntime")
    pytest.importorskip("soundfile")
    pytest.importorskip("scipy")
    from metrics.dnsmos import DEFAULT_MODEL_PATH, DnsmosModel

    if not DEFAULT_MODEL_PATH.is_file():
        pytest.skip(f"DNSMOS model not fetched at {DEFAULT_MODEL_PATH}")

    import numpy as np
    import soundfile as sf

    sr = 16000
    silence = np.zeros(sr * 2, dtype=np.float32)
    rng = np.random.default_rng(0)
    noise = (rng.standard_normal(sr * 2) * 0.2).astype(np.float32)

    import tempfile
    from pathlib import Path

    with tempfile.TemporaryDirectory() as td:
        silence_path = Path(td) / "silence.wav"
        noise_path = Path(td) / "noise.wav"
        sf.write(str(silence_path), silence, sr)
        sf.write(str(noise_path), noise, sr)

        model = DnsmosModel()
        r_silence = model.score(silence_path)
        r_noise = model.score(noise_path)

    assert r_silence.sig != r_noise.sig or r_silence.bak != r_noise.bak
