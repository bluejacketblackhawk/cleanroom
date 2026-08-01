# 09 — Cut-Word Split (feature spec)

**One-liner:** Record many short scripts in one continuous take (teleprompter workflow), saying a chosen **cut word** between scripts. Cleanroom finds every instance of that word, deletes it (and the dead air around it), splits the recording at each one, and exports each piece as its own **mastered** file. 10 scripts + the cut word between them → 10 finished videos (or audio files), cut word gone entirely.

Owner's framing, 2026-08-01: "I give you a 'cut word' and the file cuts both before and after the word, deleting it entirely, and separating the chosen video file (or audio, it should work for both) into two new files… 10 scripts with 10 cut words → 10 separate video files… picking the cutword would be cool too in settings."

Nobody else has this as a local one-step (Descript-style tools need manual scene work; this is drop → N mastered shorts). It composes three things that already exist in this repo: ASR word timestamps (`anvil-asr`), the non-destructive cut engine (`anvil-cut`), and the LGPL-safe video encode path (`anvil-media`). No new models, no new sidecars.

---

## 1. User experience

### Production view — new **Split** tab (right panel, next to Transcript)

1. **Cut word field**, pre-filled from Settings → `Default cut word` (§6). Helper text: *"Pick a word you'd never say in a script — 'kumquat', 'flamingo', 'checkpoint'. Multi-word phrases work too."*
2. **Detect** button → runs transcription if the project has none (existing transcript reused; existing model-pack prompt flow if no ASR model installed), then shows:
   - Waveform markers at every match (reuse `WaveformCutOverlay` pattern — distinct marker glyph, scissors).
   - A review list (reuse `CutReviewList` row pattern): matched text, timestamp, confidence, ▶ play-in-context (±2 s), accept/reject toggle. Fuzzy matches (§2) arrive **unaccepted**, flagged "possible match".
   - Segment summary line: *"4 cuts accepted → **5 segments** (0:00–2:11, 2:14–4:03, …)"* — live-updates with toggles. Segments shorter than 3 s get a ⚠ badge (likely false positive or doubled cut word).
   - **Add split at playhead** button — manual boundary for the one the ASR missed (row labeled "manual", always accepted, removes nothing but the surrounding silence per §3).
   - Optional **"I read N scripts"** number box → mismatch shows a warning banner, never blocks.
