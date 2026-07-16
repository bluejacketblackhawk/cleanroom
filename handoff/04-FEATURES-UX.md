# 04 — Features & UX Specification

Design language: calm, confident, dark-first (light theme too), waveform is the hero. Radix + Tailwind; one accent color; no gradients-and-glassmorphism kitsch. The app should feel like a tool made by audio people, not a SaaS dashboard.

**Interaction budget (hard rule):** default path = 3 interactions: (1) drop file, (2) click **Master**, (3) click **Export**. Everything else is optional disclosure.

## Screens

### S1. Home / Drop
Full-window drop target ("Drop audio or video — or a whole folder"), Open button, recent projects grid, subtle footer: *"Your audio never leaves this computer."* Dropping N>1 files or a folder → Batch (S4). First launch: bundled 90-second demo file card ("Try it on our terrible recording").

### S2. Production view (the main screen)
Layout: waveform center (before/after stacked or overlaid, toggle), transport bar (play/pause/seek, **A/B button — spacebar plays, `A` toggles original/processed instantly, sample-aligned**), right panel with tabs:

- **Master tab (default):** Health Card + big Master button + tier selector (Fast/Standard/Studio) + preset dropdown (target: Podcast −16 etc.). After processing: before/after loudness meters (integrated/LRA/TP), per-module chips with on/off toggles and strength sliders (progressive disclosure: chips collapsed to "what we did" summary until expanded). "Advanced" reveals full chain params (03 §4).
- **Transcript tab (M3):** transcribe button (model picker w/ size + "will download X MB" if absent), transcript with word-level playback-follow, speaker labels (editable names/colors), search, **filler/silence review list** (each cut: play-in-context button, accept/reject, bulk-apply-safe), export SRT/VTT/TXT/JSON.
- **Chapters & Metadata tab (M2):** chapter list (time, title, URL, image) — add at playhead; AI suggest (M4, needs LLM pack); metadata form (title/artist/album/year/genre/cover art drag-drop); applies to all exports.
- **Export tab:** simultaneous outputs list (each: format+bitrate+mono/stereo+destination), add-output button, presets save/load, Export All + per-file progress, "Open folder" on done, compliance report checkbox (HTML/PDF).

Health Card spec (feature #3): 3–6 rows, icon + plain sentence + optional "fix" chip, numbers on hover. Example rows: "Loudness is −23.4 LUFS — quiet for podcasts. Master will bring it to −16." / "Steady 60 Hz hum found — will remove." / "Room echo is noticeable — Standard will help; Studio tier will fix most of it *(chip: Switch to Studio)*." Never show a number without a word; never show jargon without a tooltip.

### S3. Multitrack production (M4)
Track lanes (waveforms, solo/mute/gain, speaker/music tag), alignment banner ("Tracks aligned — offset 2.34 s, drift corrected" + Undo), ducking controls on music tracks, same right-panel tabs operating on the mix.

### S4. Batch queue (M2)
Table: file, preset, status, progress, result links; queue controls (pause/reorder/remove); concurrency auto (N-1 cores); summary toast + notification on completion; failures grouped with one-click "retry failed"; per-batch report export. Back-catalog mode = this + folder drop + "preserve folder structure" output option.

### S5. Watch folders (M2)
List of watch rules: folder → preset → output destination → file-pattern filter → on/off. Runs in tray agent (autostart optional, default off, one-click enable). Tray: icon states (idle/working/error), menu (open app, pause watching, recent results). New-file stability check (size unchanged 5 s) before processing; never re-process outputs (marker sidecar or output-dir exclusion).

### S6. Presets manager (M2)
Cards: name, target, tier, chain deltas from default, outputs. Duplicate/edit/delete/import/export (.anvilpreset JSON). Shipped presets: `Podcast (Stereo −16)`, `Podcast (Mono −19)`, `Spotify/YouTube −14`, `Broadcast EBU −23`, `Audiobook (ACX)`, `Voice memo cleanup`, `Music-heavy show`.

### S7. Models manager (M2+)
Per pack: name, what it enables (plain words: "Transcription — turns speech into text"), size, installed/available, license line + attribution where required, download w/ progress+resume, delete, "verify files" (re-hash). Total-disk-used line. Offline note: "Everything already installed keeps working without internet."

### S8. Settings
General (theme, language), Processing (default tier/preset, GPU on/off + detected device line, cache size/location + clear), Folders (default output pattern `{name}_mastered.{ext}`, output naming tokens), Integration (Explorer context menu on/off, file associations, autostart tray), Updates (check now, auto-check toggle, channel; "the app never sends your audio anywhere — updates only download from GitHub"), About (version, chain version, licenses/attributions screen, diagnostics export = zip of logs+system info for GitHub issues).

### S9. Onboarding (first run, 3 cards max, skippable)
1) "Drop a file, press Master — that's the whole app." 2) "Everything runs on this computer. Airplane mode? Still works." 3) "More power when you want it: transcripts, batch, watch folders." → offers demo file.

