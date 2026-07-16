"""Tests for eval/metrics/leveler.py (speech-gated short-term loudness variance,
class-6 gate; music-segment loudness delta, class-7 gate).

`short_term_loudness_series`/`speech_gated_std`/`leveler_variance_reduction` only need
numpy/soundfile (base deps) and are tested against synthetic wavs written to tmp_path —
no ffmpeg needed. `music_segment_delta` shells out to ffmpeg for trimming/measuring;
those calls are monkeypatched (`_trim_wav_ffmpeg`, `measure_lufs_ffmpeg`) so this file
stays hermetic, matching the existing conformance-test pattern in tests/test_run.py.
"""

from __future__ import annotations

import numpy as np
import pytest
import soundfile as sf

import metrics.leveler as leveler
from metrics.loudness import LoudnessMeasurement


def _write_wav(path, audio: np.ndarray, sr: int = 48000) -> None:
    sf.write(str(path), audio.astype(np.float32), sr)


# --- short_term_loudness_series / speech_gated_std --------------------------------------


def test_short_term_loudness_series_flat_signal_has_near_zero_std(tmp_path) -> None:
    sr = 48000
    audio = 0.1 * np.sin(2 * np.pi * 220 * np.arange(sr * 6) / sr)
    path = tmp_path / "flat.wav"
    _write_wav(path, audio, sr)

    series = leveler.short_term_loudness_series(path)
    assert len(series) > 1
    assert np.std(series) < 0.5  # a steady tone should measure very consistently


def test_speech_gated_std_higher_for_level_modulated_signal(tmp_path) -> None:
    sr = 48000
    n = sr * 12
    t = np.arange(n) / sr
    tone = 0.1 * np.sin(2 * np.pi * 220 * t)

    flat_path = tmp_path / "flat.wav"
    _write_wav(flat_path, tone, sr)

    # Chop into 2s segments alternating between quiet and loud gain — a level-gap stand-in.
    gapped = tone.copy()
    seg = sr * 2
    for i, start in enumerate(range(0, n, seg)):
        end = min(start + seg, n)
        gapped[start:end] *= 3.0 if i % 2 == 0 else 0.3
    gapped_path = tmp_path / "gapped.wav"
    _write_wav(gapped_path, gapped, sr)

    flat_std = leveler.speech_gated_std(flat_path)
    gapped_std = leveler.speech_gated_std(gapped_path)
    assert gapped_std > flat_std


def test_speech_gated_std_gates_out_silence(tmp_path) -> None:
    sr = 48000
    n = sr * 12
    audio = np.zeros(n, dtype=np.float32)
    # A steady 6s tone bracketed by 3s of silence on each side. Ungated, the silence
    # windows (floored at SILENCE_FLOOR_DB) dominate the spread; gated, only windows
    # near the tone's own steady level should remain.
    audio[sr * 3 : sr * 9] = 0.2 * np.sin(2 * np.pi * 220 * np.arange(sr * 6) / sr)
    path = tmp_path / "bracketed_by_silence.wav"
    _write_wav(path, audio, sr)

    ungated_std = float(np.std(leveler.short_term_loudness_series(path)))
    gated_std = leveler.speech_gated_std(path)
    assert gated_std < ungated_std * 0.5  # gating removes most of the silence-driven spread


# --- leveler_variance_reduction (class-6 gate) -------------------------------------------


def test_leveler_variance_reduction_passes_when_gaps_smoothed(tmp_path) -> None:
    sr = 48000
    n = sr * 12
    t = np.arange(n) / sr
    tone = 0.1 * np.sin(2 * np.pi * 220 * t)

    before = tone.copy()
    seg = sr * 2
    for i, start in enumerate(range(0, n, seg)):
        end = min(start + seg, n)
        before[start:end] *= 3.0 if i % 2 == 0 else 0.3
    before_path = tmp_path / "before.wav"
    _write_wav(before_path, before, sr)

    after_path = tmp_path / "after.wav"  # the "mastered" flat-level version
    _write_wav(after_path, tone, sr)

    verdict = leveler.leveler_variance_reduction(before_path, after_path)
    assert verdict.std_after < verdict.std_before
    assert verdict.reduction_fraction >= leveler.LEVELER_VARIANCE_REDUCTION_MIN
    assert verdict.pass_ is True


def test_leveler_variance_reduction_fails_when_gaps_unchanged(tmp_path) -> None:
    sr = 48000
    n = sr * 12
    t = np.arange(n) / sr
    tone = 0.1 * np.sin(2 * np.pi * 220 * t)

    gapped = tone.copy()
    seg = sr * 2
    for i, start in enumerate(range(0, n, seg)):
        end = min(start + seg, n)
        gapped[start:end] *= 3.0 if i % 2 == 0 else 0.3

    before_path, after_path = tmp_path / "before.wav", tmp_path / "after.wav"
    _write_wav(before_path, gapped, sr)
    _write_wav(after_path, gapped, sr)  # identical -> no reduction at all

    verdict = leveler.leveler_variance_reduction(before_path, after_path)
    assert verdict.reduction_fraction == pytest.approx(0.0, abs=1e-6)
    assert verdict.pass_ is False


