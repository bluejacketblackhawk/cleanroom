r"""DNSMOS P.835 (SIG/BAK/OVRL) — the speech-quality uplift gate (06 QUALITY-EVAL.md §2).

Gate: BAK +>=1.0 on noisy classes; SIG -<=0.1 on ALL classes (never hurt the voice);
OVRL +>=0.4 on noisy classes; clean-control class: all three deltas >= -0.05.

This wraps Microsoft's published DNSMOS P.835 ONNX model (`sig_bak_ovr.onnx` from the
DNS-Challenge repo), reimplementing only the primary-model inference path from the
upstream reference script (`DNSMOS/dnsmos_local.py`): 9.01s windows of raw 16kHz audio
straight into the `input_1` tensor, no mel-spectrogram features required for SIG/BAK/OVRL
(that's only needed for the separate legacy P.808 model, which this module does not use,
since 06 §2 only gates on SIG/BAK/OVRL). That keeps the dependency footprint inside what
M1 lane 2 is allowed to add (numpy/scipy/soundfile/onnxruntime) — no librosa.

## Model provenance (dev-only, fetched once, gitignored)

    source: https://raw.githubusercontent.com/microsoft/DNS-Challenge/master/DNSMOS/DNSMOS/sig_bak_ovr.onnx
    sha256: 269fbebdb513aa23cddfbb593542ecc540284a91849ac50516870e1ac78f6edd
    size:   1,157,965 bytes
    saved to: eval/models/sig_bak_ovr.onnx (gitignored: root .gitignore has both
              `*.onnx` and `eval/models/` patterns)

Fetch it with (PowerShell):

    New-Item -ItemType Directory -Force -Path eval\models | Out-Null
    Invoke-WebRequest -Uri https://raw.githubusercontent.com/microsoft/DNS-Challenge/master/DNSMOS/DNSMOS/sig_bak_ovr.onnx -OutFile eval\models\sig_bak_ovr.onnx

Every code path here degrades cleanly when onnxruntime isn't installed or the model file
isn't present: `dnsmos_available()` returns False and callers skip the DNSMOS gate rather
than crash (see `run.py master-eval`).
"""

from __future__ import annotations

import hashlib
import os
from dataclasses import dataclass
from math import gcd
from pathlib import Path

import numpy as np

MODELS_DIR = Path(__file__).resolve().parent.parent / "models"
MODEL_FILENAME = "sig_bak_ovr.onnx"
DEFAULT_MODEL_PATH = MODELS_DIR / MODEL_FILENAME

MODEL_SOURCE_URL = (
    "https://raw.githubusercontent.com/microsoft/DNS-Challenge/master/"
    "DNSMOS/DNSMOS/sig_bak_ovr.onnx"
)
MODEL_SHA256 = "269fbebdb513aa23cddfbb593542ecc540284a91849ac50516870e1ac78f6edd"

SAMPLE_RATE = 16000
INPUT_LENGTH_S = 9.01  # matches upstream ComputeScore.__call__

# Non-personalized-MOS polynomial mapping (upstream `get_polyfit_val`, is_personalized_MOS=False).
# Maps the model's raw regression outputs onto the P.835 MOS scale.
_P_SIG = np.poly1d([-0.08397278, 1.22083953, 0.0052439])
_P_BAK = np.poly1d([-0.13166888, 1.60915514, -0.39604546])
_P_OVR = np.poly1d([-0.06766283, 1.11546468, 0.04602535])

# 06 §2 gate thresholds.
BAK_UPLIFT_MIN = 1.0
SIG_DELTA_MIN = -0.1
OVRL_UPLIFT_MIN = 0.4
CLEAN_CONTROL_DELTA_MIN = -0.05

# Comparisons below use `delta >= threshold - _EPS` rather than a bare `>=`, so a delta
# that's conceptually exactly on the threshold (e.g. -0.1) isn't spuriously failed by
# float subtraction landing a few ULPs to the wrong side (e.g. -0.10000000000000009).
_EPS = 1e-9

CLEAN_CONTROL_CLASS = "clean-studio"


