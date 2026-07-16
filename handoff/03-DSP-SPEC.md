# 03 — DSP & AI Processing Specification

The audio science. This is the most demanding work in the project — implement it with care. Every module ships with unit tests on synthetic signals + eval-corpus gates (06). Chain changes bump `chain_version`.

## 1. Analysis pass (pass 1)

Single streaming pass over decoded 48 kHz f32 audio. Emits `AnalysisReport` (JSON, stored in project; drives auto-decisions + Health Card + compliance report "before" columns).

| Measurement | Method | Used for |
|---|---|---|
| Integrated/short-term/momentary loudness, LRA | EBU R128 / BS.1770-4 via `ebur128` crate | normalization, leveler targets, report |
| True peak | 4× oversampled per BS.1770 | limiter, report |
| Noise floor estimate | 10th percentile of ST loudness on VAD-negative frames; spectral snapshot of noise segments | denoise strength auto, SNR, Health Card |
| SNR | speech-frame energy vs noise floor | Health Card, denoise auto |
| Clipping | consecutive full-scale samples + flat-top detection; count + worst region | de-clip trigger, Health Card |
| DC offset | per-channel mean | DC removal |
| Hum | spectral peaks at 50/60 Hz ± harmonics, tracked stability over time | de-hum trigger + fundamental choice |
| Reverb (RT60 estimate) | decay-rate fitting on speech offsets (Schroeder integration on gaps); coarse buckets: dry <0.3 s / ok / noticeable / bad >0.8 s | Studio-tier recommendation, Health Card |
| Bandwidth | spectral rolloff (detect 8/16 kHz-limited sources: Zoom, phone) | BWE recommendation, EQ bounds |
| VAD | Silero v5, 32 ms frames, hangover smoothing | everything speech-gated |
| Speech/music/other segmentation | classifier on 1 s windows + median filter; boundaries snapped to VAD | leveler music-mode, silence-cut protection, ducking |
| Sibilance | 5–9 kHz band energy ratio on speech frames | de-esser auto-threshold |
| Stereo | inter-channel correlation, width, dual-mono detection, L/R imbalance | downmix decisions, Health Card |
| Breaths/mouth clicks | VAD-boundary transient classifier (breath = broadband low-energy inhale pattern) | breath/de-click modules |
| Silence map | VAD-negative runs > 300 ms with levels | silence cutting |

