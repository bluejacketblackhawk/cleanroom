"""Synthetic paired-degradation corpus generator (`run.py synth`, 06 §1).

Real bad recordings are the biggest quality lever and stay owner-supplied (07 §6.6), but
until they land this module manufactures a *paired* corpus: known-clean signal in, a
labeled degradation (RIR convolution, additive noise at a known SNR, hum, clipping,
bandwidth limiting, level gaps, or a music bed) out — with the clean signal kept as the
reference. That pairing is what makes PESQ/STOI (metrics/intrusive.py) possible at all
without real recordings, and it gives `master-eval` known-target fixtures for classes 1,
2, 3, 4, 5, 6, 7, 8, 9 and 12 of the 06 §1 taxonomy. Classes 10 (multitrack bleed, needs
a real double-ender pair) and 11 (non-English, needs real other-language speech) are not
synthesized here — they need real/sourced content and stay owner-supplied.

The "clean speech" itself is synthesized too by default (a source-filter harmonic signal
with a syllabic amplitude envelope and formant-shaped resonances — NOT a speech
recognizer/vocoder, just a spectrally speech-*shaped* test signal), or a folder of real
clean wavs can be supplied with `--clean-dir` and used instead (cycled through). Either
way, DNSMOS/PESQ/STOI absolute scores on the synthesized-speech variant should be read as
a structural self-test of the harness's plumbing, not as calibrated MOS predictions —
those models are trained on real speech.
"""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass
from math import gcd
from pathlib import Path
from typing import Callable

import numpy as np

SAMPLE_RATE = 48_000
DEFAULT_DURATION_S = 20.0


def _stable_variant_offset(class_name: str, variant_idx: int) -> int:
    """A deterministic per-(class, variant) seed offset in [0, 10_000).

    MUST NOT use the builtin `hash()`: Python salts string/tuple hashing per process
    (PYTHONHASHSEED), so `hash((class_name, variant_idx))` returns a different value every
    interpreter run — which made the whole synthetic corpus non-reproducible (each
    `run.py synth` produced different noise/RIR/hum realizations, so determinism/regress
    baselines and cross-run master-eval comparisons were meaningless). A stable digest
    keeps the corpus bit-identical across regenerations, processes, and machines.
    """
    digest = hashlib.sha256(f"{class_name}:{variant_idx}".encode()).digest()
    return int.from_bytes(digest[:4], "big") % 10_000

# --- signal primitives ---------------------------------------------------------------


def synth_speech_like(duration_s: float, sr: int, seed: int) -> np.ndarray:
    """A source-filter harmonic signal with a wandering pitch, syllabic amplitude
    envelope (bursts + pauses), and three formant-like resonances. See module
    docstring: a speech-*shaped* proxy signal, not real or synthesized-via-TTS speech.
    """
    rng = np.random.default_rng(seed)
    n = int(duration_s * sr)
    t = np.arange(n) / sr

    f0_base = rng.uniform(100, 200)
    f0_wander = 15.0 * np.sin(2 * np.pi * 0.3 * t + rng.uniform(0, 2 * np.pi))
    f0 = f0_base + f0_wander
    phase = 2 * np.pi * np.cumsum(f0) / sr

    source = np.zeros(n)
    for k in range(1, 8):
        source += (1.0 / k) * np.sin(k * phase)
    source /= np.max(np.abs(source)) + 1e-9

    syll_rate = rng.uniform(3.5, 5.0)
    env = 0.5 + 0.5 * np.sin(2 * np.pi * syll_rate * t + rng.uniform(0, 2 * np.pi))
    env = env**2

    pause_rate = rng.uniform(0.3, 0.6)
    gate = 0.5 + 0.5 * np.sin(2 * np.pi * pause_rate * t + rng.uniform(0, 2 * np.pi))
    gate = np.clip((gate - 0.15) / 0.85, 0.0, 1.0)

    excited = source * env * gate

    from scipy.signal import butter, sosfilt

    formants = [(700.0, 130.0), (1220.0, 110.0), (2600.0, 170.0)]  # ~ vowel /a/
    shaped = np.zeros(n)
    for center, bw in formants:
        low = max(center - bw / 2, 40.0) / (sr / 2)
        high = min(center + bw / 2, sr / 2 - 100.0) / (sr / 2)
        sos = butter(2, [low, high], btype="band", output="sos")
        shaped += sosfilt(sos, excited)

    shaped /= np.max(np.abs(shaped)) + 1e-9
    return (0.6 * shaped).astype(np.float32)


