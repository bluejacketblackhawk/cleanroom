# 01 — Product Brief

## Vision

Every podcaster deserves broadcast-quality audio without a subscription, an upload, or an audio-engineering degree. Cleanroom is a free, MIT-licensed desktop app for Windows and macOS that does what Auphonic and Adobe Podcast do — and more — entirely on the user's machine.

## The wedge

**100% local, offline, one-click "Master my audio"** = loudness normalization + AI denoise + adaptive leveling.

Why this wins:

| Axis | Auphonic | Adobe Podcast | Cleanroom |
|---|---|---|---|
| Price | $11–89/mo (2 free h/mo) | $9.99/mo Premium (free: 1 h/day, 30-min files) | **Free forever, MIT** |
| Where audio goes | Their servers | Adobe's servers | **Never leaves the machine** |
| Hours/limits | Credit-metered | Daily caps, file-size caps | **Unlimited** |
| Speed | Upload + queue + download | Upload + queue + download | **~5× realtime locally, no upload** |
| Privacy (journalism, legal, medical, enterprise, unreleased content) | Trust policy | Trust policy | **Structurally private** |
| Preview | Re-process round-trip | Re-process round-trip | **Instant A/B, per-module toggles** |
| Back catalog (500 episodes) | Hundreds of $ in credits | Impractical | **Overnight batch, $0** |

Killer demo (build the launch video around it): turn Wi-Fi off on camera → drop a 2-hour noisy episode → press Master → play before/after → export at −16 LUFS. Nothing uploaded, nothing paid.

## Personas

1. **Hobby podcaster (primary).** USB mic, untreated room, edits in Audacity/GarageBand. Doesn't know what LUFS is; knows their audio "sounds worse than the big shows." Needs: the one-click path, plain-language feedback.
2. **Indie professional / editor-for-hire.** Produces 4–10 shows. Needs: presets per show, batch, watch folders, per-speaker consistency, loudness compliance report to send clients, CLI.
3. **Privacy-bound recorder.** Journalist, therapist-podcaster, corporate internal comms, lawyer depositions, unreleased-content studios. Needs: the offline guarantee, and documentation asserting it.
4. **Audiobook narrator.** Needs: ACX/Audible compliance checking + auto-conform, M4B chapters export.

## Competitor teardown

Verified July 2026 (re-verify at launch; sources at bottom):

**Auphonic** — the feature ceiling to match. Cloud web service. Free 2 h/mo; S $11/mo (9 h), M $23/mo (21 h), L $45/mo (45 h), XL $89/mo (100 h), annual pricing. Algorithms: Adaptive Leveler, loudness normalization (target presets incl. −16 LUFS podcast), two AI denoisers (Static incl. reverb/hum; Dynamic "everything but voice and music"), separate noise/reverb/breath amounts, adaptive high-pass filtering, AutoEQ (spectral balance + de-ess), multitrack (per-track processing, crossgate, ducking, mixdown), silence + filler-word cutting (multilingual), Whisper transcription with shareable transcript editor + speaker ID, AI shownotes/chapters/title suggestions, audiogram video export, many simultaneous output formats with full metadata/chapters/cover, publishing integrations (YouTube, hosts, S3, FTP, WebDAV…), watch folders (via cloud storage), REST API, presets/batch.

**Adobe Podcast** — the enhancement-quality ceiling to match. Web app. Free: 1 h/day, 30-min/500 MB files, no strength control. Premium $9.99/mo: 4 h/day, 2-h/1 GB files, batch, video, strength slider. Enhance Speech v2 does denoise + dereverb + "studio voice" (bandwidth extension / mic modeling). **March 2026 update: independent Speech / Noise / Music sliders** (can strip music while keeping voice, or keep ambience). Mic Check (analyzes mic setup before recording). Studio (browser recorder/editor with filler-word detection).