def test_leveler_variance_reduction_handles_zero_before_std(tmp_path) -> None:
    sr = 48000
    tone = 0.1 * np.sin(2 * np.pi * 220 * np.arange(sr * 6) / sr)
    path = tmp_path / "flat.wav"
    _write_wav(path, tone, sr)

    verdict = leveler.leveler_variance_reduction(path, path)
    assert verdict.pass_ is True  # nothing to level in the first place; not a failure


# --- music_segment_delta (class-7 gate; ffmpeg calls monkeypatched) ---------------------


def test_music_segment_delta_pass_within_band(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    monkeypatch.setattr(leveler, "resolve_ffmpeg", lambda ffmpeg=None: "ffmpeg")
    monkeypatch.setattr(leveler, "_trim_wav_ffmpeg", lambda src, dst, start, end, ffmpeg: dst.write_bytes(b""))

    measurements = iter(
        [
            LoudnessMeasurement(integrated_lufs=-20.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0),  # before
            LoudnessMeasurement(integrated_lufs=-21.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0),  # after, -1 LU
        ]
    )
    monkeypatch.setattr(leveler, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: next(measurements))

    results = leveler.music_segment_delta(
        "before.wav", "after.wav", [(0.0, 4.0)], tmp_path, ffmpeg="ffmpeg"
    )
    assert len(results) == 1
    assert results[0].delta_lu == pytest.approx(-1.0)
    assert results[0].pass_ is True


def test_music_segment_delta_fails_outside_band(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    monkeypatch.setattr(leveler, "resolve_ffmpeg", lambda ffmpeg=None: "ffmpeg")
    monkeypatch.setattr(leveler, "_trim_wav_ffmpeg", lambda src, dst, start, end, ffmpeg: dst.write_bytes(b""))

    measurements = iter(
        [
            LoudnessMeasurement(integrated_lufs=-20.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0),
            LoudnessMeasurement(integrated_lufs=-25.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0),  # -5 LU, over band
        ]
    )
    monkeypatch.setattr(leveler, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: next(measurements))

    results = leveler.music_segment_delta(
        "before.wav", "after.wav", [(0.0, 4.0)], tmp_path, ffmpeg="ffmpeg"
    )
    assert results[0].pass_ is False


def test_music_segment_delta_raises_clearly_without_ffmpeg(monkeypatch: pytest.MonkeyPatch, tmp_path) -> None:
    monkeypatch.setattr(leveler, "resolve_ffmpeg", lambda ffmpeg=None: None)
    with pytest.raises(RuntimeError, match="ffmpeg not found"):
        leveler.music_segment_delta("before.wav", "after.wav", [(0.0, 4.0)], tmp_path, ffmpeg=None)


def test_music_segment_delta_factors_out_program_normalization(
    monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    # The bed rose +5 LU absolutely, but the master normalized the whole program +4 LU;
    # the leveler only pumped the bed +1 LU relative to program, which is within band.
    # The old absolute-only metric would have failed this correctly-mastered clip.
    monkeypatch.setattr(leveler, "resolve_ffmpeg", lambda ffmpeg=None: "ffmpeg")
    monkeypatch.setattr(leveler, "_trim_wav_ffmpeg", lambda src, dst, start, end, ffmpeg: dst.write_bytes(b""))
    measurements = iter(
        [
            LoudnessMeasurement(integrated_lufs=-20.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0),  # before seg
            LoudnessMeasurement(integrated_lufs=-15.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0),  # after seg (+5)
        ]
    )
    monkeypatch.setattr(leveler, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: next(measurements))

    results = leveler.music_segment_delta(
        "before.wav", "after.wav", [(0.0, 4.0)], tmp_path, ffmpeg="ffmpeg", program_delta_lu=4.0
    )
    assert results[0].delta_lu == pytest.approx(5.0)
    assert results[0].program_delta_lu == pytest.approx(4.0)
    assert results[0].relative_delta_lu == pytest.approx(1.0)
    assert results[0].pass_ is True


def test_music_segment_delta_fails_when_bed_pumped_beyond_program(
    monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    # Program moved +4 LU but the bed jumped +7 LU: +3 LU of genuine leveler pumping,
    # over the 2 LU band — this must still fail after factoring out normalization.
    monkeypatch.setattr(leveler, "resolve_ffmpeg", lambda ffmpeg=None: "ffmpeg")
    monkeypatch.setattr(leveler, "_trim_wav_ffmpeg", lambda src, dst, start, end, ffmpeg: dst.write_bytes(b""))
    measurements = iter(
        [
            LoudnessMeasurement(integrated_lufs=-20.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0),
            LoudnessMeasurement(integrated_lufs=-13.0, true_peak_dbtp=-3.0, loudness_range_lu=1.0),  # +7
        ]
    )
    monkeypatch.setattr(leveler, "measure_lufs_ffmpeg", lambda path, ffmpeg=None: next(measurements))

    results = leveler.music_segment_delta(
        "before.wav", "after.wav", [(0.0, 4.0)], tmp_path, ffmpeg="ffmpeg", program_delta_lu=4.0
    )
    assert results[0].relative_delta_lu == pytest.approx(3.0)
    assert results[0].pass_ is False
