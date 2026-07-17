"""Tests for eval/metrics/loudness.py.

These run WITHOUT ffmpeg installed: `parse_ebur128_summary` is tested against
captured samples of real ffmpeg stderr text (not live subprocess output), and
`resolve_ffmpeg`/`ffmpeg_available` are tested by monkeypatching `shutil.which`.
`measure_lufs_ffmpeg` itself (which shells out) is exercised only for its
ffmpeg-not-found / input-not-found error paths, which also don't need ffmpeg.
"""

from __future__ import annotations

import pytest

from metrics.loudness import (
    LoudnessMeasurement,
    ffmpeg_available,
    measure_lufs_ffmpeg,
    parse_ebur128_summary,
    resolve_ffmpeg,
)

# Captured verbatim from a real run against this repo's ffmpeg 6.1.1-essentials
# (ffmpeg-static), with the exact filter string measure_lufs_ffmpeg uses:
#   ffmpeg -hide_banner -nostats -i <in> -filter_complex "ebur128=peak=true:framelog=quiet" -f null -
# `framelog=quiet` suppresses the per-frame log lines, so this is the clean case.
REAL_EBUR128_STDERR_QUIET = """\
Input #0, wav, from 'test_fixture.wav':
  Duration: 00:00:12.00, bitrate: 1536 kb/s
  Stream #0:0: Audio: pcm_s16le ([1][0][0][0] / 0x0001), 48000 Hz, stereo, s16, 1536 kb/s
Stream mapping:
  Stream #0:0 (pcm_s16le) -> ebur128:default
  ebur128:out0 -> Stream #0:0 (pcm_s16le)
Press [q] to stop, [?] for help
Output #0, null, to 'pipe:':
  Stream #0:0: Audio: pcm_s16le, 48000 Hz, stereo, s16, 1536 kb/s
[out#0/null @ 0000023d7a47d600] video:0kB audio:2250kB subtitle:0kB other streams:0kB global headers:0kB muxing overhead: unknown
size=N/A time=00:00:11.90 bitrate=N/A speed= 220x
[Parsed_ebur128_0 @ 0000023d7a47d600] Summary:

  Integrated loudness:
    I:         -16.0 LUFS
    Threshold: -26.0 LUFS

  Loudness range:
    LRA:         0.0 LU
    Threshold: -36.0 LUFS
    LRA low:   -16.0 LUFS
    LRA high:  -16.0 LUFS

  True peak:
    Peak:       -5.9 dBFS
"""

# Captured with framelog left at its default (per-frame lines before the Summary).
# The parser must grab the *final* Summary block, not an early per-frame reading —
# note the frame lines report LRA: 0.0 while the summary reports LRA: 20.0.
REAL_EBUR128_STDERR_WITH_FRAMELOG = """\
[Parsed_ebur128_0 @ 000001819b191800] t: 0.699977   TARGET:-23 LUFS    M: -21.1 S:-120.7     I: -21.1 LUFS       LRA:   0.0 LU  FTPK: -18.1 dBFS  TPK: -18.1 dBFS
[Parsed_ebur128_0 @ 000001819b191800] t: 0.799977   TARGET:-23 LUFS    M: -21.1 S:-120.7     I: -21.1 LUFS       LRA:   0.0 LU  FTPK: -18.1 dBFS  TPK: -18.1 dBFS
[Parsed_ebur128_0 @ 000001819b191800] t: 2.99998    TARGET:-23 LUFS    M: -21.1 S: -21.1     I: -21.1 LUFS       LRA:  20.0 LU  FTPK: -18.1 dBFS  TPK: -18.1 dBFS
[out#0/null @ 000001819b189e80] video:0kB audio:258kB subtitle:0kB other streams:0kB global headers:0kB muxing overhead: unknown
size=N/A time=00:00:02.90 bitrate=N/A speed= 250x
[Parsed_ebur128_0 @ 000001819b191800] Summary:

  Integrated loudness:
    I:         -21.1 LUFS
    Threshold: -31.1 LUFS

  Loudness range:
    LRA:        20.0 LU
    Threshold: -41.1 LUFS
    LRA low:   -41.1 LUFS
    LRA high:  -21.1 LUFS

  True peak:
    Peak:      -18.1 dBFS
"""


def test_parse_ebur128_summary_quiet() -> None:
    measurement = parse_ebur128_summary(REAL_EBUR128_STDERR_QUIET)
    assert measurement == LoudnessMeasurement(
        integrated_lufs=-16.0, true_peak_dbtp=-5.9, loudness_range_lu=0.0
    )


