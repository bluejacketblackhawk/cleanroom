import { useCallback, useEffect, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  multitrackAlign,
  multitrackClear,
  multitrackGetAlignment,
  multitrackListTracks,
  multitrackLoadTracks,
  multitrackMix,
  multitrackRemoveTrack,
  multitrackUndoAlign,
  multitrackUpdateTrack,
  type AbSource,
  type AlignmentInfo,
  type MasterReport,
  type MediaSummary,
  type PresetSummary,
  type TrackPatch,
  type TrackWire,
} from "../api";
import Waveform from "../Waveform";
import TrackLane from "../components/TrackLane";
import RightPanel, { type PanelTab } from "../components/RightPanel";
import MasterPanel from "../components/MasterPanel";
import ExportPanel from "../components/ExportPanel";
import { formatTime } from "../lib/format";
import type { Tier } from "../lib/presets";

interface MultitrackScreenProps {
  // Shared engine/playback state, owned by App.tsx — same shape it hands TranscriptScreen.
  media: MediaSummary | null;
  fileName: string | null;
  sourcePath: string | null;
  totalFrames: number;
  isPlaying: boolean;
  onPlay: () => void;
  onPause: () => void;
  abSource: AbSource;
  onToggleAb: () => void;
  activeTab: PanelTab;
  onTabChange: (tab: PanelTab) => void;

  // Master tab state/handlers, also owned by App.tsx — reused as-is so "the same
  // right-panel tabs operate on the mix" rather than a parallel implementation.
  tier: Tier;
  preset: string;
  presets: PresetSummary[];
  onTierChange: (t: Tier) => void;
  onPresetChange: (p: string) => void;
  onMaster: () => void;
  mastering: boolean;
  progressVerb: string;
  masterError: string | null;
  report: MasterReport | null;
  dirty: boolean;
  advanced: boolean;
  onToggleAdvanced: () => void;
  onToggleModule: (i: number) => void;
  onModuleStrength: (i: number, v: number) => void;
  onFix: (code: string) => void;

  /** App.tsx's `loadPath` — opens a file path into the engine exactly like a normal drop.
   * Called with the mixdown's temp WAV path once `multitrack_mix` succeeds. */
  onLoadPath: (path: string) => Promise<void>;
}

const GAIN_MIN = -48;
const GAIN_MAX = 24;

/**
 * S3 Multitrack (04 §S3): drop N tracks → per-track lanes (waveform, solo/mute/gain,
 * speaker/music tag, ducking on music tracks) → an alignment banner → Mix, which loads the
 * mixed-down buffer into the shared engine so the Master/Export tabs below "just work" on
 * it, unchanged.
 */
