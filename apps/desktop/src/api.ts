// Typed wrappers over the Rust playback + waveform + master/export commands (anvil-audio
// behind Tauri). Audio never crosses the webview: these are remote-control messages, not
// audio data (ADR-010). The only "audio" that reaches JS is min/max peaks for drawing.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { Tier } from "./lib/presets";

export interface AppInfo {
  name: string;
  version: string;
  chain_version: number;
  platform: string;
}

export interface MediaSummary {
  duration_secs: number;
  channels: number;
  sample_rate: number;
}

/** A `[min, max]` peak pair for one waveform bin. */
export type Peak = [number, number];

export function appInfo(): Promise<AppInfo> {
  return invoke<AppInfo>("app_info");
}

/** Decode a file, build its peaks pyramid, and load it into the engine. */
export function openMedia(path: string): Promise<MediaSummary> {
  return invoke<MediaSummary>("open_media", { path });
}

/** Tell Rust the webview has mounted and is listening for `open-file`, flushing any OS
 *  "Open With" / file-association opens that arrived during startup (see {@link onOpenFile}
 *  and the `frontend_ready` / `RunEvent::Opened` handling in `lib.rs`). */
export function frontendReady(): Promise<void> {
  return invoke<void>("frontend_ready");
}

/** Subscribe to OS "Open With" / file-association / `open -a Cleanroom <file>` opens routed
 *  from Rust. The payload is an absolute file path; handle it like a drag-drop (load it into
 *  the Master screen). Returns the unlisten fn. */
export function onOpenFile(handler: (path: string) => void): Promise<UnlistenFn> {
  return listen<string>("open-file", (event) => handler(event.payload));
}

/** Fetch `bins` min/max pairs spanning `[startFrame, endFrame)` (48 kHz frames). */
export function getPeaks(
  startFrame: number,
  endFrame: number,
  bins: number,
): Promise<Peak[]> {
  return invoke<Peak[]>("get_peaks", {
    startFrame,
    endFrame,
    bins,
  });
}

/** Same as {@link getPeaks} but for the mastered buffer (empty until `master` has run). */
export function getProcessedPeaks(
  startFrame: number,
  endFrame: number,
  bins: number,
): Promise<Peak[]> {
  return invoke<Peak[]>("get_processed_peaks", {
    startFrame,
    endFrame,
    bins,
  });
}

export function play(): Promise<void> {
  return invoke("play");
}

export function pause(): Promise<void> {
  return invoke("pause");
}

/** Seek to a frame in the internal 48 kHz domain. */
export function seek(frame: number): Promise<void> {
  return invoke("seek", { frame });
}

/** Current playhead in 48 kHz frames. */
export function playbackPosition(): Promise<number> {
  return invoke<number>("playback_position");
}

/** Which buffer the engine has loaded for A/B: the untouched source or the mastered take. */
export type AbSource = "original" | "processed";

/** Switch playback to `source`, sample-aligned, preserving position and play state. */
export function setAb(source: AbSource): Promise<void> {
  return invoke("set_ab", { source });
}

// ---- Master (JSON contract — see handoff/04-FEATURES-UX.md §S2 Master tab) ----

export interface AnalysisReport {
  integrated_lufs: number;
  true_peak_dbtp: number;
  loudness_range_lu: number;
  snr_db: number;
  speech_ratio: number;
  music_ratio: number;
  clipping_regions: number;
  bandwidth_hz: number;
  reverb_bucket: string;
}

export interface LoudnessTriple {
  integrated_lufs: number;
  true_peak_dbtp: number;
  loudness_range_lu: number;
}

export interface ModuleReport {
  name: string;
  engaged: boolean;
  strength: number | null;
  detail: string;
}

export interface HealthFinding {
  severity: "info" | "warn";
  title: string;
  detail: string;
  fix: string | null;
}

export interface MasterReport {
  analysis: AnalysisReport;
  before: LoudnessTriple;
  after: LoudnessTriple;
  preset: string;
  tier: string;
  chain_version: number;
  modules: ModuleReport[];
  health_card: HealthFinding[];
}

/** Run the one-click Master chain at `preset`/`tier` over the currently open file. */
export function master(preset: string, tier: string): Promise<MasterReport> {
  return invoke<MasterReport>("master", { preset, tier });
}

// ---- Export (04 §S2 Export tab) ----

export type ExportFormat = "wav" | "mp3" | "flac" | "opus" | "aac";