def colored_noise(kind: str, n_samples: int, seed: int) -> np.ndarray:
    """White/pink/brown noise, unit-RMS normalized. Pink/brown are shaped in the
    frequency domain (1/sqrt(f) and 1/f respectively) — no external noise-synthesis dep.
    """
    rng = np.random.default_rng(seed)
    white = rng.standard_normal(n_samples)
    if kind == "white":
        noise = white
    elif kind in ("pink", "brown"):
        spectrum = np.fft.rfft(white)
        freqs = np.fft.rfftfreq(n_samples)
        shaped = np.zeros_like(spectrum)
        nonzero = freqs > 0  # drop the DC bin entirely rather than dividing by ~0,
        # which would blow a random DC offset up into a dominant constant term
        shaped[nonzero] = spectrum[nonzero] / (
            np.sqrt(freqs[nonzero]) if kind == "pink" else freqs[nonzero]
        )
        noise = np.fft.irfft(shaped, n=n_samples)
    else:
        raise ValueError(f"unknown noise kind {kind!r} (want white/pink/brown)")
    rms = float(np.sqrt(np.mean(np.square(noise))))
    if rms > 0:
        noise = noise / rms
    return noise.astype(np.float32)


def synth_rir(rt60_s: float, sr: int, seed: int, tail_s: float | None = None) -> np.ndarray:
    """Exponential-decay noise impulse response at a target RT60 (the -60dB point of an
    exponential envelope: envelope(t) = exp(-t/tau), tau = RT60 / ln(1000))."""
    tail_s = tail_s if tail_s is not None else min(rt60_s * 1.5, 2.5)
    n = max(int(tail_s * sr), 1)
    rng = np.random.default_rng(seed)
    t = np.arange(n) / sr
    tau = rt60_s / np.log(1000.0)
    envelope = np.exp(-t / tau)
    rir = rng.standard_normal(n) * envelope
    rir[0] = 1.0  # direct-sound spike
    energy = float(np.sqrt(np.sum(rir**2)))
    if energy > 0:
        rir = rir / energy
    return rir.astype(np.float32)


def apply_rir(signal: np.ndarray, rir: np.ndarray) -> np.ndarray:
    """Convolve with an RIR, then rescale to the dry signal's RMS so the degradation is
    reverberant character/tail, not an overall level change."""
    from scipy.signal import fftconvolve

    wet = fftconvolve(signal, rir, mode="full")[: len(signal)]
    dry_rms = float(np.sqrt(np.mean(signal**2))) + 1e-12
    wet_rms = float(np.sqrt(np.mean(wet**2))) + 1e-12
    return (wet * (dry_rms / wet_rms)).astype(np.float32)


def mix_at_snr(clean: np.ndarray, noise: np.ndarray, snr_db: float) -> tuple[np.ndarray, np.ndarray]:
    """Mix `noise` into `clean` at a known SNR (RMS-based). Returns (degraded,
    clean_reference) — both scaled down together if the mix would clip, so the returned
    pair's SNR is exactly `snr_db` regardless of headroom.
    """
    if len(noise) < len(clean):
        reps = int(np.ceil(len(clean) / len(noise)))
        noise = np.tile(noise, reps)
    noise = noise[: len(clean)]

    clean_rms = float(np.sqrt(np.mean(clean**2))) + 1e-12
    noise_rms = float(np.sqrt(np.mean(noise**2))) + 1e-12
    target_noise_rms = clean_rms / (10 ** (snr_db / 20))
    noise_scaled = noise * (target_noise_rms / noise_rms)

    mixed = clean + noise_scaled
    peak = float(np.max(np.abs(mixed)))
    if peak > 0.98:
        factor = 0.98 / peak
        mixed = mixed * factor
        clean_out = clean * factor
    else:
        clean_out = clean
    return mixed.astype(np.float32), clean_out.astype(np.float32)


def add_hum(signal: np.ndarray, sr: int, freq_hz: float, seed: int, hum_db_below_peak: float = 18.0) -> np.ndarray:
    n = len(signal)
    t = np.arange(n) / sr
    rng = np.random.default_rng(seed)
    hum = np.sin(2 * np.pi * freq_hz * t)
    for k, amp in ((2, 0.5), (3, 0.25)):
        hum = hum + amp * np.sin(2 * np.pi * freq_hz * k * t + rng.uniform(0, 2 * np.pi))
    hum /= np.max(np.abs(hum)) + 1e-9
    sig_peak = float(np.max(np.abs(signal))) + 1e-9
    hum_amp = sig_peak * (10 ** (-hum_db_below_peak / 20))
    return (signal + hum * hum_amp).astype(np.float32)