**Secondary (steal ideas, don't chase):**
- *iZotope RX* ($399+): repair suite — de-click, de-clip, de-breath, mouth de-click, spectral repair. We take: de-clip, mouth de-click, breath control as auto modules.
- *Descript / Studio Sound*: transcript-based editing. We take: transcript-driven filler/silence cutting UI (not a full editor).
- *Hindenburg*: voice profiler, auto-levels. We take: per-speaker voice memory.
- *Cleanvoice*: filler/mouth-sound/dead-air removal. We take: its review-before-apply UX.
- *Krisp / NVIDIA Broadcast*: real-time denoise. We take: nothing for v1 (we're post-production; real-time monitoring is a differentiator idea below). NVIDIA Broadcast is Windows+NVIDIA-only — we must run on anything.
- *Buzzsprout "Magic Mastering"*: Auphonic white-label; validates that hosts charge $6–12/mo for exactly our wedge.

## Feature parity matrix

Every row lands in a milestone (M#, see 05) or is explicitly deferred with reason. ✦ = we exceed the competitor.

| # | Feature (competitor) | Cleanroom | Milestone |
|---|---|---|---|
| P1 | Loudness normalization, target presets, true-peak limit (Auphonic) | Two-pass EBU R128 + presets + TP limiter ✦ (bit-accurate, verifiable report) | M1 |
| P2 | Adaptive Leveler (Auphonic) | Speech-gated adaptive leveler, music-aware, dynamics-preservation control | M1 |
| P3 | AI noise reduction, strength control (both) | DeepFilterNet3 Standard tier + Fast tier; dry/wet + max-attenuation controls | M1 |
| P4 | Reverb reduction (both) | In Standard (DFN handles mild) + Studio tier (heavy dereverb) | M1/M4 |
| P5 | Speech/Noise/Music independent sliders (Adobe 2026) | Studio tier separation controls | M4 |
| P6 | "Studio voice" enhancement / bandwidth extension (Adobe) | Studio tier enhance model (GPU-preferred) | M4 |
| P7 | De-hum 50/60 Hz + harmonics (Auphonic) | Auto-detected notch bank | M3 |
| P8 | Adaptive high-pass / filtering (Auphonic) | Analysis-driven HPF | M1 |
| P9 | AutoEQ + de-ess (Auphonic) | LTAS-matched corrective EQ + split-band de-esser | M3 |
| P10 | Breath reduction amount (Auphonic) | Breath attenuation (not dumb gating) + mouth de-click ✦ | M3 |
| P11 | Multitrack: per-track chains, crossgate, ducking, mixdown (Auphonic) | Same, plus double-ender auto-alignment ✦ | M4 |
| P12 | Silence cutting (Auphonic) | VAD-based, music-protected, crossfaded, non-destructive review UI ✦ | M3 |
| P13 | Filler-word cutting, multilingual (Auphonic, Adobe Studio) | ASR-timestamped, per-instance review UI ✦ | M3 |
| P14 | Transcription (Whisper) + editor + speaker ID (Auphonic) | whisper.cpp local, transcript editor, diarization | M3 |
| P15 | AI shownotes/chapters/titles (Auphonic) | Local LLM (optional download) — private ✦ | M4 |
| P16 | Output formats: MP3/AAC/Opus/Vorbis/FLAC/ALAC/WAV, mono/stereo, multi-output (Auphonic) | Same set, simultaneous outputs | M2 |
| P17 | Metadata, chapters (ID3/MP4/Vorbis), cover art (Auphonic) | Full editor + M4B audiobook ✦ | M2 |
| P18 | Video files: process audio, keep video (both) | Demux → process → remux, no video re-encode | M2 |
| P19 | Audiogram/waveform video (Auphonic) | Clip Studio: captioned clips 1:1/9:16/16:9 ✦ (local captions) | M4 |
| P20 | Presets / batch productions (Auphonic) | Presets + unlimited batch queue ✦ | M2 |
| P21 | Watch folders (Auphonic via cloud) | True local watch folders + tray agent ✦ | M2 |
| P22 | REST API / CLI (Auphonic) | Full CLI (headless); local REST daemon post-1.0 | M2/roadmap |
| P23 | Audio stats: LUFS in/out, LRA, TP, SNR, music/speech segments (Auphonic) | Analysis report + exportable HTML/PDF compliance report ✦ | M1/M2 |
| P24 | Mic Check (Adobe) | Recording Guard: live input meter + room/noise diagnosis before recording | M4 |
| P25 | Publishing integrations (Auphonic) | **Deferred post-1.0** — conflicts with offline-first wedge; exports to local files/folders; optional host uploads later | roadmap |
| P26 | Browser recorder/editor (Adobe Studio) | **Out of scope** — we are post-production, not a DAW (see Non-goals) | — |

## New features (differentiators — "come up with new ones")

Priority: ★★★ = wedge-adjacent, in v1.0; ★★ = v1.0 if lanes stay green; ★ = post-1.0 roadmap.

1. ★★★ **Instant A/B everywhere.** Toggle processed/original at any playhead position with per-module bypass, sample-aligned. Cloud tools structurally cannot do this.
2. ★★★ **Back-Catalog Re-Master.** Point at a folder of 500 episodes → consistent loudness + denoise overnight, $0. THE demo for switchers. (Batch + preset + report, M2.)
3. ★★★ **Podcast Health Score.** Plain-language diagnosis card after analysis: "Room echo: noticeable (RT60 ≈ 0.9 s) — Studio tier will fix most of it. Hum at 60 Hz: yes. Loudness: −23.1 LUFS → will raise to −16." Numbers available on hover; words by default. (M1)
4. ★★★ **Compliance Report export.** Per-episode HTML/PDF: before/after LUFS/TP/LRA/SNR graphs, processing applied, pass/fail vs chosen target. Freelancers attach it to invoices. (M2)
5. ★★★ **ACX/Audible mode.** One preset + report: RMS −23..−18 dB, peak ≤ −3 dB, floor ≤ −60 dB RMS, auto-conform + M4B export. Whole audiobook-narrator niche unserved by Adobe/Auphonic presets. (M2 preset, M4 full check)
6. ★★ **Voice Memory.** Per-speaker EQ/level profile learned per show; episodes 2..N converge to the same sound. (M4)
7. ★★ **Recording Guard.** Live mic monitor (tray): clipping, noise floor, echo warning *before* you record an hour of garbage. Local answer to Mic Check. (M4)
8. ★★ **Double-ender sync.** Cross-correlation alignment of separately recorded tracks (remote interviews). (M4)
9. ★★ **De-clip rescue.** Recover clipped recordings — the "my best take is ruined" saver. (M4)
10. ★★ **Shell integration.** Right-click any audio file in Explorer/Finder → "Master with Cleanroom" → mastered copy appears alongside. (M2 Win, M6 Mac)
11. ★★ **Sound-version pinning.** Projects record chain+model versions; re-renders are bit-identical forever; "sound updates" are opt-in per show. Mid-season consistency nobody else guarantees. (M1 architecture)
12. ★ **Local REST daemon** (`anvild`) for NAS/automation folks. ★ **Linux build** (Tauri makes it cheap; huge OSS goodwill). ★ **Episode assembly** (intro/outro/bed insertion with auto-ducking from a template). ★ **Translation/dubbing.** All post-1.0.

## Non-goals for v1.0 (guard the scope)

No recording/DAW/multitrack *editing* (we cut silence/fillers, we don't arrange), no cloud sync/accounts, no mobile, no live streaming, no VST hosting or plugin version, no publishing integrations, no video *enhancement* (audio-in-video only), no Linux (v1.x), no translation. Each is a roadmap line in the README, not a v1 lane.

## Naming

Codename **Cleanroom** everywhere internally. Final name = owner decision before M7 (candidates + trademark screen task in 07). Hard constraints: not "Speechify" (existing TTS company), nothing confusable with Sound Forge/Adobe/Auphonic; must have a free GitHub org name; prefer a name that signals local/private/mastering.

## Launch plan (M7, OSS motion — no ad budget)

- GitHub repo as the landing page: hero GIF (the airplane-mode demo), 60-second demo video, screenshots, honest benchmark table vs Auphonic/Adobe on the public corpus, `winget install` / `brew install --cask` one-liners.
- Docs on GitHub Pages (mkdocs-material): quickstart, "what Master does to your audio" explainer, CLI reference, FAQ ("is it really offline?" → yes, here's how to verify).
- Distribution: GitHub Releases (NSIS installer + portable zip, DMG), winget manifest, Homebrew cask, Scoop bucket.
- Announcement: Show HN ("Show HN: I built a free, offline Auphonic alternative"), r/podcasting + r/audioengineering + r/selfhosted, Product Hunt, podcast-editor communities/newsletters, awesome-selfhosted PR.
- Community scaffolding from M0: CONTRIBUTING.md, issue/PR templates, good-first-issue labels, GitHub Discussions on, roadmap board public.
- Never say "Adobe/Auphonic killer" in public copy; say "free, private alternative."

## Sources (verify again at launch)

- Auphonic features/pricing: https://auphonic.com/features · https://auphonic.com/help/algorithms/singletrack.html · https://www.creatorstackclub.com/software/auphonic
- Adobe Podcast plans/features: https://podcast.adobe.com/en/plans · https://podcast.adobe.com/en/features · https://thepodcastconsultant.com/blog/adobe-podcast-enhance
- DeepFilterNet dual MIT/Apache incl. weights: https://github.com/Rikorose/DeepFilterNet