export default function MultitrackScreen({
  media,
  fileName,
  sourcePath,
  totalFrames,
  isPlaying,
  onPlay,
  onPause,
  abSource,
  onToggleAb,
  activeTab,
  onTabChange,
  tier,
  preset,
  presets,
  onTierChange,
  onPresetChange,
  onMaster,
  mastering,
  progressVerb,
  masterError,
  report,
  dirty,
  advanced,
  onToggleAdvanced,
  onToggleModule,
  onModuleStrength,
  onFix,
  onLoadPath,
}: MultitrackScreenProps) {
  const [tracks, setTracks] = useState<TrackWire[]>([]);
  const [alignment, setAlignment] = useState<AlignmentInfo | null>(null);
  const [dragActive, setDragActive] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [aligning, setAligning] = useState(false);
  const [mixing, setMixing] = useState(false);
  const [mixError, setMixError] = useState<string | null>(null);

  // Re-sync with the backend on mount — tracks/alignment survive switching nav away and
  // back (they live in Tauri-managed state, not React state).
  useEffect(() => {
    multitrackListTracks().then(setTracks).catch(() => {});
    multitrackGetAlignment().then(setAlignment).catch(() => {});
  }, []);

  const loadPaths = useCallback(async (paths: string[]) => {
    setLoadError(null);
    try {
      const added = await multitrackLoadTracks(paths);
      setTracks((rows) => [...rows, ...added]);
    } catch (e) {
      setLoadError(typeof e === "string" ? e : "Could not load one of those files.");
    }
  }, []);

  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      const p = event.payload;
      if (p.type === "enter" || p.type === "over") {
        setDragActive(true);
      } else if (p.type === "leave") {
        setDragActive(false);
      } else if (p.type === "drop") {
        setDragActive(false);
        if (p.paths.length > 0) void loadPaths(p.paths);
      }
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, [loadPaths]);

  const updateTrack = async (id: string, patch: TrackPatch) => {
    // Optimistic: the slider/toggle should feel instant, the backend call reconciles.
    setTracks((rows) => rows.map((t) => (t.id === id ? { ...t, ...patch } : t)));
    try {
      const updated = await multitrackUpdateTrack(id, patch);
      setTracks((rows) => rows.map((t) => (t.id === id ? updated : t)));
    } catch {
      // Best-effort — the optimistic value stays; the next full mix will use whatever the
      // backend actually holds if this silently failed.
    }
  };

  const removeTrack = async (id: string) => {
    setTracks((rows) => rows.filter((t) => t.id !== id));
    try {
      await multitrackRemoveTrack(id);
    } catch {
      // ignore
    }
  };

  const clearAll = async () => {
    setTracks([]);
    setAlignment(null);
    try {
      await multitrackClear();
    } catch {
      // ignore
    }
  };

  const handleAlign = async () => {
    setAligning(true);
    setLoadError(null);
    try {
      setAlignment(await multitrackAlign());
    } catch (e) {
      setLoadError(typeof e === "string" ? e : "Could not align these tracks.");
    } finally {
      setAligning(false);
    }
  };

  const handleUndoAlign = async () => {
    setAlignment(null);
    try {
      await multitrackUndoAlign();
    } catch {
      // ignore
    }
  };

  const handleMix = async () => {
    setMixing(true);
    setMixError(null);
    try {
      const summary = await multitrackMix();
      await onLoadPath(summary.path);
    } catch (e) {
      setMixError(typeof e === "string" ? e : "Could not mix these tracks.");
    } finally {
      setMixing(false);
    }
  };

  return (
    <main className="flex flex-1 flex-col gap-5 overflow-y-auto p-6">
      <div className="flex flex-wrap items-baseline justify-between gap-4">
        <div>
          <h1 className="text-lg font-semibold">Multitrack</h1>
          <p className="text-xs text-neutral-500 dark:text-neutral-400">
            Drop several tracks — mics, remote call recordings, a music bed — then mix them
            down to one file the Master tab works on.
          </p>
        </div>
        {tracks.length > 0 && (
          <button
            type="button"
            onClick={() => void clearAll()}
            className="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
          >
            Clear tracks
          </button>
        )}
      </div>

      <div
        className={`flex flex-col items-center gap-1 rounded-xl border-2 border-dashed p-6 text-center transition-colors ${
          dragActive
            ? "border-emerald-500 bg-emerald-500/5"
            : "border-neutral-300 dark:border-neutral-700"
        }`}
      >
        <p className="text-sm font-medium">Drop tracks here</p>
        <p className="text-xs text-neutral-500 dark:text-neutral-400">Each dropped file becomes its own lane.</p>
      </div>
      {loadError && <p className="text-xs text-red-500">{loadError}</p>}

      {tracks.length > 0 && (
        <>
          {alignment && (
            <div
              className={`flex flex-wrap items-center justify-between gap-3 rounded-lg border px-3 py-2 text-xs ${
                alignment.applied
                  ? "border-emerald-500/40 bg-emerald-500/5 text-emerald-700 dark:text-emerald-300"
                  : "border-neutral-300 text-neutral-600 dark:border-neutral-700 dark:text-neutral-300"
              }`}
            >
              <span>{alignment.message}</span>
              <button
                type="button"
                onClick={() => void handleUndoAlign()}
                className="font-medium text-neutral-500 dark:text-neutral-400 hover:text-neutral-800 dark:hover:text-neutral-200"
              >
                Undo
              </button>
            </div>
          )}

          <div className="flex flex-col gap-3">
            {tracks.map((t) => (
              <div
                key={t.id}
                className="flex flex-col gap-2 rounded-lg border border-neutral-200 p-3 dark:border-neutral-800"
              >
                <div className="flex flex-wrap items-center gap-2">
                  <span className="min-w-0 flex-1 truncate text-sm font-medium" title={t.file_name}>
                    {t.file_name}
                  </span>
                  <span className="text-xs text-neutral-500 dark:text-neutral-400">{formatTime(t.duration_secs)}</span>
                  <button
                    type="button"
                    title="Toggle speaker/music tag"
                    onClick={() =>
                      void updateTrack(t.id, { tag: t.tag === "music" ? "speaker" : "music" })
                    }
                    className="rounded-full border border-neutral-300 px-2.5 py-1 text-xs font-medium capitalize text-neutral-600 dark:border-neutral-700 dark:text-neutral-300"
                  >
                    {t.tag}
                  </button>
                  <button
                    type="button"
                    aria-pressed={t.solo}
                    onClick={() => void updateTrack(t.id, { solo: !t.solo })}
                    className={`rounded-full px-2.5 py-1 text-xs font-medium ${
                      t.solo
                        ? "bg-emerald-600 text-white"
                        : "border border-neutral-300 text-neutral-600 dark:border-neutral-700 dark:text-neutral-300"
                    }`}
                  >
                    Solo
                  </button>
                  <button
                    type="button"
                    aria-pressed={t.mute}
                    onClick={() => void updateTrack(t.id, { mute: !t.mute })}
                    className={`rounded-full px-2.5 py-1 text-xs font-medium ${
                      t.mute
                        ? "bg-red-500/90 text-white"
                        : "border border-neutral-300 text-neutral-600 dark:border-neutral-700 dark:text-neutral-300"
                    }`}
                  >
                    Mute
                  </button>
                  <button
                    type="button"
                    onClick={() => void removeTrack(t.id)}
                    aria-label={`Remove ${t.file_name}`}
                    className="text-neutral-500 dark:text-neutral-400 hover:text-red-500"
                  >
                    ×
                  </button>
                </div>

                <TrackLane trackId={t.id} totalFrames={Math.round(t.duration_secs * t.sample_rate)} />

                <div className="flex flex-wrap items-center gap-4 text-xs text-neutral-500 dark:text-neutral-400">
                  <label className="flex items-center gap-2">
                    <span className="w-10 shrink-0">Gain</span>
                    <input
                      type="range"
                      min={GAIN_MIN}
                      max={GAIN_MAX}
                      step={0.5}
                      value={t.gain_db}
                      onChange={(e) => void updateTrack(t.id, { gain_db: Number(e.target.value) })}
                      aria-label={`${t.file_name} gain`}
                      className="h-1.5 w-40 accent-emerald-600"
                    />
                    <span className="w-16 shrink-0 tabular-nums">{t.gain_db.toFixed(1)} dB</span>
                  </label>

                  {t.tag === "music" && (
                    <>
                      <label className="flex items-center gap-1.5">
                        <input
                          type="checkbox"
                          checked={t.duck_enabled}
                          onChange={(e) => void updateTrack(t.id, { duck_enabled: e.target.checked })}
                        />
                        Duck under speech
                      </label>
                      {t.duck_enabled && (
                        <label className="flex items-center gap-2">
                          <span className="w-14 shrink-0">Amount</span>
                          <input
                            type="range"
                            min={0}
                            max={36}
                            step={1}
                            value={t.duck_amount_db}
                            onChange={(e) =>
                              void updateTrack(t.id, { duck_amount_db: Number(e.target.value) })
                            }
                            aria-label={`${t.file_name} ducking amount`}
                            className="h-1.5 w-32 accent-emerald-600"
                          />
                          <span className="w-14 shrink-0 tabular-nums">
                            {t.duck_amount_db.toFixed(0)} dB
                          </span>
                        </label>
                      )}
                    </>
                  )}
                </div>
              </div>
            ))}
          </div>

          <div className="flex flex-wrap items-center gap-3">
            <button
              type="button"
              onClick={() => void handleAlign()}
              disabled={tracks.length < 2 || aligning}
              className="rounded-lg border border-neutral-300 px-4 py-2 text-sm font-medium text-neutral-600 hover:bg-neutral-100 disabled:cursor-not-allowed disabled:opacity-40 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
            >
              {aligning ? "Aligning…" : "Align tracks"}
            </button>
            <button
              type="button"
              onClick={() => void handleMix()}
              disabled={mixing}
              className="rounded-lg bg-emerald-600 px-4 py-2 text-sm font-semibold text-white hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
            >
              {mixing ? "Mixing…" : "Mix tracks"}
            </button>
            {mixError && <span className="text-xs text-red-500">{mixError}</span>}
          </div>
        </>
      )}

      {media && (
        <section className="flex flex-col gap-4 border-t border-neutral-200 pt-5 dark:border-neutral-800 lg:flex-row">
          <div className="flex min-w-0 flex-1 flex-col gap-3">
            <div className="flex items-baseline justify-between gap-4">
              <span className="truncate text-sm font-medium" title={fileName ?? ""}>
                {fileName}
              </span>
              <span className="shrink-0 text-xs text-neutral-500 dark:text-neutral-400">{formatTime(media.duration_secs)}</span>
            </div>
            <Waveform
              totalFrames={totalFrames}
              sampleRate={media.sample_rate}
              isPlaying={isPlaying}
              source={abSource}
              onSeek={() => {}}
              onReachedEnd={() => {}}
            />
            <div className="flex flex-wrap items-center gap-3">
              {isPlaying ? (
                <button
                  type="button"
                  onClick={onPause}
                  className="rounded-lg bg-neutral-900 px-5 py-2 text-sm font-medium text-white dark:bg-white dark:text-neutral-900"
                >
                  Pause
                </button>
              ) : (
                <button
                  type="button"
                  onClick={onPlay}
                  className="rounded-lg bg-emerald-600 px-5 py-2 text-sm font-medium text-white hover:bg-emerald-500"
                >
                  Play
                </button>
              )}
              <button
                type="button"
                onClick={onToggleAb}
                disabled={!report}
                aria-pressed={abSource === "processed"}
                className={`rounded-lg border px-4 py-2 text-sm font-medium transition-colors disabled:cursor-not-allowed disabled:opacity-40 ${
                  abSource === "processed"
                    ? "border-emerald-500 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
                    : "border-neutral-300 text-neutral-600 dark:border-neutral-700 dark:text-neutral-300"
                }`}
              >
                A/B: {abSource === "processed" ? "Mastered" : "Original"}
              </button>
            </div>
          </div>

          <aside className="w-full shrink-0 lg:w-[380px]">
            <RightPanel
              activeTab={activeTab}
              onTabChange={onTabChange}
              masterContent={
                <MasterPanel
                  hasMedia={!!media}
                  tier={tier}
                  preset={preset}
                  presets={presets}
                  onTierChange={onTierChange}
                  onPresetChange={onPresetChange}
                  onMaster={onMaster}
                  mastering={mastering}
                  progressVerb={progressVerb}
                  error={masterError}
                  report={report}
                  dirty={dirty}
                  advanced={advanced}
                  onToggleAdvanced={onToggleAdvanced}
                  onToggleModule={onToggleModule}
                  onModuleStrength={onModuleStrength}
                  onFix={onFix}
                />
              }
              exportContent={<ExportPanel canExport={!!report} sourcePath={sourcePath} />}
            />
          </aside>
        </section>
      )}
    </main>
  );
}