def apply_bandwidth_limit(signal: np.ndarray, sr: int, low_hz: float, high_hz: float) -> np.ndarray:
    from scipy.signal import butter, sosfiltfilt

    nyq = sr / 2
    low = max(low_hz, 1.0) / nyq
    high = min(high_hz, nyq - 100.0) / nyq
    sos = butter(4, [low, high], btype="band", output="sos")
    return sosfiltfilt(sos, signal).astype(np.float32)


def apply_clip(signal: np.ndarray, ceiling: float = 0.7) -> np.ndarray:
    return np.clip(signal, -ceiling, ceiling).astype(np.float32)


def apply_level_gaps(
    signal: np.ndarray, sr: int, seed: int, segment_s: float = 4.0, gap_db_range: tuple[float, float] = (-18.0, 6.0)
) -> np.ndarray:
    """Randomly re-gains fixed-length segments — a quiet-guest/loud-host stand-in."""
    rng = np.random.default_rng(seed)
    n = len(signal)
    seg_len = max(int(segment_s * sr), 1)
    out = signal.copy()
    for start in range(0, n, seg_len):
        end = min(start + seg_len, n)
        gain_db = rng.uniform(*gap_db_range)
        out[start:end] = out[start:end] * (10 ** (gain_db / 20))
    return out.astype(np.float32)


def synth_music_bed(duration_s: float, sr: int, seed: int) -> np.ndarray:
    rng = np.random.default_rng(seed)
    n = int(duration_s * sr)
    t = np.arange(n) / sr
    chord_freqs = (220.0, 277.18, 329.63)  # A3-C#4-E4 triad
    bed = np.zeros(n)
    for f in chord_freqs:
        bed += np.sin(2 * np.pi * f * t + rng.uniform(0, 2 * np.pi))
    swell = 0.7 + 0.3 * np.sin(2 * np.pi * 0.1 * t)
    bed *= swell
    bed /= np.max(np.abs(bed)) + 1e-9
    return bed.astype(np.float32)


def mix_music_plus_speech(
    speech: np.ndarray, music: np.ndarray, segments_s: list[tuple[float, float]], sr: int, music_db_below_speech: float = 6.0
) -> np.ndarray:
    out = speech.copy()
    speech_rms = float(np.sqrt(np.mean(speech**2))) + 1e-12
    music_rms = float(np.sqrt(np.mean(music**2))) + 1e-12
    music_gain = speech_rms * (10 ** (-music_db_below_speech / 20)) / music_rms
    for start_s, end_s in segments_s:
        s, e = int(start_s * sr), min(int(end_s * sr), len(out))
        seg_len = e - s
        if seg_len <= 0:
            continue
        out[s:e] = out[s:e] + music[:seg_len] * music_gain
    return out.astype(np.float32)


def resample_to(audio: np.ndarray, sr: int, target_sr: int) -> np.ndarray:
    if sr == target_sr:
        return audio.astype(np.float32)
    from scipy.signal import resample_poly

    g = gcd(sr, target_sr)
    up, down = target_sr // g, sr // g
    return resample_poly(audio, up, down).astype(np.float32)


def load_clean_pool(clean_dir: Path, sr: int) -> list[np.ndarray]:
    """Load every *.wav under `clean_dir`, mixed to mono and resampled to `sr`."""
    import soundfile as sf

    pool = []
    for wav_path in sorted(clean_dir.glob("*.wav")):
        audio, file_sr = sf.read(str(wav_path), always_2d=False, dtype="float32")
        if audio.ndim > 1:
            audio = audio.mean(axis=1)
        pool.append(resample_to(audio.astype(np.float32), file_sr, sr))
    return pool


def load_noise_pool(noise_dir: Path, sr: int) -> list[np.ndarray]:
    return load_clean_pool(noise_dir, sr)  # same load/resample/mono logic