def test_parse_ebur128_summary_ignores_per_frame_lines() -> None:
    # Frame lines report I: -21.1 / LRA: 0.0 repeatedly; only the trailing Summary
    # block (I: -21.1, LRA: 20.0, Peak: -18.1) should be trusted.
    measurement = parse_ebur128_summary(REAL_EBUR128_STDERR_WITH_FRAMELOG)
    assert measurement == LoudnessMeasurement(
        integrated_lufs=-21.1, true_peak_dbtp=-18.1, loudness_range_lu=20.0
    )


def test_parse_ebur128_summary_positive_and_negative_values() -> None:
    text = """\
[Parsed_ebur128_0 @ 0x0] Summary:

  Integrated loudness:
    I:          -0.3 LUFS
    Threshold: -10.3 LUFS

  Loudness range:
    LRA:         3.5 LU
    Threshold: -20.3 LUFS
    LRA low:   -12.0 LUFS
    LRA high:   -8.5 LUFS

  True peak:
    Peak:        0.4 dBFS
"""
    measurement = parse_ebur128_summary(text)
    assert measurement.integrated_lufs == -0.3
    assert measurement.true_peak_dbtp == 0.4
    assert measurement.loudness_range_lu == 3.5


def test_parse_ebur128_summary_missing_summary_block_raises() -> None:
    with pytest.raises(RuntimeError, match="Summary"):
        parse_ebur128_summary("ffmpeg version 6.1.1\nsome unrelated error output\n")


def test_parse_ebur128_summary_truncated_summary_raises() -> None:
    text = """\
[Parsed_ebur128_0 @ 0x0] Summary:

  Integrated loudness:
    I:         -16.0 LUFS
"""
    with pytest.raises(RuntimeError, match="true peak"):
        parse_ebur128_summary(text)


def test_resolve_ffmpeg_prefers_explicit_path(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("metrics.loudness.shutil.which", lambda cmd: f"/resolved/{cmd}")
    monkeypatch.setenv("CLEANROOM_FFMPEG", "env-ffmpeg")
    assert resolve_ffmpeg("explicit-ffmpeg") == "/resolved/explicit-ffmpeg"


def test_resolve_ffmpeg_falls_back_to_env_var(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("metrics.loudness.shutil.which", lambda cmd: f"/resolved/{cmd}")
    monkeypatch.setenv("CLEANROOM_FFMPEG", "env-ffmpeg")
    assert resolve_ffmpeg(None) == "/resolved/env-ffmpeg"


def test_resolve_ffmpeg_falls_back_to_path(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("CLEANROOM_FFMPEG", raising=False)

    def fake_which(cmd: str) -> str | None:
        return "/usr/bin/ffmpeg" if cmd == "ffmpeg" else None

    monkeypatch.setattr("metrics.loudness.shutil.which", fake_which)
    assert resolve_ffmpeg(None) == "/usr/bin/ffmpeg"


def test_resolve_ffmpeg_returns_none_when_nothing_found(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("CLEANROOM_FFMPEG", raising=False)
    monkeypatch.setattr("metrics.loudness.shutil.which", lambda cmd: None)
    assert resolve_ffmpeg(None) is None
    assert ffmpeg_available(None) is False


def test_ffmpeg_available_true_when_resolved(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("metrics.loudness.shutil.which", lambda cmd: "/usr/bin/ffmpeg")
    assert ffmpeg_available("ffmpeg") is True


def test_measure_lufs_ffmpeg_missing_input_file_raises_without_ffmpeg() -> None:
    # The input-file check runs before ffmpeg is even resolved, so this is a
    # deterministic, ffmpeg-free failure path.
    with pytest.raises(RuntimeError, match="input file not found"):
        measure_lufs_ffmpeg("definitely/does/not/exist.wav")


def test_measure_lufs_ffmpeg_missing_ffmpeg_raises_clear_error(
    tmp_path, monkeypatch: pytest.MonkeyPatch
) -> None:
    existing = tmp_path / "clip.wav"
    existing.write_bytes(b"RIFF....WAVEfmt ")  # content is irrelevant; never reaches ffmpeg
    monkeypatch.delenv("CLEANROOM_FFMPEG", raising=False)
    monkeypatch.setattr("metrics.loudness.shutil.which", lambda cmd: None)
    with pytest.raises(RuntimeError, match="ffmpeg not found"):
        measure_lufs_ffmpeg(existing)
