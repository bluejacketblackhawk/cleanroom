"""Tests for eval/synth.py: the synthetic paired-degradation corpus generator.

Hermetic — only needs numpy/scipy/soundfile (the harness's base deps, always present
per requirements.txt), no ffmpeg/model/anvil binary. `test_run.py::test_cmd_synth_*`
covers the `run.py synth` CLI wiring; this file covers the underlying signal-processing
primitives and `generate_corpus`.
"""

from __future__ import annotations

import numpy as np
import pytest

import run
import synth


SR = 16000  # small sample rate keeps these tests fast


# --- signal primitives --------------------------------------------------------------


def test_synth_speech_like_is_bounded_and_nonzero() -> None:
    sig = synth.synth_speech_like(duration_s=2.0, sr=SR, seed=1)
    assert len(sig) == int(2.0 * SR)
    assert np.max(np.abs(sig)) <= 1.0
    assert np.max(np.abs(sig)) > 0.01


def test_synth_speech_like_is_deterministic_per_seed() -> None:
    a = synth.synth_speech_like(1.0, SR, seed=7)
    b = synth.synth_speech_like(1.0, SR, seed=7)
    c = synth.synth_speech_like(1.0, SR, seed=8)
    assert np.array_equal(a, b)
    assert not np.array_equal(a, c)


@pytest.mark.parametrize("kind", ["white", "pink", "brown"])
def test_colored_noise_is_unit_rms_and_bounded_length(kind: str) -> None:
    n = SR * 2
    noise = synth.colored_noise(kind, n, seed=1)
    assert len(noise) == n
    assert np.std(noise) == pytest.approx(1.0, rel=0.1)


def test_colored_noise_rejects_unknown_kind() -> None:
    with pytest.raises(ValueError):
        synth.colored_noise("purple", 100, seed=1)


def test_pink_noise_has_more_low_frequency_energy_than_high() -> None:
    n = SR * 4
    pink = synth.colored_noise("pink", n, seed=3)
    spectrum = np.abs(np.fft.rfft(pink))
    freqs = np.fft.rfftfreq(n, d=1.0 / SR)
    low_energy = float(np.sum(spectrum[(freqs > 50) & (freqs < 500)] ** 2))
    high_energy = float(np.sum(spectrum[(freqs > 4000) & (freqs < 7000)] ** 2))
    assert low_energy > high_energy


def test_synth_rir_direct_spike_and_decay() -> None:
    rir = synth.synth_rir(rt60_s=0.5, sr=SR, seed=1)
    assert rir[0] == pytest.approx(np.max(np.abs(rir)), rel=1e-6) or abs(rir[0]) > 0
    # energy should be concentrated early (decay), not flat over the whole tail
    half = len(rir) // 2
    first_half_energy = float(np.sum(rir[:half] ** 2))
    second_half_energy = float(np.sum(rir[half:] ** 2))
    assert first_half_energy > second_half_energy


def test_apply_rir_preserves_rms_and_lengthens_effective_tail() -> None:
    sig = synth.synth_speech_like(2.0, SR, seed=2)
    rir = synth.synth_rir(0.6, SR, seed=2)
    wet = synth.apply_rir(sig, rir)
    assert len(wet) == len(sig)
    dry_rms = float(np.sqrt(np.mean(sig**2)))
    wet_rms = float(np.sqrt(np.mean(wet**2)))
    assert wet_rms == pytest.approx(dry_rms, rel=0.05)


def test_mix_at_snr_hits_target_snr() -> None:
    rng = np.random.default_rng(5)
    clean = 0.2 * np.sin(2 * np.pi * 300 * np.arange(SR * 2) / SR)
    noise = rng.standard_normal(SR * 2).astype(np.float32)

    mixed, clean_ref = synth.mix_at_snr(clean, noise, snr_db=10.0)
    residual_noise = mixed - clean_ref
    clean_rms = float(np.sqrt(np.mean(clean_ref**2)))
    noise_rms = float(np.sqrt(np.mean(residual_noise**2)))
    achieved_snr = 20 * np.log10(clean_rms / noise_rms)
    assert achieved_snr == pytest.approx(10.0, abs=0.5)


