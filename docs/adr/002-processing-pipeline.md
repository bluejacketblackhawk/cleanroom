# ADR-002: Processing Pipeline

- Status: Accepted (from the build handoff)
- Date: 2026-07-13
- Source: handoff/02-ARCHITECTURE.md § Processing pipeline (ADR-002)

## Context

Audio mastering requires processing files ranging from 3 minutes to 3 hours with consistent quality across batch and interactive workflows. Memory must be bounded (< 1.5 GB peak) even on 3-hour files. Audio processing modules have latency; synchronization across analysis and rendering must be automatic and sample-exact. Preview and final render must use the same processing chain.

## Decision

- **Internal format: f32, 48 kHz** (DFN3 native rate), planar per channel (interleaved-free). Original sample rate/bit depth preserved in metadata; output resampled as requested (rubato, windowed-sinc).
- **Streaming, chunked execution:** Fixed hop of 480 samples (10 ms) grouped into blocks (4800 = 100 ms) flowing through a pull-graph. A 3-hour file never fully resides in RAM (budget: < 1.5 GB). Modules declare latency; the graph auto-compensates so A/B stays sample-aligned.
- **Two passes maximum:** Pass 1 = analysis (measurements, VAD, classifiers, loudness); Pass 2 = render with parameters frozen from Pass 1 (exact two-pass loudness normalization). Preview uses the same graph in realtime mode with Pass 1 data already cached.
- **Intermediate cache:** Per-module output cached to disk (`%LOCALAPPDATA%/anvil/cache`, LRU-capped) so toggling one module re-renders only downstream stages.

## Consequences

**Enables:** Memory-bounded processing of arbitrarily long files; exact A/B switching via automatic latency compensation; fast preview via cache reuse; deterministic batch processing (same input + settings + version ⇒ bit-identical)

**Constrains:**
- Enforce: Fixed block sizes and processing order. No data-dependent thread scheduling affecting the mix — parallelism only across independent tracks/files, never inside one stream's sample math (or use deterministic reduction).
- All modules must declare latency accurately and respect the pull-model: data flows downstream on request, not pushed upstream.
- The graph architecture is the load-bearing constraint for feature 3-hour-files; any deviation (streaming audio into Python/external processes without buffering, pulling full files into RAM for analysis) must be reviewed as a regression risk.