@dataclass
class DnsmosResult:
    """One clip's DNSMOS P.835 scores, MOS-mapped and raw, averaged over 9.01s windows."""

    sig: float
    bak: float
    ovrl: float
    sig_raw: float
    bak_raw: float
    ovrl_raw: float
    num_segments: int


def onnxruntime_available() -> bool:
    """True if the `onnxruntime` package can be imported."""
    try:
        import onnxruntime  # noqa: F401
    except ImportError:
        return False
    return True


def model_available(model_path: str | os.PathLike[str] | None = None) -> bool:
    """True if the DNSMOS ONNX model file exists at `model_path` (default eval/models/)."""
    path = Path(model_path) if model_path else DEFAULT_MODEL_PATH
    return path.is_file()


def dnsmos_available(model_path: str | os.PathLike[str] | None = None) -> bool:
    """True if both onnxruntime and the model file are available — the harness's
    single gate for whether DNSMOS checks should run or be skipped."""
    return onnxruntime_available() and model_available(model_path)


def verify_model_hash(model_path: str | os.PathLike[str] | None = None) -> bool:
    """Verify the on-disk model's sha256 against the pinned `MODEL_SHA256`."""
    path = Path(model_path) if model_path else DEFAULT_MODEL_PATH
    if not path.is_file():
        return False
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    return digest == MODEL_SHA256


def _resample_to_16k(audio: np.ndarray, sr: int) -> np.ndarray:
    """Band-limited resample to 16kHz via scipy's polyphase resampler (no librosa)."""
    if sr == SAMPLE_RATE:
        return audio.astype(np.float32)
    from scipy.signal import resample_poly

    g = gcd(sr, SAMPLE_RATE)
    up, down = SAMPLE_RATE // g, sr // g
    return resample_poly(audio, up, down).astype(np.float32)


def _load_mono_16k(path: str | os.PathLike[str]) -> np.ndarray:
    import soundfile as sf

    audio, sr = sf.read(str(path), always_2d=False, dtype="float32")
    if audio.ndim > 1:
        audio = audio.mean(axis=1)
    return _resample_to_16k(audio.astype(np.float32), sr)


def _polyfit(sig_raw: float, bak_raw: float, ovr_raw: float) -> tuple[float, float, float]:
    return float(_P_SIG(sig_raw)), float(_P_BAK(bak_raw)), float(_P_OVR(ovr_raw))


class DnsmosModel:
    """Loads the ONNX session once; call `.score(wav_path)` per clip.

    Raises RuntimeError at construction if onnxruntime or the model file is missing —
    callers should guard with `dnsmos_available()` first and skip cleanly instead of
    letting this propagate.
    """

    def __init__(self, model_path: str | os.PathLike[str] | None = None) -> None:
        if not onnxruntime_available():
            raise RuntimeError("onnxruntime is not installed (pip install onnxruntime)")
        path = Path(model_path) if model_path else DEFAULT_MODEL_PATH
        if not path.is_file():
            raise RuntimeError(
                f"DNSMOS model not found at {path}; see eval/metrics/dnsmos.py "
                "module docstring for fetch instructions"
            )
        import onnxruntime as ort

        self._session = ort.InferenceSession(str(path), providers=["CPUExecutionProvider"])
        self._input_name = self._session.get_inputs()[0].name

    def score(self, wav_path: str | os.PathLike[str]) -> DnsmosResult:
        """Score one clip. Segments it into 9.01s/1s-hop windows (matching upstream),
        runs each through the model, and averages the raw outputs before MOS-mapping —
        exactly the upstream clip-level aggregation, so multi-window clips get one
        stable score instead of per-window noise."""
        audio = _load_mono_16k(wav_path)
        seg_len = int(INPUT_LENGTH_S * SAMPLE_RATE)

        if len(audio) == 0:
            audio = np.zeros(seg_len, dtype=np.float32)
        while len(audio) < seg_len:
            audio = np.concatenate([audio, audio])

        num_hops = max(int(np.floor(len(audio) / SAMPLE_RATE) - INPUT_LENGTH_S) + 1, 1)
        hop_len = SAMPLE_RATE

        sig_raws: list[float] = []
        bak_raws: list[float] = []
        ovr_raws: list[float] = []
        for idx in range(num_hops):
            start = idx * hop_len
            seg = audio[start : start + seg_len]
            if len(seg) < seg_len:
                continue
            inp = seg.astype(np.float32)[np.newaxis, :]
            out = self._session.run(None, {self._input_name: inp})[0][0]
            sig_raws.append(float(out[0]))
            bak_raws.append(float(out[1]))
            ovr_raws.append(float(out[2]))

        if not sig_raws:  # very short clip: score the padded single window directly
            inp = audio[:seg_len].astype(np.float32)[np.newaxis, :]
            out = self._session.run(None, {self._input_name: inp})[0][0]
            sig_raws, bak_raws, ovr_raws = [float(out[0])], [float(out[1])], [float(out[2])]

        sig_raw, bak_raw, ovr_raw = (
            float(np.mean(sig_raws)),
            float(np.mean(bak_raws)),
            float(np.mean(ovr_raws)),
        )
        sig, bak, ovrl = _polyfit(sig_raw, bak_raw, ovr_raw)
        return DnsmosResult(
            sig=sig,
            bak=bak,
            ovrl=ovrl,
            sig_raw=sig_raw,
            bak_raw=bak_raw,
            ovrl_raw=ovr_raw,
            num_segments=len(sig_raws),
        )