### S10. Recording Guard (M4, tray/window)
Live input meter with: level headroom guidance ("a bit hot — lower gain until the bar stays green while you speak loudly"), noise floor readout with plain rating, echo estimate (clap test button → RT60), sample-rate/device display. One screen, zero persistence — it's a pre-flight check, not a recorder.

## Clip Studio (M4, feature P19)
Select transcript text or waveform range → clip editor: aspect (1:1/9:16/16:9), caption style (3 templates, word-highlight karaoke from whisper timestamps), waveform/color/cover-art background, title text → render MP4 (H.264 via OS encoder if available, else ffmpeg LGPL-safe route — see 07 §video-encode) with mastered audio. Target: a shareable clip in < 60 s of user effort.

## CLI acceptance (M2)
Every S2/S4 capability scriptable headless (02 §CLI). `--json` machine output; exit codes: 0 ok, 2 bad input, 3 preset/model missing, 4 cancelled, 5 internal. `anvil batch --watch` = S5 without GUI. Docs page with copy-paste recipes (NAS cron, OBS post-record hook).

## Microcopy rules
Plain words first, numbers on hover/expand. Never blame the user. Never "error" without a next step. Processing verbs in progress ("Removing hum… Balancing voices… Setting loudness…") — they double as education. British-neutral en-US, no exclamation marks in errors, at most one in success toasts.

## Keyboard map (S2)
Space play/pause · A toggle A/B · M master · E export · ←/→ seek 5 s (Shift = 30 s) · ↑/↓ zoom waveform · [ ] prev/next cut or chapter · Del reject selected cut · Enter accept selected cut · Ctrl+Z/Y undo/redo (cut-list + metadata ops).

## Accessibility & i18n
WCAG AA contrast both themes; full keyboard traversal; screen-reader labels on all controls (waveform exposes text alternative: duration, loudness, cut count); reduced-motion honors OS; font scaling to 150% without clipping. All strings through an i18n layer from M0 (en only shipped at 1.0; de/es/fr/pt-BR post-launch — German podcast market is Auphonic's home turf).

## Error & empty states (write them, don't improvise)
Unsupported/corrupt file (say what it is, link supported list) · model pack missing (inline install button with size) · GPU init failed (auto-fell-back-to-CPU notice, not an error) · disk full mid-render (pause + free-space prompt, resume) · watch folder unreachable (rule paused badge) · ffmpeg sidecar missing/hash-mismatch (reinstall prompt) · file in use/locked (retry) · 0 speech detected ("this sounds like music — Master will use the music profile").

## Per-feature acceptance criteria (gates for 05)

- **One-click Master:** demo file → mastered export in 3 interactions; Health Card shows ≥2 correct findings on corpus fixtures; cancel works at any stage; second Master run with tweaked module = only downstream stages recompute (cache hit visible in logs).
- **A/B:** toggle latency < 50 ms, sample-aligned (null test: A/B rapid toggling produces no position jump; automated test compares playhead continuity).
- **Batch:** 100-file soak, zero crashes, failures isolated per-file, machine sleep/resume mid-batch survives.
- **Watch folders:** file dropped while agent running → output within RTF budget + notification; agent survives logoff/logon; no double-processing on rename storms (stability check test).
- **Transcript:** 1-h episode, small model, follows playback within one word; edits persist in project; SRT timestamps pass round-trip validation.
- **Cut review:** accept/reject 50 cuts → render matches cut-list exactly (EDL golden test); undo restores.
- **Chapters/metadata:** round-trip test — import tagged MP3/M4A, export, diff tags: nothing lost, edits applied (ID3v2.3+2.4, MP4, Vorbis).
- **Compliance report:** HTML+PDF open cleanly, values match `anvil analyze` of the rendered file within tolerance (self-consistency test in CI).
- **Clip Studio:** 30 s clip renders < 2 min on 4-core CPU; captions within ±1 frame of word timestamps.
- **Shell integration:** right-click WAV in Explorer → mastered file appears + toast; uninstall removes menu cleanly.