export interface OutputSpec {
  format: ExportFormat;
  path: string;
  bitrate?: number | null;
  mono?: boolean | null;
}

export interface OutputResultItem {
  path: string;
  ok: boolean;
  message: string | null;
}

export interface ExportResult {
  ok: boolean;
  outputs: OutputResultItem[];
  compliance_report: string | null;
  compliance_error: string | null;
}

/** Export every output from the one mastered buffer (no re-decode between them). When
 * `compliance` is set, also writes an HTML+PDF report from the last Master run's real
 * measurements next to the first output that succeeds. */
export function exportOutputs(outputs: OutputSpec[], compliance: boolean): Promise<ExportResult> {
  return invoke<ExportResult>("export_outputs", { outputs, compliance });
}

/** Per-output encode progress while `exportOutputs` is in flight. */
export interface ExportProgressEvent {
  index: number;
  fraction: number;
}

export function onExportProgress(
  cb: (e: ExportProgressEvent) => void,
): Promise<UnlistenFn> {
  return listen<ExportProgressEvent>("export://progress", (e) => cb(e.payload));
}

// ---- Presets manager (04 §S6) ----

export interface PresetSummary {
  /** The id `master`/batch/watch accept: a shipped id, or `"user:<uuid>"`. */
  preset_ref: string;
  name: string;
  tier: Tier;
  target_lufs: number;
  true_peak_ceiling_dbtp: number;
  /** "shipped" presets are read-only (duplicate to customize); "user" ones are the
   * editable/duplicable/deletable library. */
  source: "shipped" | "user";
}

export interface PresetEdit {
  name: string;
  tier: Tier;
  target_lufs: number;
  true_peak_ceiling_dbtp: number;
}

export function presetsList(): Promise<PresetSummary[]> {
  return invoke<PresetSummary[]>("presets_list");
}

export function presetsDuplicate(presetRef: string, newName: string): Promise<PresetSummary> {
  return invoke<PresetSummary>("presets_duplicate", { presetRef, newName });
}

export function presetsUpdate(presetRef: string, edit: PresetEdit): Promise<PresetSummary> {
  return invoke<PresetSummary>("presets_update", { presetRef, edit });
}

export function presetsDelete(presetRef: string): Promise<void> {
  return invoke("presets_delete", { presetRef });
}

export function presetsImport(path: string): Promise<PresetSummary> {
  return invoke<PresetSummary>("presets_import", { path });
}

export function presetsExport(presetRef: string, destPath: string): Promise<void> {
  return invoke("presets_export", { presetRef, destPath });
}

// ---- Batch (04 §S4) ----

export type BatchItemState = "queued" | "running" | "done" | "failed" | "cancelled";

export interface BatchItemStatus {
  id: string;
  input: string;
  output: string;
  state: BatchItemState;
  progress: number;
  message: string;
  error: string | null;
}

export interface BatchOutputSpec {
  output_dir: string;
  preserve_structure?: boolean;
  naming?: string;
}

export function batchSubmitFiles(
  inputs: string[],
  presetRef: string,
  tier: Tier,
  output: BatchOutputSpec,
): Promise<string[]> {
  return invoke<string[]>("batch_submit_files", { inputs, presetRef, tier, output });
}

export function batchSubmitFolder(
  root: string,
  presetRef: string,
  tier: Tier,
  output: BatchOutputSpec,
): Promise<string[]> {
  return invoke<string[]>("batch_submit_folder", { root, presetRef, tier, output });
}

export function batchSnapshot(): Promise<BatchItemStatus[]> {
  return invoke<BatchItemStatus[]>("batch_snapshot");
}

export function batchOverallProgress(): Promise<number> {
  return invoke<number>("batch_overall_progress");
}

export function batchCancel(id: string): Promise<boolean> {
  return invoke<boolean>("batch_cancel", { id });
}

export function batchCancelAll(): Promise<void> {
  return invoke("batch_cancel_all");
}

export function batchPause(): Promise<void> {
  return invoke("batch_pause");
}

export function batchResume(): Promise<void> {
  return invoke("batch_resume");
}

export function batchIsPaused(): Promise<boolean> {
  return invoke<boolean>("batch_is_paused");
}

export function batchReorder(id: string, newIndex: number): Promise<boolean> {
  return invoke<boolean>("batch_reorder", { id, newIndex });
}

export function batchRemove(id: string): Promise<boolean> {
  return invoke<boolean>("batch_remove", { id });
}