def fit_or_loop_to_duration(audio: np.ndarray, n_samples: int, seed: int) -> np.ndarray:
    """Loop (if shorter) or take a deterministic random window (if longer) to hit
    exactly `n_samples`, so pool-sourced clips can stand in for any recipe's duration."""
    if len(audio) == n_samples:
        return audio.astype(np.float32)
    if len(audio) < n_samples:
        reps = int(np.ceil(n_samples / max(len(audio), 1)))
        return np.tile(audio, reps)[:n_samples].astype(np.float32)
    rng = np.random.default_rng(seed)
    start = int(rng.integers(0, len(audio) - n_samples + 1))
    return audio[start : start + n_samples].astype(np.float32)


# --- class recipes ---------------------------------------------------------------------
#
# Each recipe takes the clean base signal + a seed and returns (degraded, reference,
# meta). `reference` is usually just `clean` unchanged, except where the degradation
# pipeline rescales it too (mix_at_snr) to keep the paired SNR exact.


@dataclass
class SynthVariant:
    class_name: str
    variant_id: str
    degraded: np.ndarray
    reference: np.ndarray
    meta: dict


def _recipe_clean_studio(clean: np.ndarray, sr: int, seed: int, variant_idx: int, noise_pool: list[np.ndarray]) -> SynthVariant:
    return SynthVariant(
        "clean-studio", f"clean-{variant_idx:02d}", clean.copy(), clean.copy(),
        {"notes": "must-not-degrade control: degraded == reference (identical signal)."},
    )


def _recipe_room_echo(clean: np.ndarray, sr: int, seed: int, variant_idx: int, noise_pool: list[np.ndarray]) -> SynthVariant:
    rt60 = [0.4, 0.8, 1.3][variant_idx % 3]
    rir = synth_rir(rt60, sr, seed)
    degraded = apply_rir(clean, rir)
    return SynthVariant(
        "untreated-room-echo", f"echo-rt60-{rt60:.1f}s-{variant_idx:02d}", degraded, clean.copy(),
        {"rir_rt60_s": rt60, "notes": "synthetic exponential-decay RIR convolution."},
    )


def _recipe_broadband_noise(clean: np.ndarray, sr: int, seed: int, variant_idx: int, noise_pool: list[np.ndarray]) -> SynthVariant:
    snr = [15.0, 10.0, 5.0][variant_idx % 3]
    kind = ["white", "pink", "pink"][variant_idx % 3]
    noise = noise_pool[variant_idx % len(noise_pool)] if noise_pool else colored_noise(kind, len(clean), seed)
    degraded, ref = mix_at_snr(clean, noise, snr)
    return SynthVariant(
        "constant-broadband-noise", f"broadband-{kind}-snr{snr:g}-{variant_idx:02d}", degraded, ref,
        {"snr_db": snr, "noise_kind": kind, "notes": "constant additive noise at a known SNR."},
    )


def _recipe_dynamic_noise(clean: np.ndarray, sr: int, seed: int, variant_idx: int, noise_pool: list[np.ndarray]) -> SynthVariant:
    snr = [10.0, 5.0, 0.0][variant_idx % 3]
    base = noise_pool[variant_idx % len(noise_pool)] if noise_pool else colored_noise("pink", len(clean), seed)
    if len(base) < len(clean):
        base = np.tile(base, int(np.ceil(len(clean) / len(base))))
    base = base[: len(clean)]
    rng = np.random.default_rng(seed)
    burst_gate = np.zeros(len(clean))
    seg = max(int(1.5 * sr), 1)
    for start in range(0, len(clean), seg):
        end = min(start + seg, len(clean))
        if rng.uniform() < 0.5:
            burst_gate[start:end] = 1.0
    intermittent = base * burst_gate
    degraded, ref = mix_at_snr(clean, intermittent, snr)
    return SynthVariant(
        "dynamic-noise", f"dynamic-snr{snr:g}-{variant_idx:02d}", degraded, ref,
        {"snr_db": snr, "notes": "intermittent (gated on/off) noise bursts, known average SNR."},
    )


def _recipe_hum(clean: np.ndarray, sr: int, seed: int, variant_idx: int, noise_pool: list[np.ndarray]) -> SynthVariant:
    freq = [50.0, 60.0, 50.0][variant_idx % 3]
    degraded = add_hum(clean, sr, freq, seed, hum_db_below_peak=[12.0, 15.0, 18.0][variant_idx % 3])
    return SynthVariant(
        "hum-50-60hz", f"hum-{freq:g}hz-{variant_idx:02d}", degraded, clean.copy(),
        {"hum_freq_hz": freq, "notes": "fundamental + 2nd/3rd harmonics mains hum."},
    )


