# ADR-008: Project & Data Model

- Status: Accepted (from the build handoff)
- Date: 2026-07-13
- Source: handoff/02-ARCHITECTURE.md § Project & data model

## Context

Users work on multi-file shows, apply edits (cuts, crossfades, metadata), and revisit projects weeks later. Projects must be portable, human-readable where possible, and versionable (to survive schema migrations). Intermediate analysis (VAD/loudness) and waveform peaks should be cached for fast reopening.

## Decision

- **`.anvilproj` = a folder** containing:
  - `project.json` (schema-versioned): metadata, source file references + content hashes, render settings, output manifest
  - `analysis/` cached analysis (VAD labels, loudness measurements, classifiers per source file)
  - `cache/` intermediate processing output (module results), LRU-capped on disk
  - `cuts.json` (EDL format): edit list (cut/crossfade/silence fill specifications)
  - `chapters/` chapter metadata and show notes (JSON + markdown)
  - `.anvilpeaks` binary pyramid file (min/max peaks at octave zoom levels), memory-mapped for instant waveform at any zoom

- **Presets = JSON documents** (chain params + target loudness + output formats), stored in user dir + shipped defaults. Importable/exportable (shareable files → community).

- **Settings:** `%APPDATA%/anvil/settings.json` (Win) / `~/Library/Application Support/anvil/settings.json` (Mac). User preferences, recent projects, plugin/model paths.

- **Autosave:** every 30 s + on close. Crash-safe via write-temp-then-rename (atomic on all OSes).

## Consequences

**Enables:** Portable project folders (USB stick, shared folder, cloud sync); versionable schema with migrations; fast reopens (cached analysis + peaks); offline-sharable presets; crash recovery via temp-file pattern

**Constrains:**
- Enforce: source file content hashes in `project.json` — if a source file is modified externally, UI detects staleness and re-analyzes.
- `.anvilpeaks` is binary and auto-generated; never ship it in templates, only build on first import.
- `project.json` schema version in every file; code must support at least N-1 schema version (one major release backward-compat).
- Presets are JSON and importable from any `.json` file user provides; validation must be strict (bad presets fail loudly, don't crash render).
- Cache is best-effort; missing cache triggers re-compute, not error.
