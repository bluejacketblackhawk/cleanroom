# ADR-005: Media IO

- Status: Accepted (from the build handoff)
- Date: 2026-07-13
- Source: handoff/02-ARCHITECTURE.md § Media IO (ADR-005)

## Context

ANVIL processes audio from diverse sources (podcasts, voiceovers, video) in many formats (WAV, FLAC, MP3, AAC, OGG, ALAC, MP4, MOV, MKV, WebM). Some formats require specific decoders (mp3 = libmpg123, aac = external). Encoding must produce artifact-free output while avoiding patent/licensing complexity. Metadata and chapters must round-trip without loss. Video processing must preserve video tracks during audio mastering.

## Decision

- **Decode:** symphonia (MPL-2.0, fine with MIT app; file-level copyleft — don't modify, or upstream mods) for wav/flac/mp3/aac/ogg/alac. **ffmpeg sidecar fallback** for everything else and all video containers.
- **Encode:** ffmpeg sidecar for MP3 (LAME), Opus, Vorbis, FLAC. **AAC via OS encoders** — Media Foundation (Win) / AudioToolbox (Mac) — sidesteps ffmpeg-AAC quality and patent-license questions; ffmpeg native AAC as last-resort fallback.
- **ffmpeg build: LGPL-only** (no GPL components, no libfdk), run as a **separate process** (sidecar), never linked. Compliance = ship license text + exact build source/offer. Parse `-progress pipe:1` for job progress. Pin the build; hash-check at startup.
- **Video:** demux audio → process → **remux with `-c:v copy`** (never re-encode video). Container support target: mp4/mov/mkv/webm.
- **Metadata/chapters:** lofty (MIT/Apache) for ID3v2.3/2.4 (incl. CHAP), Vorbis comments, MP4 atoms. ffmpeg `-map_metadata`/ffmetadata for MP4 chapter atoms + M4B. Cover art read/write. **Round-trip rule: never drop tags the user didn't touch.**

## Consequences

**Enables:** Universal format support without bundling GPL; OS-native AAC encoding (quality + licensing clarity); seamless video podcast support; metadata preservation across edit workflows; small standalone ffmpeg footprint

**Constrains:**
- Enforce: ffmpeg as sidecar only, never linked. Source offer (fork + pointer) must be included. LGPL-only build (no GPL, no libfdk).
- symphonia is MPL-2.0 (file-level copyleft): acceptable in MIT app if unmodified; any changes must be upstreamed or result stays MIT-incompatible.
- Metadata: ID3v2.4 and MP4 atoms preferred for modern tags. ID3v2.3 supported for legacy. CHAP frames preserved for chapters. Never drop user's tags during edit/re-encode cycles.
- Video remux must use `-c:v copy` (no re-encoding). Video output format decision (mp4 vs mkv) becomes a UI choice, not automatic.
