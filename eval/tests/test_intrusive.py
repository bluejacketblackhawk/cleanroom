"""Tests for eval/metrics/intrusive.py (PESQ-WB / STOI).

`intrusive_gate`'s aggregation/trend-labeling logic is tested by monkeypatching
`compute_pesq`/`compute_stoi` (no real pesq/pystoi call needed for that part). A couple
of tests exercise the real `pesq`/`pystoi` packages end-to-end on synthetic signals and
skip cleanly (`pytest.importorskip`) when those optional deps aren't installed.
"""

from __future__ import annotations

import pytest

import metrics.intrusive as intrusive
from metrics.intrusive import (
    PESQ_UPLIFT_MIN,
    intrusive_available,
    intrusive_gate,
    pesq_available,
    stoi_available,
)


# --- intrusive_gate: aggregation + trend labeling (mocked pesq/stoi) -------------------


def test_intrusive_gate_passes_on_sufficient_pesq_uplift(monkeypatch: pytest.MonkeyPatch) -> None:
    pesq_values = iter([1.5, 1.9 + PESQ_UPLIFT_MIN])  # before, after
    stoi_values = iter([0.5, 0.6])
    monkeypatch.setattr(intrusive, "compute_pesq", lambda ref, deg: next(pesq_values))
    monkeypatch.setattr(intrusive, "compute_stoi", lambda ref, deg, extended=False: next(stoi_values))

    verdict = intrusive_gate("ref.wav", "before.wav", "after.wav")
    assert verdict.pesq_ok is True
    assert verdict.stoi_trend == "improved"


def test_intrusive_gate_fails_on_insufficient_pesq_uplift(monkeypatch: pytest.MonkeyPatch) -> None:
    pesq_values = iter([1.5, 1.7])  # only +0.2, short of +0.4
    stoi_values = iter([0.5, 0.5])
    monkeypatch.setattr(intrusive, "compute_pesq", lambda ref, deg: next(pesq_values))
    monkeypatch.setattr(intrusive, "compute_stoi", lambda ref, deg, extended=False: next(stoi_values))

    verdict = intrusive_gate("ref.wav", "before.wav", "after.wav")
    assert verdict.pesq_ok is False
    assert verdict.stoi_trend == "unchanged"


def test_intrusive_gate_stoi_is_report_only_never_fails_the_pesq_gate(monkeypatch: pytest.MonkeyPatch) -> None:
    # STOI regressing should not flip pesq_ok — 06 §2 says STOI is report-only trend.
    pesq_values = iter([1.0, 1.5])  # +0.5, sufficient
    stoi_values = iter([0.6, 0.3])  # regressed
    monkeypatch.setattr(intrusive, "compute_pesq", lambda ref, deg: next(pesq_values))
    monkeypatch.setattr(intrusive, "compute_stoi", lambda ref, deg, extended=False: next(stoi_values))

    verdict = intrusive_gate("ref.wav", "before.wav", "after.wav")
    assert verdict.pesq_ok is True
    assert verdict.stoi_trend == "regressed"
    assert verdict.stoi_delta < 0


# --- _align_lengths ----------------------------------------------------------------------


def test_align_lengths_trims_to_shorter() -> None:
    import numpy as np

    ref = np.zeros(100, dtype=np.float32)
    deg = np.zeros(90, dtype=np.float32)
    ref_out, deg_out = intrusive._align_lengths(ref, deg)
    assert len(ref_out) == len(deg_out) == 90


# --- availability helpers ----------------------------------------------------------------


def test_pesq_available_reflects_import() -> None:
    try:
        import pesq  # noqa: F401

        expected = True
    except ImportError:
        expected = False
    assert pesq_available() is expected


def test_stoi_available_reflects_import() -> None:
    try:
        import pystoi  # noqa: F401

        expected = True
    except ImportError:
        expected = False
    assert stoi_available() is expected


def test_intrusive_available_requires_both(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(intrusive, "pesq_available", lambda: True)
    monkeypatch.setattr(intrusive, "stoi_available", lambda: False)
    assert intrusive_available() is False


# --- optional real pesq/pystoi smoke test (skips cleanly without the packages) --------


def test_real_pesq_and_stoi_prefer_the_closer_signal(tmp_path) -> None:
    pytest.importorskip("pesq")
    pytest.importorskip("pystoi")
    pytest.importorskip("soundfile")
    pytest.importorskip("scipy")
    import soundfile as sf

    from synth import mix_at_snr, synth_speech_like  # speech-shaped, not a pure tone —
    # PESQ saturates at its floor score on a pure sinusoid regardless of added noise.
    from metrics.intrusive import compute_pesq, compute_stoi

    sr = 16000
    clean = synth_speech_like(duration_s=3.0, sr=sr, seed=11)
    noise = _white_noise(len(clean), seed=1)
    close, _ = mix_at_snr(clean, noise, snr_db=30.0)  # barely degraded
    far, _ = mix_at_snr(clean, noise, snr_db=-5.0)  # heavily degraded

    ref_path, close_path, far_path = tmp_path / "ref.wav", tmp_path / "close.wav", tmp_path / "far.wav"
    sf.write(str(ref_path), clean.astype("float32"), sr)
    sf.write(str(close_path), close.astype("float32"), sr)
    sf.write(str(far_path), far.astype("float32"), sr)

    assert compute_pesq(ref_path, close_path) > compute_pesq(ref_path, far_path)
    assert compute_stoi(ref_path, close_path) > compute_stoi(ref_path, far_path)


def _white_noise(n: int, seed: int):
    import numpy as np

    return np.random.default_rng(seed).standard_normal(n).astype("float32")