export function batchRetryFailed(): Promise<string[]> {
  return invoke<string[]>("batch_retry_failed");
}

/** Tell "drop N files" from "drop a folder" apart for the S4 drop zone (no dialog picker
 * in this build — see `apps/desktop` handoff notes on why text-path inputs are used
 * instead of a file-dialog plugin). */
export function batchPathKind(path: string): Promise<"file" | "dir" | "missing"> {
  return invoke("batch_path_kind", { path });
}

export function onBatchProgress(cb: (items: BatchItemStatus[]) => void): Promise<UnlistenFn> {
  return listen<BatchItemStatus[]>("batch://progress", (e) => cb(e.payload));
}

// ---- Watch folders (04 §S5) ----

export type FilePattern = "any_supported" | { extensions: string[] };

export interface WatchRule {
  id: string;
  folder: string;
  preset: {
    schema_version: number;
    name: string;
    tier: Tier;
    target_lufs: number;
    true_peak_ceiling_dbtp: number;
  };
  tier: Tier;
  output_dir: string;
  pattern: FilePattern;
  enabled: boolean;
}

export interface WatchRuleStatus {
  rule: WatchRule;
  error: string | null;
}

export function watchListRules(): Promise<WatchRuleStatus[]> {
  return invoke<WatchRuleStatus[]>("watch_list_rules");
}

export function watchAddRule(
  folder: string,
  presetRef: string,
  tier: Tier,
  outputDir: string,
  extensions?: string[],
): Promise<string> {
  return invoke<string>("watch_add_rule", { folder, presetRef, tier, outputDir, extensions });
}

export function watchRemoveRule(id: string): Promise<boolean> {
  return invoke<boolean>("watch_remove_rule", { id });
}

export function watchSetEnabled(id: string, enabled: boolean): Promise<boolean> {
  return invoke<boolean>("watch_set_enabled", { id, enabled });
}

export function watchRetryUnreachable(): Promise<void> {
  return invoke("watch_retry_unreachable");
}

export function onWatchStatus(cb: (rules: WatchRuleStatus[]) => void): Promise<UnlistenFn> {
  return listen<WatchRuleStatus[]>("watch://status", (e) => cb(e.payload));
}

// ---- Models manager (04 §S7) ----

export interface ModelPack {
  id: string;
  name: string;
  detail: string;
  size: string;
  size_bytes: number;
  license: string;
  /** "denoise" | "asr" | "llm" | "diarize" — the Transcript tab's model picker filters to
   * "asr". */
  kind: "denoise" | "asr" | "llm" | "diarize";
  installed: boolean;
  downloadable: boolean;
  arrives: string | null;
  /** Bytes already downloaded toward this pack from a prior in-progress/cancelled run. */
  downloaded_bytes: number;
  /** Provisioned by the app installer, not downloaded here (the diarization models). The
   * UI shows a "comes with the app" note instead of a download button. */
  installer_provisioned: boolean;
}

export function modelsList(): Promise<ModelPack[]> {
  return invoke<ModelPack[]>("models_list");
}

/** Start (or resume) downloading `pack` in the background; progress streams over
 * {@link onModelDownloadProgress}. */
export function downloadModel(pack: string): Promise<void> {
  return invoke("download_model", { pack });
}

/** Cancel an in-flight download, keeping its progress on disk so the next
 * {@link downloadModel} call resumes instead of restarting. */
export function downloadModelCancel(pack: string): Promise<boolean> {
  return invoke<boolean>("download_model_cancel", { pack });
}

export interface ModelDownloadProgress {
  pack: string;
  downloaded_bytes: number;
  total_bytes: number;
  status: "downloading" | "verifying" | "done" | "paused" | "error";
  message: string | null;
}

export function onModelDownloadProgress(
  cb: (e: ModelDownloadProgress) => void,
): Promise<UnlistenFn> {
  return listen<ModelDownloadProgress>("models://download", (e) => cb(e.payload));
}

// ---- Transcript tab (04 §S2 Transcript tab, M3) ----

export interface TranscriptWord {
  text: string;
  start: number;
  end: number;
  confidence: number;
  /** Diarized speaker id, or null/undefined before diarization (see {@link diarize}). */
  speaker?: number | null;
}

export interface TranscriptSegment {
  text: string;
  start: number;
  end: number;
  /** The segment's dominant diarized speaker, or null/undefined before diarization. */
  speaker?: number | null;
}

export interface Transcript {
  language: string;
  words: TranscriptWord[];
  segments: TranscriptSegment[];
}

