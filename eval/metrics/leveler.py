"""Leveler quality metrics — speech-gated short-term loudness variance (06 §2, class 6
"level-gaps") and music-segment loudness stability (class 7 "music-plus-speech").

Gate: variance (std-dev of gated short-term loudness) reduced >=50% on level-gap
fixtures; music-segment loudness change <=2 LU on music+speech fixtures.

The short-term loudness series here is a documented *approximation* of EBU R128 short-
term loudness: RMS level in dBFS over a sliding window, not full K-weighted LUFS. Full
K-weighting is out of scope for the deps this lane is allowed to add (numpy/scipy/
soundfile only, no pyloudnorm) — the metric under gate is a *relative* reduction in
variance, and RMS dB tracks level changes closely enough for that comparison to be
meaningful. Where an absolute LUFS number matters (the loudness/true-peak gates), the
harness cross-checks with ffmpeg's ebur128 filter instead (see metrics/loudness.py and
`music_segment_delta` below, which uses it for the class-7 check).
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from metrics.loudness import LoudnessMeasurement, measure_lufs_ffmpeg, resolve_ffmpeg

# Short-term loudness window (EBU R128 defines short-term as a 3s window); hop smaller
# than the window so consecutive windows overlap and gaps between speech bursts aren't
# missed entirely.
SHORT_TERM_WINDOW_S = 3.0
SHORT_TERM_HOP_S = 1.0

# Frames quieter than (peak_db - GATE_RELATIVE_DB) are treated as silence/non-speech and
# excluded from the variance calculation — a simple relative gate standing in for EBU
# R128's absolute+relative two-stage gate (again: approximation, not a certified
# loudness meter).
GATE_RELATIVE_DB = 40.0
SILENCE_FLOOR_DB = -100.0

LEVELER_VARIANCE_REDUCTION_MIN = 0.5  # >=50% reduction (06 §2, class 6)
MUSIC_SEGMENT_DELTA_MAX_LU = 2.0  # <=2 LU change (06 §2, class 7)


def _load_mono(path: str | os.PathLike[str]) -> tuple[np.ndarray, int]:
    import soundfile as sf

    audio, sr = sf.read(str(path), always_2d=False, dtype="float32")
    if audio.ndim > 1:
        audio = audio.mean(axis=1)
    return audio.astype(np.float32), sr


def short_term_loudness_series(
    path: str | os.PathLike[str],
    window_s: float = SHORT_TERM_WINDOW_S,
    hop_s: float = SHORT_TERM_HOP_S,
) -> np.ndarray:
    """RMS level in dBFS over sliding windows across the whole file. See module
    docstring: this is an RMS approximation of short-term loudness, not K-weighted LUFS.
    """
    audio, sr = _load_mono(path)
    win = max(int(round(window_s * sr)), 1)
    hop = max(int(round(hop_s * sr)), 1)
    if len(audio) < win:
        windows = [audio] if len(audio) > 0 else [np.zeros(1, dtype=np.float32)]
    else:
        starts = range(0, len(audio) - win + 1, hop)
        windows = [audio[s : s + win] for s in starts]

    levels = []
    for w in windows:
        rms = float(np.sqrt(np.mean(np.square(w, dtype=np.float64))))
        db = 20.0 * np.log10(rms) if rms > 0 else SILENCE_FLOOR_DB
        levels.append(max(db, SILENCE_FLOOR_DB))
    return np.array(levels, dtype=np.float64)


def speech_gated_std(
    path: str | os.PathLike[str],
    gate_relative_db: float = GATE_RELATIVE_DB,
) -> float:
    """Std-dev of the short-term loudness series, after gating out windows quieter than
    (peak - gate_relative_db) — i.e. excluding silence/near-silence between speech
    bursts so the variance reflects level *inconsistency during speech*, not the
    (expected, desirable) gap between speech and silence."""
    series = short_term_loudness_series(path)
    if len(series) == 0:
        return 0.0
    peak = float(np.max(series))
    gated = series[series >= (peak - gate_relative_db)]
    if len(gated) < 2:
        return 0.0
    return float(np.std(gated))


@dataclass
class LevelerVarianceVerdict:
    std_before: float
    std_after: float
    reduction_fraction: float  # (before - after) / before; can be negative if it got worse
    pass_: bool


def leveler_variance_reduction(
    before_path: str | os.PathLike[str],
    after_path: str | os.PathLike[str],
    min_reduction: float = LEVELER_VARIANCE_REDUCTION_MIN,
) -> LevelerVarianceVerdict:
    """06 §2 class-6 gate: speech-gated short-term loudness std-dev reduced >=50%."""
    std_before = speech_gated_std(before_path)
    std_after = speech_gated_std(after_path)
    if std_before <= 0:
        # Nothing to level (already dead flat, or silent) — treat as trivially passing
        # rather than dividing by zero.
        reduction = 0.0 if std_after <= 0 else -1.0
    else:
        reduction = (std_before - std_after) / std_before
    return LevelerVarianceVerdict(
        std_before=std_before,
        std_after=std_after,
        reduction_fraction=reduction,
        pass_=reduction >= min_reduction or std_before <= 0,
    )


@dataclass
class MusicSegmentDelta:
    start_s: float
    end_s: float
    before: LoudnessMeasurement
    after: LoudnessMeasurement
    delta_lu: float  # absolute segment loudness change (after - before)
    program_delta_lu: float  # whole-program loudness change the master applied (normalization)
    relative_delta_lu: float  # delta_lu - program_delta_lu: leveler-induced bed pumping
    pass_: bool


def _trim_wav_ffmpeg(
    src: Path, dst: Path, start_s: float, end_s: float, ffmpeg: str
) -> None:
    import subprocess

    duration = max(end_s - start_s, 0.01)
    proc = subprocess.run(
        [
            ffmpeg, "-y", "-hide_banner", "-nostats",
            "-ss", f"{start_s}", "-t", f"{duration}",
            "-i", str(src),
            "-ar", "48000", "-c:a", "pcm_s16le",
            str(dst),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0 or not dst.exists():
        raise RuntimeError(
            f"ffmpeg failed trimming segment [{start_s},{end_s}) from {src}: "
            f"{proc.stderr[-500:]}"
        )


def music_segment_delta(
    before_path: str | os.PathLike[str],
    after_path: str | os.PathLike[str],
    segments: list[tuple[float, float]],
    tmp_dir: str | os.PathLike[str],
    ffmpeg: str | os.PathLike[str] | None = None,
    program_delta_lu: float = 0.0,
    max_delta_lu: float = MUSIC_SEGMENT_DELTA_MAX_LU,
) -> list[MusicSegmentDelta]:
    """06 §2 class-7 gate: the *leveler* must not move a labeled music segment's loudness
    by more than 2 LU **relative to the program-level normalization** the master applies.

    Mastering intentionally renormalizes the whole program to target (e.g. −20 → −16 LUFS
    = +4 LU on every segment); the loudness gate already owns that global shift. This gate
    is about the leveler pumping/ducking the bed *on top of* that shift, so we subtract the
    program-level change (`program_delta_lu`, the master's whole-file before→after
    integrated-loudness delta) from each segment's absolute change and gate on the
    residual. With `program_delta_lu=0.0` (the default) this reduces to the raw absolute
    delta — the correct behavior when comparing two already-normalized signals.

    Segments are ffmpeg-trimmed to their own temp wavs and measured independently with
    `measure_lufs_ffmpeg` (the same ebur128 cross-check used everywhere else in this
    harness) rather than approximated from the whole-file RMS series, since this gate is
    stated in LU and needs the real loudness meter, not the leveler's RMS approximation.
    Raises RuntimeError if ffmpeg is unavailable — callers should check
    `metrics.loudness.ffmpeg_available()` first and skip the check cleanly.
    """
    resolved = resolve_ffmpeg(ffmpeg)
    if resolved is None:
        raise RuntimeError(
            "ffmpeg not found: pass --ffmpeg, set CLEANROOM_FFMPEG, or install ffmpeg on PATH"
        )
    tmp = Path(tmp_dir)
    tmp.mkdir(parents=True, exist_ok=True)

    results: list[MusicSegmentDelta] = []
    for i, (start_s, end_s) in enumerate(segments):
        before_seg = tmp / f"music-seg-{i}-before.wav"
        after_seg = tmp / f"music-seg-{i}-after.wav"
        _trim_wav_ffmpeg(Path(before_path), before_seg, start_s, end_s, resolved)
        _trim_wav_ffmpeg(Path(after_path), after_seg, start_s, end_s, resolved)
        before_m = measure_lufs_ffmpeg(before_seg, ffmpeg=resolved)
        after_m = measure_lufs_ffmpeg(after_seg, ffmpeg=resolved)
        delta = after_m.integrated_lufs - before_m.integrated_lufs
        relative = delta - program_delta_lu
        results.append(
            MusicSegmentDelta(
                start_s=start_s,
                end_s=end_s,
                before=before_m,
                after=after_m,
                delta_lu=delta,
                program_delta_lu=program_delta_lu,
                relative_delta_lu=relative,
                pass_=abs(relative) <= max_delta_lu,
            )
        )
    return results