Performance budget: analysis ≥ 20× realtime on 4-core CPU (it's mostly FFTs + tiny models).

## 2. Auto-decision logic ("one click" = this table)

`Master` maps AnalysisReport → chain config. Deterministic, unit-tested, every decision surfaced in the Health Card with plain-language rationale:

- SNR < 35 dB → denoise on; strength = clamp(map(SNR 35→0.3, 10→1.0)); SNR ≥ 35 dB → denoise light (0.2) unless "clean" (≥45 dB) → off.
- Hum detected & stable → de-hum at detected fundamental.
- RT60 > 0.5 s → recommend Studio tier (chip on Health Card); Standard still applies DFN (mild reverb reduction).
- Clipping regions > 3 → de-clip on (M4+; before that: warn).
- Bandwidth < 12 kHz → note "limited source" (BWE only in Studio tier; never fake-EQ brightness into hiss).
- DC |offset| > 0.001 → DC removal (always cheap-on anyway).
- Dual-mono → collapse to mono processing, output per preset.
- Loudness: always two-pass normalize to preset target; if input within ±1 LU and TP compliant → passthrough gain-only (report "already compliant").
- Music-majority file (>60% music frames) → leveler switches to music mode (gentler, see §4.7), denoise defaults lighter, silence-cut off.

## 3. Chain order (per track)

```
decode → resample 48k → DC/HPF → de-hum → de-click/de-clip → AI denoise/enhance
→ breath control → de-ess → AutoEQ → adaptive leveler → [multitrack: crossgate/ducking/mix]
→ two-pass loudness normalize → true-peak limiter → resample out → dither → encode
```

Rationale: repair before enhancement, enhancement before dynamics (leveler must not pump on noise), loudness math last so it's exact, limiter after gain so ceiling is guaranteed, dither at final bit-depth reduction only.

## 4. Module specs

Every module: `bypass`, latency declaration, f32 in/out, parameter struct with serde + defaults, `process(block)` + `flush()`. Defaults tuned on the corpus, not by vibes.

### 4.1 DC removal + high-pass
1st-order DC blocker + adaptive HPF: speech-only file → 80 Hz Butterworth 2nd-order (male-voice safe); music present → 40 Hz; analysis found rumble/plosive energy → up to 120 Hz on speech segments only. Param: `mode: auto|fixed(hz)|off`.

### 4.2 De-hum
Cascaded IIR notches at detected fundamental (50 or 60 Hz auto) + harmonics up to 1 kHz, Q auto from hum bandwidth, gain floor −40 dB, only engaged when analysis confirms (never notch blind). Re-check mid-file (generator hum drifts): track ±0.5 Hz. Param: `fundamental: auto|50|60`, `strength`.

### 4.3 De-click / de-crackle (M3) & De-clip (M4)
Transient detector (derivative outlier vs local stats) → AR-model interpolation over ≤2 ms gaps (mouth clicks: gentler threshold, speech-gated). De-clip: detect flat-tops → cubic/AR reconstruction, applied only to clipped regions, then peak-safe gain trim. Never engage on percussion (music-frame guard).

### 4.4 AI denoise / enhance (the heart)
Three tiers, one UI slider ("Repair strength") + tier selector:

- **Fast:** RNNoise (or GTCRN after bake-off) — realtime on anything, light artifacts acceptable, used for previews on weak machines and `--tier fast`.
- **Standard (default): DeepFilterNet3** via `df` crate. 48 kHz native, ~40 ms latency, ≥ realtime on 2 cores. Controls: max attenuation dB (default 100 → we expose 6..100 mapped from strength), post-filter beta (dry/wet). Handles stationary + dynamic noise and mild reverb.
- **Studio (M4):** heavy model, GPU-preferred, chunked offline (not realtime). Requirements: strong dereverb, bandwidth extension to 48 kHz from ≥8 kHz sources, and **separated Speech/Noise/Music gain sliders** (Adobe March-2026 parity, P5) — implies a separation-style model or multi-stem post-mix. **Bake-off task (M4): candidates MossFormer2-SE-48K + MossFormer2-SR-48K (Apache-2.0), resemble-enhance (MIT), VoiceFixer (MIT); criteria: DNSMOS uplift on corpus, artifact rate in blind listen, ONNX exportability, RTF on DirectML mid-GPU and CPU, license.** Chunk with 8 s windows / 1 s crossfaded overlap; verify no boundary artifacts (spectral discontinuity metric in 06).

Global rule: denoise strength is **capped by musicality** — music frames get ≤50% of speech-frame attenuation unless user overrides (protects beds/intros).

### 4.5 Breath control + mouth de-click
Breaths (classified in analysis): attenuate −6 dB default (range 0..−18, never hard-gate — hard-gated breathing sounds robotic), 30 ms ramps. Mouth de-click: see 4.3 gentle mode. Off in music mode.

### 4.6 De-esser
Split-band compressor keyed on 5–9 kHz/broadband ratio; threshold auto from analysis sibilance stats (target: sibilant frames pulled to ≤ +3 dB over speech average); ratio 3:1, attack 1 ms, release 60 ms, lookahead 5 ms. Speech-gated.

### 4.7 AutoEQ (M3)
Compute long-term average spectrum (LTAS) on speech frames → fit ≤8 biquads (bounded ±6 dB, Q ≤ 2) toward a target speech curve (broadcast-ish tilt; ship `neutral`, `warm`, `presence` targets). Never boost above source bandwidth rolloff. Per-speaker when diarization available (Voice Memory feature stores these curves per show). Param: `amount 0..1` scales all band gains.

### 4.8 Adaptive leveler (the crown jewel — most listening time goes here)
Two stages, speech-gated, music-aware:

1. **Slow AGC:** target = preset speech ST loudness (e.g., −18 LUFS ST so post-normalize integrated lands near target). Gain computed on 3 s windows of *speech-gated* ST loudness, interpolated log-domain, slew ≤ 2 dB/s, range ±12 dB (param `max_gain`). Music/other frames: gain frozen at boundary value then eased toward 0 dB correction (music keeps intentional dynamics; ducking handles beds separately). VAD-negative: hold last gain (never pump noise floor up — denoise runs first, but belt-and-suspenders).
2. **Fast tamer:** RMS compressor, 2:1 above target+6 dB, attack 5 ms, release 150 ms program-dependent, ≤ 6 dB reduction — catches laughs/shouts the slow stage misses.

Params: `dynamics preservation` (0 = broadcast-tight, 1 = off; default 0.35 scales both stages), `max_gain`. Per-speaker mode (M3+): with diarization, each speaker's median speech loudness is first normalized to the common target (fixes quiet-guest-loud-host better than any window-based AGC can).
**Quality gates:** no audible pumping on corpus class "music+speech" (blind check), gain curve exported in report, converges within 2 s at file start (pre-roll analysis warm-start — we have pass 1, so start at the right gain, not from 0 dB).

### 4.9 Loudness normalization (two-pass, exact)
Targets table (presets): Podcast stereo **−16 LUFS**, podcast mono −19, Spotify/YouTube −14, EBU R128 broadcast −23, ATSC A/85 −24, ACX audiobook (special: RMS −23..−18 dB + peak ≤ −3 dB + floor check), Custom (−30..−10). Method: measure post-chain integrated loudness (gated, BS.1770-4) on a metering pre-render of the chain output (cheap: chain is deterministic; reuse pass-1 where modules are bypassed) → apply static gain → limiter guarantees TP ceiling → **verify**: re-measure final output; if |error| > 0.5 LU (limiter ate into loud program), apply one correction iteration. Report in/out values.

### 4.10 True-peak limiter
Lookahead 5 ms, 4× oversampled TP detection (BS.1770), ceiling per preset (−1.0 dBTP default; −2.0 for lossy low-bitrate; −3.0 dB ACX), attack via 5 ms raised-cosine gain smoothing, release 80 ms program-dependent, ISP-safe. Hard guarantee: output TP ≤ ceiling on the entire corpus (CI-enforced, zero tolerance). Gain-reduction trace stored for report.

### 4.11 Dither
TPDF 1 LSB at final bit-depth reduction (24→16), seeded from content hash (determinism). Off for float/lossy outputs.

## 5. Silence & filler cutting (M3) — non-destructive

- **Silence:** VAD-negative runs ≥ `min_gap` (default 1.5 s) shortened to `target_gap` (default 0.7 s), 60 ms equal-power crossfades, never inside music segments, never cut below 0.4 s (unnatural), chapter-boundary silences protected. Output = cut-list (EDL), applied at render; UI shows strikethrough regions on waveform (04).
- **Fillers:** from ASR word timestamps: language-specific lexicon (en: um, uh, erm, hmm, "you know"/"like" only in `aggressive` mode), confidence ≥ 0.85, padded ±40 ms, crossfaded. Each instance is an accept/reject row in the review UI; bulk "apply safe set." Quality gates (06): precision ≥ 90% / recall ≥ 70% on labeled set; no audible artifacts at 20 random cut points per corpus file (spectral-discontinuity metric + listen).
- Breath-adjacent trims merge with neighboring cuts (no "gasp-cut" artifacts).

## 6. Multitrack (M4)

- Inputs: N speech tracks + M music/SFX tracks (user-tagged, auto-guessed from analysis).
- **Alignment:** GCC-PHAT cross-correlation on 60 s windows → constant offset (double-enders) or drift line (≤50 ppm resample repair for cheap recorders); confidence surfaced.
- **Crossgate (bleed control):** when track A is the dominant speaker and track B's signal is coherent with A (delayed/attenuated copy — spill), duck B by up to −15 dB (param). Coherence via normalized cross-correlation per 100 ms; never gate B's own speech onsets (VAD-B veto). This is Auphonic's multitrack magic (P11) — get it right.
- **Per-track chains:** each speech track runs §4 chain with per-speaker leveling; music tracks: light chain (HPF/loudness prep only).
- **Ducking:** music tracks duck under any speech VAD by `duck_db` (default −12, range −6..−24), lookahead 200 ms fade-down, 800 ms fade-up, hold 300 ms (no chatter between words).
- **Mixdown:** sum → master bus → §4.9/4.10. Solo/mute + per-track gain offsets in UI.

## 7. Quality tiers summary

| Tier | Modules | Target RTF (4-core CPU) | Use |
|---|---|---|---|
| Fast | RNNoise/GTCRN, leveler, loudness, limiter | ≥ 15× | weak machines, quick pass, previews |
| Standard (default) | full chain w/ DFN3 | ≥ 5× | the one-click path |
| Studio | Standard + heavy enhance/separation (+BWE, hard dereverb) | ≥ 1× GPU / ≥ 0.3× CPU (warn) | bad audio, Adobe-parity mode |

## 8. Edge cases (unit-test fixtures for each)

Silence-only file (no-op + friendly message, no div-by-zero on LUFS gating) · music-only (leveler music mode, no denoise butchery) · clipped-throughout · 8 kHz phone memo (no fake brightness) · dual-mono · mid-side/badly-phased stereo (correlation < −0.5 → warn, offer mono) · 8-hour file (streaming, progress, cancel) · sample-rate zoo (8/11.025/16/22.05/32/44.1/48/96/192 k) · variable-frame-rate video audio · corrupted/truncated file (graceful error) · 32-bit float input already > 0 dBFS · DC-only content · empty/0-byte file.

## 9. References for implementers

ITU-R BS.1770-4 (loudness/true-peak math), EBU R128 + Tech 3341/3342 (gating, LRA), AES streaming loudness recommendations (−16 LUFS convention), ACX submission requirements (RMS/peak/floor), DeepFilterNet paper + `df` crate docs, Silero VAD repo, whisper.cpp docs, sherpa-onnx diarization examples. Cite the specific clause in code comments where a constant comes from a standard.