/** Transcribe the currently open file with `model` (a whisper pack id from
 * {@link modelsList}, filtered to `kind === "asr"`). */
export function transcribe(model: string): Promise<Transcript> {
  return invoke<Transcript>("transcribe", { model });
}

/** One diarized speaker: a dense id and a display label ("Speaker 1", …). */
export interface Speaker {
  id: number;
  label: string;
}

export interface DiarizeResult {
  /** The last transcript, now with a `speaker` on every word/segment. */
  transcript: Transcript;
  speakers: Speaker[];
}

/** Identify speakers in the currently open file and map the turns onto the last
 * {@link transcribe} result (04 §S2 "speaker labels"). Requires a transcript first.
 * `numSpeakers` forces an exact count (e.g. 2 for host + guest); omit to auto-detect.
 * Returns a clean error when the diarization sidecar/models aren't installed. */
export function diarize(numSpeakers?: number | null): Promise<DiarizeResult> {
  return invoke<DiarizeResult>("diarize", { numSpeakers: numSpeakers ?? null });
}

export type CutKind = "silence" | "filler";

export interface Cut {
  start: number;
  end: number;
  kind: CutKind;
  label: string;
  accepted: boolean;
}

export interface CutPlan {
  cuts: Cut[];
}

/** Plan silence/filler cuts for the currently open file. `mode` is `"silence"`,
 * `"filler"`, or `"both"`. */
export function planCuts(mode: "silence" | "filler" | "both"): Promise<CutPlan> {
  return invoke<CutPlan>("plan_cuts", { mode });
}

/** Apply the given complete set of accepted cut indices (from the last {@link planCuts}
 * result) — re-renders and swaps in the processed buffer, same as `master`. */
export function applyCuts(acceptedIndices: number[]): Promise<void> {
  return invoke("apply_cuts", { acceptedIndices });
}

export type TranscriptExportFormat = "srt" | "vtt" | "txt" | "json";

/** Format the last `transcribe` result as SRT/VTT/TXT/JSON text (not written to disk). */
export function exportTranscript(format: TranscriptExportFormat): Promise<string> {
  return invoke<string>("export_transcript", { format });
}

/** Write already-formatted text to `path` (used after {@link exportTranscript}). */
export function writeTextFile(path: string, content: string): Promise<void> {
  return invoke("write_text_file", { path, content });
}

// ---- Chapters & Metadata tab (04 §S2, M2) ----------------------------------------------

export interface ChapterWire {
  title: string;
  start_ms: number;
  end_ms: number;
}

export interface FileMetadata {
  path: string;
  title: string | null;
  artist: string | null;
  album: string | null;
  genre: string | null;
  date: string | null;
  comment: string | null;
  track: number | null;
  /** Existing cover art as a `data:` URL the UI renders directly, or null. */
  cover_art: string | null;
  cover_mime: string | null;
  chapters: ChapterWire[];
  /** false when chapters couldn't be read (ffmpeg unavailable) — tags are still valid. */
  chapters_available: boolean;
  chapters_note: string | null;
}

export interface MetadataEdit {
  title?: string | null;
  artist?: string | null;
  album?: string | null;
  genre?: string | null;
  date?: string | null;
  comment?: string | null;
  track?: number | null;
  /** Path to a new cover-art image to embed; omit to leave existing art untouched. */
  cover_art_path?: string | null;
  remove_cover_art?: boolean;
  chapters: ChapterWire[];
}

/** Read the currently open file's standard tags + chapters. */
export function metadataRead(): Promise<FileMetadata> {
  return invoke<FileMetadata>("metadata_read");
}

/** Write the edited tags + chapters to `target` (the UI defaults it to the source file, so
 * this edits in place; point it at a mastered export to tag that instead). Writing a
 * non-empty chapter list needs the ffmpeg component; a tags-only edit does not. */
export function metadataWrite(target: string, edit: MetadataEdit): Promise<void> {
  return invoke("metadata_write", { target, edit });
}

// ---- AI shownotes (04 §S2 Chapters & Metadata "AI suggest", M4) ------------------------

export interface ShownoteChapter {
  title: string;
  start_secs: number;
}

export interface ShownotesResult {
  summary: string;
  bullets: string[];
  chapters: ShownoteChapter[];
  titles: string[];
  keywords: string[];
  /** "llm" when the local Qwen model wrote these; "fallback" for the built-in summarizer. */
  engine: "llm" | "fallback";
  /** Actionable note on the fallback path (why the AI model wasn't used). */
  note: string | null;
}