def test_mix_at_snr_never_clips() -> None:
    clean = 0.9 * np.sin(2 * np.pi * 300 * np.arange(SR) / SR)
    noise = np.random.default_rng(1).standard_normal(SR).astype(np.float32)
    mixed, clean_ref = synth.mix_at_snr(clean, noise, snr_db=-10.0)  # noise-dominant, would clip unscaled
    assert np.max(np.abs(mixed)) <= 0.99
    assert len(clean_ref) == len(clean)


def test_apply_clip_respects_ceiling() -> None:
    sig = np.linspace(-2.0, 2.0, 1000).astype(np.float32)
    clipped = synth.apply_clip(sig, ceiling=0.5)
    assert np.max(np.abs(clipped)) == pytest.approx(0.5)


def test_apply_bandwidth_limit_removes_out_of_band_energy() -> None:
    n = SR * 2
    t = np.arange(n) / SR
    # An in-band tone dominates the signal's real energy, so the out-of-band tones'
    # surviving fraction is a meaningful measure of attenuation (without it, filtering
    # both input tones near-silent leaves the ratio dominated by filter-artifact noise
    # floor on both sides, which isn't what this test is trying to check).
    in_band_tone = 0.3 * np.sin(2 * np.pi * 1000 * t)
    low_tone = 0.3 * np.sin(2 * np.pi * 100 * t)
    high_tone = 0.3 * np.sin(2 * np.pi * 6000 * t)
    combined = (in_band_tone + low_tone + high_tone).astype(np.float32)

    limited = synth.apply_bandwidth_limit(combined, SR, 300.0, 3400.0)  # phone band
    spectrum = np.abs(np.fft.rfft(limited))
    freqs = np.fft.rfftfreq(n, d=1.0 / SR)
    energy_near_6k = float(np.sum(spectrum[(freqs > 5500) & (freqs < 6500)] ** 2))
    energy_near_100 = float(np.sum(spectrum[(freqs > 50) & (freqs < 150)] ** 2))
    total_energy = float(np.sum(spectrum**2))
    assert energy_near_6k / total_energy < 0.01
    assert energy_near_100 / total_energy < 0.01  # also outside the 300-3400Hz band


def test_apply_level_gaps_changes_segment_levels() -> None:
    sig = 0.1 * np.sin(2 * np.pi * 220 * np.arange(SR * 8) / SR)
    gapped = synth.apply_level_gaps(sig, SR, seed=1, segment_s=2.0)
    assert len(gapped) == len(sig)
    assert not np.allclose(gapped, sig)