# --- 06 §2 gate helpers (pure functions over before/after scores; unit-testable without
# the model itself, since they only operate on already-computed DnsmosResult values) ------


@dataclass
class DnsmosGateVerdict:
    fixture_class: str
    is_clean_control: bool
    sig_delta: float
    bak_delta: float
    ovrl_delta: float
    sig_ok: bool
    bak_ok: bool | None  # None when the check doesn't apply (clean-control clips)
    ovrl_ok: bool | None
    clean_control_ok: bool | None
    overall_pass: bool


def dnsmos_gate(fixture_class: str, before: DnsmosResult, after: DnsmosResult) -> DnsmosGateVerdict:
    """Apply the 06 §2 DNSMOS gate for one fixture given its before/after scores.

    - SIG must never drop by more than SIG_DELTA_MIN, on every class.
    - clean-control class (`clean-studio`): ALL three deltas must be >= -0.05 (stricter,
      and BAK/OVRL uplift is not required — it's a must-not-degrade control, not a
      noisy-class uplift target).
    - every other ("noisy") class: BAK delta >= +1.0 and OVRL delta >= +0.4, in addition
      to the always-on SIG floor.
    """
    sig_delta = after.sig - before.sig
    bak_delta = after.bak - before.bak
    ovrl_delta = after.ovrl - before.ovrl

    sig_ok = sig_delta >= SIG_DELTA_MIN - _EPS
    is_clean = fixture_class == CLEAN_CONTROL_CLASS

    if is_clean:
        clean_ok = (
            sig_delta >= CLEAN_CONTROL_DELTA_MIN - _EPS
            and bak_delta >= CLEAN_CONTROL_DELTA_MIN - _EPS
            and ovrl_delta >= CLEAN_CONTROL_DELTA_MIN - _EPS
        )
        overall = sig_ok and clean_ok
        return DnsmosGateVerdict(
            fixture_class=fixture_class,
            is_clean_control=True,
            sig_delta=sig_delta,
            bak_delta=bak_delta,
            ovrl_delta=ovrl_delta,
            sig_ok=sig_ok,
            bak_ok=None,
            ovrl_ok=None,
            clean_control_ok=clean_ok,
            overall_pass=overall,
        )

    bak_ok = bak_delta >= BAK_UPLIFT_MIN - _EPS
    ovrl_ok = ovrl_delta >= OVRL_UPLIFT_MIN - _EPS
    overall = sig_ok and bak_ok and ovrl_ok
    return DnsmosGateVerdict(
        fixture_class=fixture_class,
        is_clean_control=False,
        sig_delta=sig_delta,
        bak_delta=bak_delta,
        ovrl_delta=ovrl_delta,
        sig_ok=sig_ok,
        bak_ok=bak_ok,
        ovrl_ok=ovrl_ok,
        clean_control_ok=None,
        overall_pass=overall,
    )