def _recipe_level_gaps(clean: np.ndarray, sr: int, seed: int, variant_idx: int, noise_pool: list[np.ndarray]) -> SynthVariant:
    segment_s = [3.0, 4.0, 5.0][variant_idx % 3]
    degraded = apply_level_gaps(clean, sr, seed, segment_s=segment_s)
    return SynthVariant(
        "level-gaps", f"levelgaps-{segment_s:g}s-{variant_idx:02d}", degraded, clean.copy(),
        {"segment_s": segment_s, "notes": "randomly re-gained fixed-length segments (quiet-guest/loud-host stand-in)."},
    )


def _recipe_music_plus_speech(clean: np.ndarray, sr: int, seed: int, variant_idx: int, noise_pool: list[np.ndarray]) -> SynthVariant:
    music = synth_music_bed(len(clean) / sr, sr, seed)
    duration_s = len(clean) / sr
    segments = [(0.0, min(4.0, duration_s)), (max(duration_s - 4.0, 0.0), duration_s)]
    degraded = mix_music_plus_speech(clean, music, segments, sr, music_db_below_speech=[4.0, 6.0, 8.0][variant_idx % 3])
    return SynthVariant(
        "music-plus-speech", f"music-{variant_idx:02d}", degraded, clean.copy(),
        {"music_segments_s": segments, "notes": "synthetic chord-pad music bed under intro/outro segments."},
    )


def _recipe_clipped(clean: np.ndarray, sr: int, seed: int, variant_idx: int, noise_pool: list[np.ndarray]) -> SynthVariant:
    ceiling = [0.8, 0.6, 0.4][variant_idx % 3]
    degraded = apply_clip(clean * 1.5, ceiling=ceiling)
    return SynthVariant(
        "clipped", f"clip-ceil{ceiling:g}-{variant_idx:02d}", degraded, clean.copy(),
        {"clip_ceiling": ceiling, "notes": "input pre-gained 1.5x then hard-clipped at ceiling."},
    )


def _recipe_bandwidth_limited(clean: np.ndarray, sr: int, seed: int, variant_idx: int, noise_pool: list[np.ndarray]) -> SynthVariant:
    bands = [(300.0, 3400.0), (50.0, 8000.0), (50.0, 16000.0)][variant_idx % 3]
    names = ["phone", "zoom8k", "zoom16k"]
    degraded = apply_bandwidth_limit(clean, sr, *bands)
    return SynthVariant(
        "bandwidth-limited", f"bw-{names[variant_idx % 3]}-{variant_idx:02d}", degraded, clean.copy(),
        {"bandwidth_hz": list(bands), "notes": "Butterworth bandpass emulating phone/Zoom bandwidth."},
    )


def _recipe_worst_case(clean: np.ndarray, sr: int, seed: int, variant_idx: int, noise_pool: list[np.ndarray]) -> SynthVariant:
    rt60 = [0.5, 0.8, 1.1][variant_idx % 3]
    snr = [8.0, 5.0, 2.0][variant_idx % 3]
    rir = synth_rir(rt60, sr, seed)
    reverberant = apply_rir(clean, rir)
    noise = noise_pool[variant_idx % len(noise_pool)] if noise_pool else colored_noise("pink", len(clean), seed + 1)
    noisy, ref = mix_at_snr(reverberant, noise, snr)
    degraded = apply_bandwidth_limit(noisy, sr, 80.0, 8000.0)
    return SynthVariant(
        "worstcase-laptop-reverb-noise", f"worstcase-{variant_idx:02d}", degraded, ref,
        {"rir_rt60_s": rt60, "snr_db": snr, "notes": "RIR + noise + laptop-mic bandwidth limit combined."},
    )


CLASS_RECIPES: dict[str, Callable] = {
    "clean-studio": _recipe_clean_studio,
    "untreated-room-echo": _recipe_room_echo,
    "constant-broadband-noise": _recipe_broadband_noise,
    "dynamic-noise": _recipe_dynamic_noise,
    "hum-50-60hz": _recipe_hum,
    "level-gaps": _recipe_level_gaps,
    "music-plus-speech": _recipe_music_plus_speech,
    "clipped": _recipe_clipped,
    "bandwidth-limited": _recipe_bandwidth_limited,
    "worstcase-laptop-reverb-noise": _recipe_worst_case,
}