def test_resample_to_changes_length_proportionally() -> None:
    sig = np.zeros(SR, dtype=np.float32)
    out = synth.resample_to(sig, SR, SR // 2)
    assert abs(len(out) - SR // 2) <= 2


def test_fit_or_loop_to_duration_loops_short_and_trims_long() -> None:
    short = np.arange(10, dtype=np.float32)
    looped = synth.fit_or_loop_to_duration(short, 25, seed=1)
    assert len(looped) == 25

    long = np.arange(1000, dtype=np.float32)
    trimmed = synth.fit_or_loop_to_duration(long, 100, seed=1)
    assert len(trimmed) == 100


# --- generate_corpus (end to end, small) ----------------------------------------------


def test_generate_corpus_writes_valid_manifest(tmp_path) -> None:
    out_dir = tmp_path / "synth-corpus"
    manifest = synth.generate_corpus(
        out_dir, classes=["clean-studio", "constant-broadband-noise"], variants_per_class=2,
        duration_s=1.0, sr=SR, seed=42,
    )
    assert len(manifest["clips"]) == 4
    errors = run.validate(manifest)
    assert errors == []

    for clip in manifest["clips"]:
        degraded_path = out_dir / clip["path"]
        reference_path = out_dir / clip["ground_truth"]["reference_wav"]
        assert degraded_path.is_file()
        assert reference_path.is_file()


def test_generate_corpus_clean_studio_degraded_equals_reference(tmp_path) -> None:
    import soundfile as sf

    out_dir = tmp_path / "synth-corpus"
    manifest = synth.generate_corpus(out_dir, classes=["clean-studio"], variants_per_class=1, duration_s=1.0, sr=SR, seed=1)
    clip = manifest["clips"][0]
    degraded, _ = sf.read(str(out_dir / clip["path"]))
    reference, _ = sf.read(str(out_dir / clip["ground_truth"]["reference_wav"]))
    assert np.array_equal(degraded, reference)


def test_generate_corpus_rejects_unknown_class(tmp_path) -> None:
    with pytest.raises(ValueError, match="no synth recipe"):
        synth.generate_corpus(tmp_path / "out", classes=["not-a-real-class"], variants_per_class=1)


def test_generate_corpus_music_plus_speech_has_music_segments(tmp_path) -> None:
    out_dir = tmp_path / "synth-corpus"
    manifest = synth.generate_corpus(
        out_dir, classes=["music-plus-speech"], variants_per_class=1, duration_s=6.0, sr=SR, seed=1
    )
    clip = manifest["clips"][0]
    assert "music_segments_s" in clip["ground_truth"]
    assert len(clip["ground_truth"]["music_segments_s"]) >= 1


def test_generate_corpus_uses_clean_dir_when_given(tmp_path) -> None:
    import soundfile as sf

    clean_dir = tmp_path / "clean_wavs"
    clean_dir.mkdir()
    real_clean = 0.3 * np.sin(2 * np.pi * 440 * np.arange(SR * 3) / SR)
    sf.write(str(clean_dir / "take1.wav"), real_clean.astype(np.float32), SR)

    out_dir = tmp_path / "synth-corpus"
    manifest = synth.generate_corpus(
        out_dir, classes=["clean-studio"], variants_per_class=1, duration_s=1.0, sr=SR, seed=1, clean_dir=clean_dir
    )
    assert "clean-dir input" in manifest["clips"][0]["source"]
    # Real clean speech backs the fixture -> perceptual MOS gates (DNSMOS/PESQ/STOI) apply.
    assert manifest["speech_source"] == "real"
    assert manifest["clips"][0]["speech_source"] == "real"


def test_stable_variant_offset_is_deterministic_and_bounded() -> None:
    # Must be reproducible across calls/processes (unlike builtin hash()) and in range.
    for cls in ("clean-studio", "hum-50-60hz", "worstcase-laptop-reverb-noise"):
        for i in range(4):
            off = synth._stable_variant_offset(cls, i)
            assert 0 <= off < 10_000
            assert off == synth._stable_variant_offset(cls, i)
    # Distinct (class, variant) keys should not all collide onto one offset.
    offsets = {synth._stable_variant_offset(c, i) for c in ("a", "b", "c") for i in range(3)}
    assert len(offsets) > 1


def test_generate_corpus_is_reproducible_across_regenerations(tmp_path) -> None:
    # Regression guard for the PYTHONHASHSEED bug: two independent generations with the
    # same seed must produce byte-identical audio, so the corpus (and every baseline built
    # from it) is stable across processes.
    import soundfile as sf

    a = synth.generate_corpus(tmp_path / "a", classes=["hum-50-60hz"], variants_per_class=2, duration_s=1.0, sr=SR, seed=7)
    b = synth.generate_corpus(tmp_path / "b", classes=["hum-50-60hz"], variants_per_class=2, duration_s=1.0, sr=SR, seed=7)
    for ca, cb in zip(a["clips"], b["clips"]):
        wa, _ = sf.read(str(tmp_path / "a" / ca["path"]))
        wb, _ = sf.read(str(tmp_path / "b" / cb["path"]))
        assert np.array_equal(wa, wb)


def test_generate_corpus_marks_synthetic_speech_source_without_clean_dir(tmp_path) -> None:
    # No --clean-dir: speech is the synth_speech_like proxy, so the manifest must flag it
    # "synthetic" and master-eval treats the perceptual MOS gates as report-only.
    manifest = synth.generate_corpus(
        tmp_path / "out", classes=["clean-studio"], variants_per_class=1, duration_s=1.0, sr=SR, seed=1
    )
    assert manifest["speech_source"] == "synthetic"
    assert manifest["clips"][0]["speech_source"] == "synthetic"