/** Generate episode shownotes (summary, chapters, titles, keywords) from the last
 * {@link transcribe} result. Uses the local Qwen model when installed; otherwise degrades
 * cleanly to the built-in extractive summarizer (flagged in `engine`/`note`), never erroring
 * over a missing model. Requires a transcript first. */
export function generateShownotes(): Promise<ShownotesResult> {
  return invoke<ShownotesResult>("generate_shownotes");
}

// ---- S3 Multitrack (04 §S3, M4) --------------------------------------------------------
//
// `multitrack_mix` hands back a plain WAV path — open it with {@link openMedia} (same as
// any other file) to get the mix into the engine, then the existing `master`/
// `exportOutputs`/`setAb`/`getPeaks` calls operate on it unchanged. Field names below are
// the exact Rust struct field names (snake_case) since a struct-typed Tauri argument is
// deserialized by serde as-is, not through the camelCase top-level-argument conversion —
// same convention as {@link BatchOutputSpec} above.

export interface TrackWire {
  id: string;
  file_name: string;
  duration_secs: number;
  channels: number;
  sample_rate: number;
  /** "speaker" | "music" — auto-detected on load, user-editable. */
  tag: string;
  solo: boolean;
  mute: boolean;
  gain_db: number;
  duck_enabled: boolean;
  duck_amount_db: number;
}

export interface TrackPatch {
  tag?: string;
  solo?: boolean;
  mute?: boolean;
  gain_db?: number;
  duck_enabled?: boolean;
  duck_amount_db?: number;
}

export interface AlignmentInfo {
  applied: boolean;
  offset_secs: number;
  drift_corrected: boolean;
  message: string;
}

export interface MixSummary {
  path: string;
  duration_secs: number;
  channels: number;
  sample_rate: number;
  track_count: number;
}

/** Decode and append each path as a new track. Fails on the first unreadable file. */
export function multitrackLoadTracks(paths: string[]): Promise<TrackWire[]> {
  return invoke<TrackWire[]>("multitrack_load_tracks", { paths });
}

export function multitrackListTracks(): Promise<TrackWire[]> {
  return invoke<TrackWire[]>("multitrack_list_tracks");
}

export function multitrackGetPeaks(
  trackId: string,
  startFrame: number,
  endFrame: number,
  bins: number,
): Promise<Peak[]> {
  return invoke<Peak[]>("multitrack_get_peaks", { trackId, startFrame, endFrame, bins });
}

export function multitrackUpdateTrack(trackId: string, patch: TrackPatch): Promise<TrackWire> {
  return invoke<TrackWire>("multitrack_update_track", { trackId, patch });
}

export function multitrackRemoveTrack(trackId: string): Promise<void> {
  return invoke("multitrack_remove_track", { trackId });
}

export function multitrackClear(): Promise<void> {
  return invoke("multitrack_clear");
}

/** Real GCC-PHAT cross-correlation alignment + clock-drift estimate (via the `anvil_multitrack`
 * crate) — reports the offset the mix will apply, or says plainly when the tracks don't
 * correlate. */
export function multitrackAlign(): Promise<AlignmentInfo> {
  return invoke<AlignmentInfo>("multitrack_align");
}

export function multitrackUndoAlign(): Promise<void> {
  return invoke("multitrack_undo_align");
}

export function multitrackGetAlignment(): Promise<AlignmentInfo | null> {
  return invoke<AlignmentInfo | null>("multitrack_get_alignment");
}

/** Mix every non-muted (solo-respecting) track down to one buffer and write it to a temp
 * WAV — open the returned path with {@link openMedia} to load it into the engine. */
export function multitrackMix(): Promise<MixSummary> {
  return invoke<MixSummary>("multitrack_mix");
}

// ---- Clip Studio (04 §Clip Studio, M4) -------------------------------------------------

export interface ClipRange {
  start_secs: number;
  end_secs: number;
}

export interface ClipBackground {
  /** "waveform" | "color" | "cover_art". */
  kind: string;
  /** Hex color (`#rrggbb`) — used for "color", and as the waveform line color. */
  color?: string | null;
  cover_art_path?: string | null;
}

export interface ClipRenderRequest {
  range: ClipRange;
  /** "1:1" | "9:16" | "16:9". */
  aspect: string;
  /** "clean" | "bold" | "minimal". */
  caption_style: string;
  captions_enabled: boolean;
  background: ClipBackground;
  title: string;
  out_path: string;
}

