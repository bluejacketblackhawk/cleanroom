"""Paired intrusive quality metrics — PESQ-WB and STOI (06 QUALITY-EVAL.md §2).

These need a paired clean reference, which is why `run.py synth` exists: real degraded
recordings have no clean reference, but synthetic degradations (clean speech convolved
with an RIR and/or mixed with noise at a known SNR) keep the original clean signal
around, so PESQ/STOI can be computed against it.

Gate: PESQ +>=0.4 uplift (degraded -> mastered), STOI never negative delta — but per 06
§2 that STOI row is "report-only trend lines", so `stoi_gate` always reports the trend
without failing the run on its own.

Both `pesq` and `pystoi` are optional dependencies (`intrusive_available()` gates on
both); callers should skip these checks cleanly when unavailable rather than crash.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from math import gcd

import numpy as np

PESQ_SAMPLE_RATE = 16000  # pesq's 'wb' (wideband) mode requires exactly 8k or 16k
STOI_SAMPLE_RATE = 10000  # pystoi's own internal working rate; it resamples for you,
# but feeding it a consistent rate keeps both signals aligned before hand-off.

PESQ_UPLIFT_MIN = 0.4


def pesq_available() -> bool:
    try:
        import pesq  # noqa: F401
    except ImportError:
        return False
    return True


def stoi_available() -> bool:
    try:
        import pystoi  # noqa: F401
    except ImportError:
        return False
    return True


def intrusive_available() -> bool:
    """True if both pesq and pystoi are importable."""
    return pesq_available() and stoi_available()


def _resample(audio: np.ndarray, sr: int, target_sr: int) -> np.ndarray:
    if sr == target_sr:
        return audio.astype(np.float32)
    from scipy.signal import resample_poly

    g = gcd(sr, target_sr)
    up, down = target_sr // g, sr // g
    return resample_poly(audio, up, down).astype(np.float32)


def _load_mono(path: str | os.PathLike[str]) -> tuple[np.ndarray, int]:
    import soundfile as sf

    audio, sr = sf.read(str(path), always_2d=False, dtype="float32")
    if audio.ndim > 1:
        audio = audio.mean(axis=1)
    return audio.astype(np.float32), sr


def _align_lengths(ref: np.ndarray, deg: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """Trim both signals to the shorter length.

    PESQ/STOI both require sample-aligned, equal-length ref/deg pairs. Real-world
    mastered output can be a handful of samples longer/shorter than its reference
    (resampling, block-processing edge effects) — trimming to the common length is
    the standard, conservative way to handle that without misaligning the signals.
    """
    n = min(len(ref), len(deg))
    return ref[:n], deg[:n]


def compute_pesq(reference_path: str | os.PathLike[str], degraded_path: str | os.PathLike[str]) -> float:
    """PESQ-WB (wideband) mean opinion score, ref vs degraded, both resampled to 16kHz."""
    from pesq import pesq

    ref_audio, ref_sr = _load_mono(reference_path)
    deg_audio, deg_sr = _load_mono(degraded_path)
    ref16 = _resample(ref_audio, ref_sr, PESQ_SAMPLE_RATE)
    deg16 = _resample(deg_audio, deg_sr, PESQ_SAMPLE_RATE)
    ref16, deg16 = _align_lengths(ref16, deg16)
    return float(pesq(PESQ_SAMPLE_RATE, ref16, deg16, "wb"))


def compute_stoi(
    reference_path: str | os.PathLike[str],
    degraded_path: str | os.PathLike[str],
    extended: bool = False,
) -> float:
    """STOI (or ESTOI if extended=True) intelligibility score, ref vs degraded."""
    from pystoi import stoi

    ref_audio, ref_sr = _load_mono(reference_path)
    deg_audio, deg_sr = _load_mono(degraded_path)
    # pystoi resamples internally to its own working rate as long as fs_sig matches
    # what's passed in, so feed it at a shared sample rate rather than STOI's internal
    # 10 kHz directly — align ref/deg to whichever native rate the reference has.
    deg_at_ref_sr = _resample(deg_audio, deg_sr, ref_sr)
    ref_aligned, deg_aligned = _align_lengths(ref_audio, deg_at_ref_sr)
    return float(stoi(ref_aligned, deg_aligned, ref_sr, extended=extended))


@dataclass
class IntrusiveGateVerdict:
    pesq_before: float
    pesq_after: float
    pesq_delta: float
    pesq_ok: bool
    stoi_before: float
    stoi_after: float
    stoi_delta: float
    stoi_trend: str  # "improved" | "unchanged" | "regressed" — report-only, never fails


def intrusive_gate(
    clean_reference_path: str | os.PathLike[str],
    before_path: str | os.PathLike[str],
    after_path: str | os.PathLike[str],
) -> IntrusiveGateVerdict:
    """Score PESQ/STOI of `before` (degraded) and `after` (mastered) against the same
    clean paired reference, and apply the 06 §2 gate (PESQ +>=0.4; STOI is report-only)."""
    pesq_before = compute_pesq(clean_reference_path, before_path)
    pesq_after = compute_pesq(clean_reference_path, after_path)
    pesq_delta = pesq_after - pesq_before

    stoi_before = compute_stoi(clean_reference_path, before_path)
    stoi_after = compute_stoi(clean_reference_path, after_path)
    stoi_delta = stoi_after - stoi_before
    if stoi_delta > 1e-6:
        trend = "improved"
    elif stoi_delta < -1e-6:
        trend = "regressed"
    else:
        trend = "unchanged"

    return IntrusiveGateVerdict(
        pesq_before=pesq_before,
        pesq_after=pesq_after,
        pesq_delta=pesq_delta,
        pesq_ok=pesq_delta >= PESQ_UPLIFT_MIN,
        stoi_before=stoi_before,
        stoi_after=stoi_after,
        stoi_delta=stoi_delta,
        stoi_trend=trend,
    )