# Classes intentionally NOT synthesized (see module docstring): need real sourced
# content, not manufacturable from a single clean signal + DSP degradations.
UNSYNTHESIZABLE_CLASSES = ("multitrack-bleed", "non-english")


def generate_corpus(
    out_dir: Path,
    classes: list[str] | None = None,
    variants_per_class: int = 3,
    duration_s: float = DEFAULT_DURATION_S,
    sr: int = SAMPLE_RATE,
    seed: int = 1234,
    clean_dir: Path | None = None,
    noise_dir: Path | None = None,
) -> dict:
    """Generate the synthetic paired corpus into `out_dir`, writing a manifest.json in
    the same schema `run.py validate`/`class_coverage` already understand (REQUIRED_CLIP_FIELDS
    + a `ground_truth.reference_wav` pointing at the paired clean reference).

    Returns the manifest dict (also written to `out_dir/manifest.json`).
    """
    out_dir.mkdir(parents=True, exist_ok=True)
    classes = classes or list(CLASS_RECIPES.keys())
    unknown = [c for c in classes if c not in CLASS_RECIPES]
    if unknown:
        raise ValueError(
            f"no synth recipe for class(es) {unknown}; available: {sorted(CLASS_RECIPES)} "
            f"(classes {UNSYNTHESIZABLE_CLASSES} need real sourced content, see module docstring)"
        )

    clean_pool = load_clean_pool(clean_dir, sr) if clean_dir else []
    noise_pool = load_noise_pool(noise_dir, sr) if noise_dir else []
    n_samples = int(duration_s * sr)

    import soundfile as sf

    clips = []
    for class_name in classes:
        recipe = CLASS_RECIPES[class_name]
        for variant_idx in range(variants_per_class):
            variant_seed = seed + _stable_variant_offset(class_name, variant_idx)

            if clean_pool:
                base = fit_or_loop_to_duration(
                    clean_pool[variant_idx % len(clean_pool)], n_samples, variant_seed
                )
            else:
                base = synth_speech_like(duration_s, sr, variant_seed)

            variant = recipe(base, sr, variant_seed, variant_idx, noise_pool)

            class_dir = out_dir / variant.class_name
            class_dir.mkdir(parents=True, exist_ok=True)
            degraded_path = class_dir / f"{variant.variant_id}.wav"
            reference_path = class_dir / f"{variant.variant_id}.clean.wav"
            sf.write(str(degraded_path), variant.degraded, sr, subtype="PCM_16")
            sf.write(str(reference_path), variant.reference, sr, subtype="PCM_16")

            clip = {
                "id": f"syn-{variant.variant_id}",
                "class": variant.class_name,
                "path": str(degraded_path.relative_to(out_dir).as_posix()),
                "license": "CC0-1.0",
                "redistributable": True,
                "duration_s": round(len(variant.degraded) / sr, 3),
                "source": "synthetic: run.py synth" + (" (clean-dir input)" if clean_pool else " (synthesized speech-like signal)"),
                "synthetic": True,
                # Whether the *speech* under the degradation is a real recording or the
                # synth_speech_like proxy. The perceptual MOS gates (DNSMOS/PESQ/STOI) are
                # only calibrated on real speech — master-eval keys off this to gate them on
                # "real" fixtures and treat them as report-only trend lines on "synthetic"
                # ones (see this module's docstring; eval/run.py `_master_eval_one`).
                "speech_source": "real" if clean_pool else "synthetic",
                "ground_truth": {
                    "reference_wav": str(reference_path.relative_to(out_dir).as_posix()),
                    **{k: v for k, v in variant.meta.items() if k not in ("notes",)},
                },
                "notes": variant.meta.get("notes", ""),
            }
            clips.append(clip)

    manifest = {
        "$comment": (
            "Synthetic paired-degradation corpus from `python run.py synth`. Regenerate "
            "on demand; gitignored (eval/corpus/* pattern). Classes 10/11 need real "
            "sourced content and are never in here — see eval/synth.py module docstring."
        ),
        "version": 1,
        "synthetic": True,
        # Manifest-level default for clips that don't carry their own `speech_source`.
        # "real" when a --clean-dir pool of real recordings backs the speech; "synthetic"
        # when speech is the synth_speech_like proxy (DNSMOS/PESQ/STOI then report-only).
        "speech_source": "real" if clean_pool else "synthetic",
        "generated_by": "eval/run.py synth",
        "sample_rate": sr,
        "clips": clips,
    }
    manifest_path = out_dir / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    return manifest