export interface ClipRenderResult {
  ok: boolean;
  path: string;
  message: string | null;
  /** Honest fidelity disclosures (e.g. "captions aren't word-highlight karaoke yet") —
   * never silently swallowed. Empty when nothing was downgraded. */
  seam_notes: string[];
}

/** Render the selected range of the currently open file to an MP4. Progress streams over
 * {@link onClipProgress}. */
export function clipStudioRender(request: ClipRenderRequest): Promise<ClipRenderResult> {
  return invoke<ClipRenderResult>("clip_studio_render", { request });
}

export interface ClipProgressEvent {
  fraction: number;
}

export function onClipProgress(cb: (e: ClipProgressEvent) => void): Promise<UnlistenFn> {
  return listen<ClipProgressEvent>("clip://progress", (e) => cb(e.payload));
}

// ---- S10 Recording Guard (04 §S10, M4) -------------------------------------------------

export interface GuardDevice {
  name: string;
  is_default: boolean;
}

export interface GuardMeter {
  running: boolean;
  device_name: string;
  sample_rate: number;
  channels: number;
  peak_dbfs: number;
  rms_dbfs: number;
  /** "hot" | "good" | "quiet" | "silent". */
  headroom_level: string;
  headroom_message: string;
  noise_floor_dbfs: number;
  /** "quiet" | "some_hiss" | "noisy". */
  noise_rating: string;
  noise_message: string;
}

export interface ClapResult {
  ok: boolean;
  rt60_secs: number | null;
  /** "dry" | "ok" | "noticeable" | "bad" — present only when `rt60_secs` is. */
  reverb_bucket: string | null;
  message: string;
}

export function guardListDevices(): Promise<GuardDevice[]> {
  return invoke<GuardDevice[]>("guard_list_devices");
}

/** Start (or, if already running, just report) the live input meter. `deviceName` picks a
 * specific input; omit for the system default. */
export function guardStart(deviceName?: string | null): Promise<GuardMeter> {
  return invoke<GuardMeter>("guard_start", { deviceName: deviceName ?? null });
}

export function guardStop(): Promise<void> {
  return invoke("guard_stop");
}

/** Poll the current meter reading — call this on an interval while the meter is running. */
export function guardMeter(): Promise<GuardMeter> {
  return invoke<GuardMeter>("guard_meter");
}

/** Capture `durationSecs` of audio and estimate room echo from its decay (blocks for the
 * capture window — it's a short, foreground, user-initiated test). */
export function guardClapTest(durationSecs: number): Promise<ClapResult> {
  return invoke<ClapResult>("guard_clap_test", { durationSecs });
}

// ---- Batch crash recovery (05 §M5.F) ---------------------------------------------------

export interface RecoveryEntry {
  input: string;
  output: string;
}

/** Files still `queued`/`running` in a batch ledger from a session that didn't drain
 * cleanly (crash, forced quit, power loss) — empty in the normal case. Never
 * auto-resubmitted; the UI just tells the user so they can re-drop what's missing. */
export function batchCheckRecovery(): Promise<RecoveryEntry[]> {
  return invoke<RecoveryEntry[]>("batch_check_recovery");
}

/** Acknowledge the recovery banner and clear the ledger. */
export function batchDismissRecovery(): Promise<void> {
  return invoke("batch_dismiss_recovery");
}

// ---- Settings (04 §S8) -------------------------------------------------------------------

export interface DiagnosticsResult {
  zip_path: string;
  log_file_count: number;
}

/** Zip logs + basic system info (never audio, never project content — see
 * `diagnostics.rs`'s module doc) to `targetPath` for attaching to a GitHub issue. */
export function exportDiagnostics(targetPath: string): Promise<DiagnosticsResult> {
  return invoke<DiagnosticsResult>("export_diagnostics", { targetPath });
}

export function settingsSetContextMenu(enabled: boolean): Promise<void> {
  return invoke("settings_set_context_menu", { enabled });
}

export function settingsSetFileAssociations(enabled: boolean): Promise<void> {
  return invoke("settings_set_file_associations", { enabled });
}

export function settingsSetAutostart(enabled: boolean): Promise<void> {
  return invoke("settings_set_autostart", { enabled });
}

export interface IntegrationStatus {
  autostart: boolean;
}

export function settingsGetIntegrationStatus(): Promise<IntegrationStatus> {
  return invoke<IntegrationStatus>("settings_get_integration_status");
}