3. **Split & Export** button → per-segment progress (existing job system), then a results list with per-file open/reveal buttons + a summary toast. Each segment runs the **full Master chain independently** by default (§4); toggle "Master each segment" off for raw splits.
4. Export settings inline: output folder (default `{basename}_segments/` beside the source), naming template (§5), preset (video sources default to `spotify_youtube` −14 LUFS — shorts platforms; audio sources default to the project's preset), tier.

Interaction budget: with a default cut word saved in Settings, the whole flow is **Drop → Split tab → Detect → Split & Export** — and §7's batch/watch integration gets it to zero clicks.

### Settings screen additions
Under a new "Splitting" group: **Default cut word** (text, the owner-requested setting), **default naming template**, **Master each segment** (default on). Stored in `anvil_project::Settings` (§6).

### CLI (same feature, headless — QA harness drives this)

```
anvil split in.mp4 --cut-word kumquat [--expected 10] [--out-dir out/]
      [--preset spotify_youtube] [--tier standard] [--no-master]
      [--name-template "{nn} {first_words}"] [--model small] [--fuzzy|--exact]
      [--min-confidence 0.6] [--json]
```

`--cut-word` falls back to Settings' default; missing both → `EXIT_BAD_INPUT (2)` with a hint. `--expected` mismatch → warning on stderr, exit still 0 (JSON carries `expected_mismatch: true`). JSON output: `{ cut_word, matches: [{text, start, end, confidence, accepted}], segments: [{index, title, path, start, end, duration, lufs_in, lufs_out}] }`. New `Command::Split` variant in `crates/anvil-cli/src/main.rs`, exit codes unchanged (0/2/3/4/5).

## 2. Detection (in `anvil-cut`, new `split.rs` module)

Input: `anvil_asr::Transcript.words` (word-level timestamps + confidence exist today) + the cut phrase.

**As built 2026-08-01 (refined by measurement on the spoken TTS fixture — whisper-base heard a spoken "kumquat" as "Comquad" at 0.4 confidence, which the original spec would have left as a review-only row):**

- **Normalization:** casefold, strip attached punctuation (`Word.text` "may carry casing/punctuation" per its docs). Multi-word phrases match consecutive words with ≤ 0.5 s inter-word gap.
- **Three match tiers** (fuzzy tiers gated to tokens ≥ 5 chars; `--exact` disables both non-exact tiers): exact normalized equality · Levenshtein ≤ 1 ("cumquat") · **phonetic consonant-skeleton equality** (`kumquat` and `Comquad` both fold to `kmkt`) — ASR mangles unusual words far past edit distance 1, and the cut word is *deliberately* unusual.
- **Split-token merge:** a single-token phrase also matches two consecutive tokens joined (gap ≤ 0.2 s, both halves ≥ 2 chars — never swallows a real neighboring word), because whisper writes "kum quat".
- **Acceptance = isolation first:** a match standing **alone between silence runs** (a run ends within 0.35 s before it, another starts within 0.35 s after) is auto-accepted regardless of confidence or spelling — that's the acoustic signature of a cut word said between scripts, and whisper is *expected* to be unsure about a made-up-sounding word. An embedded match auto-accepts only when exact and ≥ `min_confidence` (0.6). Everything else is an unaccepted "possible match" review row.
- **Hallucination guard:** a match lying entirely inside one VAD-negative run is never auto-accepted — whisper's classic failure is inventing words inside long silences, exactly where cut words live next to.
- Deterministic: same transcript + options ⇒ identical matches (same guarantee `plan()` documents).

API sketch (mirrors the crate's existing style):

```rust
pub struct SplitOptions { pub phrase: String, pub fuzzy: bool, pub min_confidence: f32,
                          pub speech_pad_pre: f64 /*0.15*/, pub speech_pad_post: f64 /*0.30*/ }
pub fn detect_cut_words(t: &Transcript, silence: &SilenceInput, o: &SplitOptions) -> Vec<Cut>
pub fn partition(plan: &CutPlan, o: &SplitOptions) -> SegmentPlan
pub struct SegmentPlan { pub parts: Vec<Part> }        // Part { index, title_words, edl: Edl, start, end }
```

`detect_cut_words` emits `Cut { kind: CutKind::CutWord, label: matched_text, accepted, .. }`. **`CutKind::CutWord` is a new additive variant** (wire string `"cut_word"` — snake_case per the existing serde contract; additive enum variants keep old project files deserializing; the contract note in `anvil-cut`'s docs about not reshaping lightly is satisfied — add a round-trip test like the existing ones).

## 3. Boundary geometry (what exactly gets deleted)

The cut word is never cut tight — the owner wants it "gone entirely" and segments that start clean:

```
…script A end]  [silence]  [KUMQUAT]  [silence/breath]  [script B start…
             ▲──────────── removed region ─────────────▲
        A ends at last-word end            B starts at first-word start
          + post-roll 0.30 s                  − pre-roll 0.15 s
```

- Removed region = the matched word span **extended across every abutting VAD-negative run** (from `SilenceInput.runs` — same data the silence cutter already consumes), then backed off by `speech_pad_post` (0.30 s of natural room tone kept at each segment's tail) and `speech_pad_pre` (0.15 s kept before each segment's first word).
- Segment audio edges get a 15 ms fade-in/out in the split render path (part edges are hard starts, unlike the crossfaded *joins* `apply_with_crossfade` handles today).
- `partition` walks accepted `CutWord` cuts in timeline order → parts are the spans between them. Silence/filler cuts *inside* a part survive into that part's `Edl` (splitting composes with the existing cutting features — a segment can also get its ums removed). Empty/degenerate parts (< 0.5 s of kept speech — leading noise before script 1, trailing junk after a final cut word, doubled cut words) are dropped, so "say it after every script including the last" and "only between scripts" both yield exactly N segments.

## 4. Per-part processing

Analysis + transcription run **once, on the whole file** (cheap, needed for detection anyway). Then each part independently: part EDL → `anvil_cut::apply` render → **full Master chain** (existing pipeline) → **its own two-pass loudness normalize + TP limit** to the chosen preset. Each short publishes standalone, so per-part normalization is correct — never normalize the whole take then slice. `--no-master`: skip the chain, still normalize nothing, straight encode (splitting as a pure utility). Deterministic per part (existing guarantees apply per render).

## 5. Outputs & naming

- Default folder `{basename}_segments/`; collision-safe (` (2)` suffix, never overwrite silently).
- Template tokens: `{n}` / `{nn}` (1-based, zero-padded), `{name}` (source basename), `{first_words}` (first ~4 transcribed words of the part, slugified — segments come out *identifiable*: `03 why nobody holds the door.mp4`), `{date}`. Default: `{nn} {first_words}`.
- Metadata: title = rendered template, track = n/N (existing `anvil-media::metadata` path); video containers keep source rotation/colorspace metadata.
- Report hook: split summary appended to the compliance report when `--report`/UI report is on (per-part loudness table).

## 6. Code integration map (all seams verified in-repo)

| Where | Change |
|---|---|
| `crates/anvil-cut/src/split.rs` (new) + `lib.rs` | §2/§3: detection, `CutKind::CutWord`, `partition`, part-edge fades; golden tests like `edl_golden_kept_segments` |
| `crates/anvil-project/src/lib.rs` (`Settings`, line ~71) | add `default_cut_word: Option<String>`, `split_name_template: String` (default `"{nn} {first_words}"`), `split_master_default: bool` (true) — serde-default'd so existing settings files load |
| `crates/anvil-media/src/video.rs` | §video below: `export_video_segment(sidecar, video_in, part, mastered_audio, out, quality)` — one ffmpeg invocation per part, modeled on `remux_with_audio_spec` |
| `crates/anvil-cli/src/main.rs` | `Command::Split` (§1 CLI) |
| `apps/desktop/src-tauri/src/split.rs` (new, register in `lib.rs`) | Tauri commands: `split_detect`, `split_render`, progress events via the existing job system — mirror `transcript.rs`/`clip_studio.rs` shape |
| `apps/desktop/src/` | Split tab in `RightPanel.tsx`; `SplitPanel.tsx` reusing `CutReviewList` + `WaveformCutOverlay`; Settings group in `SettingsScreen.tsx` |
| `apps/desktop/src-tauri/src/watch.rs` + `batch.rs` | §7 rule extension |
| `eval/` | §8 fixture + gates |

**Video path (the one genuinely new capability).** Today `video.rs` is remux-only (`-c:v copy`, "no code path that touches video frames"). Frame-accurate splitting **requires re-encoding video** — keyframe-snapped stream copy can land seconds off (would clip speech or leave the cut word in; unacceptable). Per part, one sidecar invocation: accurate input seek (`-ss {part.start}` before `-i`, `-t {part.dur}` — frame-accurate *because* we re-encode), video re-encoded via the **existing encoder-selection ladder** built for Clip Studio (`FfmpegSidecar::h264_encoder`: OS encoders first — Media Foundation / VideoToolbox — then OpenH264, GPL encoders refused; the licensing constraint in `clip.rs`'s header applies verbatim), source resolution/fps preserved, quality target ≈ max(source bitrate × 1.2, codec floor); HEVC sources → H.264 output v1 (note in results line). Mastered part audio muxed from `pipe:0` exactly like `remux_with_audio_spec` (`-map` video / pipe-audio, container-appropriate audio codec). Audio timeline is authoritative; A/V sync gate in §8. **Smart-cut (re-encode only boundary GOPs, stream-copy the rest) is explicitly v2/roadmap** — big win, notoriously fiddly, not needed to ship.

## 7. Batch & watch integration (the zero-click end state)

`WatchRule`/batch `OutputSettings` gain an optional `split: Option<SplitSettings>` (cut word, template, master on/off). The owner's real workflow becomes: finish teleprompter session → recorder drops the file in the watch folder → N mastered, named shorts appear. This is the demo GIF for the feature announcement.

## 8. Acceptance criteria & eval gates

Synthetic fixture — extend `eval/synth.py` (it already generates the README demo's synthetic narration, so the whole fixture reproduces from scratch with no recording session): 10 known scripts joined with the cut word + varied gaps (0.4–3 s), rendered as audio AND as 1080p video; ground-truth boundary times in the manifest.

- Exactly 10 segments out (audio and video), no empties, deterministic across two runs (hash-identical audio outputs).
- Every boundary within **±120 ms** of ground truth (pads accounted).
- **Re-transcribe every segment: the cut word appears in none of them.** (The owner's core ask, mechanically verified.)
- Each segment: integrated loudness within ±0.5 LU of preset target, TP ≤ ceiling (existing gates, applied per part).
- Video: per-segment A/V sync within **±1 frame** (ffprobe first-audio/first-video pts check), duration drift ≤ 1 frame, plays in Windows Films&TV + QuickTime.
- Detection: on the fixture set with 3 fuzzy-spelled instances planted, exact+fuzzy finds 13/13 with ≤ 1 false positive (which the review UI catches — false positives arrive unaccepted).
- UI: Detect → review → export flow ≤ 4 interactions with a saved default cut word; 3-s-segment warning fires on the planted double-word case.
- Airplane-mode test still passes (feature is ASR + ffmpeg, both local).

## 9. Edge cases (test fixtures, not hopes)

Cut word inside actual script content (review UI + distinctive-word guidance; per-row reject) · said twice in a row (degenerate part dropped) · at file start/end (empty leading/trailing parts dropped) · never detected (0 matches → hint card: check spelling, try fuzzy, add manual splits; manual-only splitting fully supported) · whisper writes it as two tokens ("kum quat" — phrase matching with the gap rule catches it) · non-English scripts (matching is language-agnostic casefold; works with any whisper language) · VFR video (existing decode handles; `-vsync` per current clip path) · 4-hour take with 40 cut words (streaming analysis, per-part renders bounded) · mono/stereo mismatch across presets (existing OutputSpec handles).

## 10. Non-goals now / roadmap

- **Retake word** (natural sequel, not in this build): a second keyword meaning "discard the previous attempt of this script and keep my re-read" — turns flubbed takes into a solved problem. Design note kept here because the partition machinery makes it a small delta later.
- Keyword-spotting fast path (detect without full ASR) — pointless while transcripts are wanted for naming/captions anyway.
- Smart-cut video (v2, §6). Per-segment Clip Studio handoff ("Send segments to Clip Studio") — P2 button once both features are stable.

## 11. Build plan (Opus-and-lower rule stands)

1. `anvil-cut::split` — detection + partition + fades + golden tests (opus, the correctness core).
2. `anvil-media::export_video_segment` + A/V sync test rig (opus).
3. CLI `split` + eval fixture/gates wired (sonnet).
4. Settings fields + Tauri `split.rs` + Split tab UI (sonnet).
5. Watch/batch `SplitSettings` (sonnet).
6. Docs page + README feature row + demo GIF script (haiku).

Estimate: 4–6 focused sessions. Order matters: 1–3 prove the feature end-to-end headless (CLI + eval green) before any UI exists.
