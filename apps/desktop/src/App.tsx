import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { resolveResource } from "@tauri-apps/api/path";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import {
  appInfo,
  batchCheckRecovery,
  batchDismissRecovery,
  frontendReady,
  master,
  onOpenFile,
  openMedia,
  pause,
  play,
  playbackPosition,
  seek,
  setAb,
  type AbSource,
  type AppInfo,
  type MasterReport,
  type MediaSummary,
  type RecoveryEntry,
} from "./api";
import Waveform from "./Waveform";
import RightPanel, { type PanelTab } from "./components/RightPanel";
import MasterPanel from "./components/MasterPanel";
import ExportPanel from "./components/ExportPanel";
import NavRail from "./components/NavRail";
import Onboarding from "./components/Onboarding";
import BatchScreen from "./screens/BatchScreen";
import WatchScreen from "./screens/WatchScreen";
import PresetsScreen from "./screens/PresetsScreen";
import ModelsScreen from "./screens/ModelsScreen";
import SettingsScreen from "./screens/SettingsScreen";
import TranscriptScreen from "./screens/TranscriptScreen";
import MetadataScreen from "./screens/MetadataScreen";
import MultitrackScreen from "./screens/MultitrackScreen";
import ClipStudioScreen from "./screens/ClipStudioScreen";
import RecordingGuardScreen from "./screens/RecordingGuardScreen";
import { formatTime } from "./lib/format";
import { DEFAULT_PRESET, DEFAULT_TIER, PROGRESS_VERBS, type Tier } from "./lib/presets";
import { usePresets } from "./lib/usePresets";
import type { View } from "./lib/nav";

/** Persisted across launches so onboarding only ever shows once (04 §S9 "first run").
 * Deliberately in `localStorage`, not a Rust-side setting — it's pure UI state with no
 * privacy weight and no reason to round-trip through the engine. */
const ONBOARDING_STORAGE_KEY = "anvil.onboarding.completed";

/** Small labeled wrapper around a waveform lane for the stacked before/after view — an
 * emerald ring marks whichever lane is currently the A/B-active one. */
function WaveLane({
  label,
  active,
  children,
}: {
  label: string;
  active: boolean;
  children: ReactNode;
}) {
  return (
    <div
      className={`relative rounded-lg ring-1 transition-colors ${
        active ? "ring-emerald-500" : "ring-transparent"
      }`}
    >
      <span className="pointer-events-none absolute left-2 top-1.5 z-10 rounded bg-neutral-900/70 px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide text-white dark:bg-white/80 dark:text-neutral-900">
        {label}
      </span>
      {children}
    </div>
  );
}

export default function App() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [view, setView] = useState<View>("master");
  const [media, setMedia] = useState<MediaSummary | null>(null);
  const [fileName, setFileName] = useState<string | null>(null);
  const [sourcePath, setSourcePath] = useState<string | null>(null);
  const [isPlaying, setIsPlaying] = useState(false);
  const [dragActive, setDragActive] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { presets, refresh: refreshPresets } = usePresets();

  // Master tab state.
  const [tier, setTier] = useState<Tier>(DEFAULT_TIER);
  const [preset, setPreset] = useState<string>(DEFAULT_PRESET);
  const [report, setReport] = useState<MasterReport | null>(null);
  const [mastering, setMastering] = useState(false);
  const [masterError, setMasterError] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);
  const [advanced, setAdvanced] = useState(false);
  const [progressVerb, setProgressVerb] = useState(PROGRESS_VERBS[0]);

  // A/B + waveform view state.
  const [abSource, setAbSourceState] = useState<AbSource>("original");
  const [viewMode, setViewMode] = useState<"stacked" | "overlay">("stacked");

  // Onboarding (04 §S9) + batch crash-recovery banner (05 §M5.F).
  const [showOnboarding, setShowOnboarding] = useState(false);
  const [recovery, setRecovery] = useState<RecoveryEntry[]>([]);

  // Right panel tab.
  const [activeTab, setActiveTab] = useState<PanelTab>("master");

  useEffect(() => {
    // Proves the Rust <-> UI IPC bridge end-to-end (M0).
    appInfo()
      .then(setInfo)
      .catch(() => setInfo(null));
  }, []);

  useEffect(() => {
    // First run only (04 §S9) — a completed/skipped onboarding never shows again.
    if (window.localStorage.getItem(ONBOARDING_STORAGE_KEY) !== "1") {
      setShowOnboarding(true);
    }
    // Whatever a previous session's batch queue still had `queued`/`running` when the app
    // last exited (05 §M5.F crash recovery) — empty in the normal, clean-exit case.
    batchCheckRecovery()
      .then(setRecovery)
      .catch(() => setRecovery([]));
  }, []);

  const dismissOnboarding = useCallback(() => {
    window.localStorage.setItem(ONBOARDING_STORAGE_KEY, "1");
    setShowOnboarding(false);
  }, []);

  const loadPath = useCallback(async (path: string) => {
    setBusy(true);
    setError(null);
    try {
      const summary = await openMedia(path);
      setMedia(summary);
      setFileName(path.split(/[\\/]/).pop() ?? path);
      setSourcePath(path);
      setIsPlaying(false);
      // A fresh file clears any previous mastering result — the engine did the same.
      setReport(null);
      setDirty(false);
      setAdvanced(false);
      setAbSourceState("original");
      setMasterError(null);
      setActiveTab("master");
    } catch (e) {
      setMedia(null);
      setError(typeof e === "string" ? e : "Could not open that file.");
    } finally {
      setBusy(false);
    }
  }, []);

  const handleOpenDialog = useCallback(async () => {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [
        {
          name: "Audio & video",
          extensions: [
            "wav", "mp3", "flac", "m4a", "aac", "ogg", "opus", "aiff", "wma",
            "mp4", "mov", "mkv", "webm", "avi",
          ],
        },
        { name: "All files", extensions: ["*"] },
      ],
    });
    if (typeof selected === "string") {
      setView("master");
      void loadPath(selected);
    }
  }, [loadPath]);

  const handleTryDemo = useCallback(async () => {
    dismissOnboarding();
    try {
      // Resolves through Tauri's resource resolver — the file ships via
      // `tauri.conf.json`'s `bundle.resources` (both the NSIS install and the portable
      // zip carry it), so this works identically installed or portable, online or off.
      const demoPath = await resolveResource("resources/demo/bad-recording-example.wav");
      await loadPath(demoPath);
    } catch (e) {
      setError(
        typeof e === "string" ? e : "Couldn't find the bundled demo file — drop your own instead.",
      );
    }
  }, [dismissOnboarding, loadPath]);

  const dismissRecovery = useCallback(() => {
    setRecovery([]);
    batchDismissRecovery().catch(() => {
      /* best-effort — the banner is already hidden either way */
    });
  }, []);

  // Tauri v2 drag-and-drop: the payload carries OS file paths, which we hand to Rust.
  // Only the Master screen owns file drops — Batch/Watch/Presets/Models each register
  // their own drop handling (or none) while they're the active view, so a drop over the
  // Batch screen doesn't also try to open it as a single Master file.
  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      if (view !== "master") return;
      const p = event.payload;
      if (p.type === "enter" || p.type === "over") {
        setDragActive(true);
      } else if (p.type === "leave") {
        setDragActive(false);
      } else if (p.type === "drop") {
        setDragActive(false);
        const first = p.paths[0];
        if (first) void loadPath(first);
      }
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, [loadPath, view]);

  // OS "Open With" / file-association / `open -a Cleanroom <file>` opens are routed from
  // Rust as an `open-file` event carrying the absolute path (FIX 1). Handle it exactly like
  // a drop: jump to the Master screen and load the file. We register the listener *first*,
  // then call `frontendReady()`, so any open that arrived during a cold launch (queued
  // Rust-side until now) flushes into a live listener instead of being missed.
  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    let disposed = false;
    void onOpenFile((path) => {
      setView("master");
      void loadPath(path);
    }).then((fn) => {
      if (disposed) {
        fn();
        return;
      }
      unlisten = fn;
      void frontendReady();
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [loadPath]);

  // Cmd/Ctrl+O opens the file picker — drag-drop alone isn't discoverable or keyboard-
  // accessible (GUI-test finding: "drag-only ingestion, a11y-relevant").
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && (e.key === "o" || e.key === "O")) {
        e.preventDefault();
        void handleOpenDialog();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [handleOpenDialog]);

  const totalFrames = useMemo(
    () => (media ? Math.round(media.duration_secs * media.sample_rate) : 0),
    [media],
  );

  const handlePlay = useCallback(async () => {
    await play();
    setIsPlaying(true);
  }, []);

  const handlePause = useCallback(async () => {
    await pause();
    setIsPlaying(false);
  }, []);

  const handleReachedEnd = useCallback(() => {
    setIsPlaying(false);
  }, []);

  const handleClose = useCallback(() => {
    setMedia(null);
    setFileName(null);
    setSourcePath(null);
    setIsPlaying(false);
    setReport(null);
    setDirty(false);
    setAdvanced(false);
    setAbSourceState("original");
    setActiveTab("master");
    setMasterError(null);
  }, []);

  // Cycle "Removing noise… Balancing voices…" while a Master run is in flight
  // (microcopy rules: processing verbs double as education).
  useEffect(() => {
    if (!mastering) return;
    let i = 0;
    setProgressVerb(PROGRESS_VERBS[0]);
    const id = window.setInterval(() => {
      i = (i + 1) % PROGRESS_VERBS.length;
      setProgressVerb(PROGRESS_VERBS[i]);
    }, 550);
    return () => window.clearInterval(id);
  }, [mastering]);

  const handleMaster = useCallback(
    async (opts?: { tier?: Tier; preset?: string }) => {
      if (!media) return;
      const useTier = opts?.tier ?? tier;
      const usePreset = opts?.preset ?? preset;
      setMastering(true);
      setMasterError(null);
      try {
        const r = await master(usePreset, useTier);
        setReport(r);
        setDirty(false);
        // The backend already switched the engine to the mastered take.
        setAbSourceState("processed");
        if (opts?.tier) setTier(opts.tier);
        if (opts?.preset) setPreset(opts.preset);
      } catch (e) {
        setMasterError(typeof e === "string" ? e : "Mastering failed.");
      } finally {
        setMastering(false);
      }
    },
    [media, preset, tier],
  );

  const handleTierChange = useCallback(
    (t: Tier) => {
      setTier(t);
      if (report) setDirty(true);
    },
    [report],
  );

  const handlePresetChange = useCallback(
    (p: string) => {
      setPreset(p);
      if (report) setDirty(true);
    },
    [report],
  );

  const handleToggleModule = useCallback((i: number) => {
    setReport((r) => {
      if (!r) return r;
      const modules = r.modules.map((m, idx) =>
        idx === i ? { ...m, engaged: !m.engaged } : m,
      );
      return { ...r, modules };
    });
    setDirty(true);
  }, []);

  const handleModuleStrength = useCallback((i: number, value: number) => {
    setReport((r) => {
      if (!r) return r;
      const modules = r.modules.map((m, idx) => (idx === i ? { ...m, strength: value } : m));
      return { ...r, modules };
    });
    setDirty(true);
  }, []);

  const handleFix = useCallback(
    (code: string) => {
      if (code === "switch_to_studio") {
        // The chip *is* the fix — apply it immediately rather than making the user
        // change the tier selector and click Master again.
        void handleMaster({ tier: "studio" });
      }
    },
    [handleMaster],
  );

  // Best-effort A/B switch: local state flips immediately for a responsive toggle, the
  // Tauri call reconciles the actual engine in the background.
  const applyAbSource = useCallback((source: AbSource) => {
    setAbSourceState(source);
    setAb(source).catch((e) => {
      console.error("A/B switch failed", e);
    });
  }, []);

  const toggleAb = useCallback(() => {
    if (!report) return;
    applyAbSource(abSource === "original" ? "processed" : "original");
  }, [abSource, report, applyAbSource]);

  const handleSeekRelative = useCallback(
    async (deltaFrames: number) => {
      if (!media || totalFrames <= 0) return;
      const current = await playbackPosition();
      const next = Math.max(0, Math.min(totalFrames, current + deltaFrames));
      await seek(next);
    },
    [media, totalFrames],
  );

  // Keyboard map (04 §Keyboard map): Space play/pause, A A/B, M Master, E Export tab,
  // arrows seek 5 s (Shift = 30 s). Ignored while a form field has focus.
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      const tag = target?.tagName;
      const isFormField =
        tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || target?.isContentEditable;
      if (isFormField || !media || view !== "master") return;

      switch (e.key) {
        case " ":
          e.preventDefault();
          void (isPlaying ? handlePause() : handlePlay());
          break;
        case "a":
        case "A":
          e.preventDefault();
          toggleAb();
          break;
        case "m":
        case "M":
          e.preventDefault();
          void handleMaster();
          break;
        case "e":
        case "E":
          e.preventDefault();
          setActiveTab("export");
          break;
        case "ArrowLeft":
        case "ArrowRight": {
          e.preventDefault();
          const deltaSecs = (e.shiftKey ? 30 : 5) * (e.key === "ArrowLeft" ? -1 : 1);
          const deltaFrames = Math.round(deltaSecs * (media.sample_rate || 48_000));
          void handleSeekRelative(deltaFrames);
          break;
        }
        default:
          break;
      }
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [media, view, isPlaying, handlePlay, handlePause, toggleAb, handleMaster, handleSeekRelative]);

  return (
    <div className="flex min-h-screen flex-col bg-neutral-50 text-neutral-900 dark:bg-neutral-950 dark:text-neutral-100">
      <Onboarding open={showOnboarding} onSkip={dismissOnboarding} onTryDemo={() => void handleTryDemo()} />

      <header className="flex items-center justify-between border-b border-neutral-200 px-6 py-4 dark:border-neutral-800">
        <div className="flex items-baseline gap-2">
          <span className="text-lg font-semibold tracking-tight">Cleanroom</span>
          <span className="text-xs text-neutral-500 dark:text-neutral-400">
            {info ? `v${info.version}` : "…"}
          </span>
        </div>
        <span className="rounded-full border border-emerald-500/40 px-2 py-0.5 text-xs text-emerald-600 dark:text-emerald-400">
          100% local
        </span>
      </header>

      {recovery.length > 0 && (
        <div
          role="status"
          className="flex flex-wrap items-center justify-between gap-3 border-b border-amber-300/60 bg-amber-50 px-6 py-2 text-xs text-amber-900 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-200"
        >
          <span>
            {recovery.length === 1
              ? "1 file from an earlier session didn't finish mastering."
              : `${recovery.length} files from an earlier session didn't finish mastering.`}{" "}
            Nothing was lost except the render itself — re-drop them on the Batch tab when
            you're ready.
          </span>
          <div className="flex shrink-0 gap-2">
            <button
              type="button"
              onClick={() => setView("batch")}
              className="rounded-md border border-amber-400 px-2 py-1 font-medium hover:bg-amber-100 dark:border-amber-500/50 dark:hover:bg-amber-500/20"
            >
              Go to Batch
            </button>
            <button
              type="button"
              onClick={dismissRecovery}
              className="rounded-md px-2 py-1 font-medium hover:bg-amber-100 dark:hover:bg-amber-500/20"
            >
              Dismiss
            </button>
          </div>
        </div>
      )}

      <div className="flex flex-1">
        <NavRail active={view} onChange={setView} />

        {view === "settings" && <SettingsScreen info={info} />}
        {view === "batch" && <BatchScreen presets={presets} />}
        {view === "watch" && <WatchScreen presets={presets} />}
        {view === "presets" && <PresetsScreen presets={presets} onChanged={refreshPresets} />}
        {view === "models" && <ModelsScreen />}
        {view === "transcript" && (
          <TranscriptScreen
            media={media}
            fileName={fileName}
            sourcePath={sourcePath}
            totalFrames={totalFrames}
            isPlaying={isPlaying}
            onPlay={() => void handlePlay()}
            onPause={() => void handlePause()}
          />
        )}

        {view === "metadata" && (
          <MetadataScreen media={media} fileName={fileName} sourcePath={sourcePath} />
        )}

        {view === "multitrack" && (
          <MultitrackScreen
            media={media}
            fileName={fileName}
            sourcePath={sourcePath}
            totalFrames={totalFrames}
            isPlaying={isPlaying}
            onPlay={() => void handlePlay()}
            onPause={() => void handlePause()}
            abSource={abSource}
            onToggleAb={toggleAb}
            activeTab={activeTab}
            onTabChange={setActiveTab}
            tier={tier}
            preset={preset}
            presets={presets}
            onTierChange={handleTierChange}
            onPresetChange={handlePresetChange}
            onMaster={() => void handleMaster()}
            mastering={mastering}
            progressVerb={progressVerb}
            masterError={masterError}
            report={report}
            dirty={dirty}
            advanced={advanced}
            onToggleAdvanced={() => setAdvanced((a) => !a)}
            onToggleModule={handleToggleModule}
            onModuleStrength={handleModuleStrength}
            onFix={handleFix}
            onLoadPath={loadPath}
          />
        )}

        {view === "clip_studio" && (
          <ClipStudioScreen
            media={media}
            fileName={fileName}
            sourcePath={sourcePath}
            isPlaying={isPlaying}
            report={report}
          />
        )}

        {view === "guard" && <RecordingGuardScreen />}

        {view === "master" && (
          <main className="flex flex-1 flex-col gap-6 p-6 lg:flex-row">
            {!media ? (
              <div className="flex flex-1 items-center justify-center">
                <div
                  className={`flex w-full max-w-xl flex-col items-center gap-4 rounded-2xl border-2 border-dashed p-12 text-center transition-colors ${
                    dragActive
                      ? "border-emerald-500 bg-emerald-500/5"
                      : "border-neutral-300 dark:border-neutral-700"
                  }`}
                >
                  <p className="text-base font-medium">
                    {busy ? "Opening…" : "Drop an audio or video file"}
                  </p>
                  <p className="text-sm text-neutral-500 dark:text-neutral-400">
                    {error ?? "It stays on your machine. Nothing is uploaded."}
                  </p>
                  <p className="text-xs text-neutral-500 dark:text-neutral-400">
                    WAV, MP3, FLAC, M4A, MP4, MOV, MKV — it all works.
                  </p>
                  <button
                    type="button"
                    onClick={() => void handleOpenDialog()}
                    className="mt-1 rounded-lg border border-neutral-300 px-4 py-2 text-sm font-medium text-neutral-700 transition-colors hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-200 dark:hover:bg-neutral-800"
                  >
                    Choose a file…
                  </button>
                </div>
              </div>
            ) : (
              <>
            <section className="flex min-w-0 flex-1 flex-col gap-4">
              <div className="flex items-baseline justify-between gap-4">
                <span className="truncate text-sm font-medium" title={fileName ?? ""}>
                  {fileName}
                </span>
                <span className="shrink-0 text-xs text-neutral-500 dark:text-neutral-400">
                  {media.channels === 1 ? "mono" : `${media.channels} ch`} ·{" "}
                  {(media.sample_rate / 1000).toFixed(1)} kHz ·{" "}
                  {formatTime(media.duration_secs)}
                </span>
              </div>

              {report ? (
                <div className="flex flex-col gap-2">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">
                      Hearing:{" "}
                      <span className="text-emerald-600 dark:text-emerald-400">
                        {abSource === "processed" ? "Mastered" : "Original"}
                      </span>
                    </span>
                    <div
                      role="radiogroup"
                      aria-label="Waveform view"
                      className="flex gap-1 rounded-md bg-neutral-100 p-0.5 text-xs dark:bg-neutral-900"
                    >
                      {(["stacked", "overlay"] as const).map((mode) => (
                        <button
                          key={mode}
                          type="button"
                          role="radio"
                          aria-checked={viewMode === mode}
                          onClick={() => setViewMode(mode)}
                          className={`rounded px-2 py-1 font-medium capitalize transition-colors ${
                            viewMode === mode
                              ? "bg-white shadow-sm dark:bg-neutral-700"
                              : "text-neutral-500 dark:text-neutral-400"
                          }`}
                        >
                          {mode}
                        </button>
                      ))}
                    </div>
                  </div>

                  {viewMode === "stacked" ? (
                    <div className="flex flex-col gap-2">
                      <WaveLane label="Before" active={abSource === "original"}>
                        <Waveform
                          totalFrames={totalFrames}
                          sampleRate={media.sample_rate}
                          isPlaying={isPlaying}
                          source="original"
                          heightClassName="h-24"
                          ariaContext={formatLufsContext(report.before.integrated_lufs)}
                          onSeek={() => {}}
                          onReachedEnd={handleReachedEnd}
                        />
                      </WaveLane>
                      <WaveLane label="After" active={abSource === "processed"}>
                        <Waveform
                          totalFrames={totalFrames}
                          sampleRate={media.sample_rate}
                          isPlaying={isPlaying}
                          source="processed"
                          heightClassName="h-24"
                          ariaContext={formatLufsContext(report.after.integrated_lufs)}
                          onSeek={() => {}}
                          onReachedEnd={handleReachedEnd}
                        />
                      </WaveLane>
                    </div>
                  ) : (
                    <>
                      <Waveform
                        totalFrames={totalFrames}
                        sampleRate={media.sample_rate}
                        isPlaying={isPlaying}
                        source={abSource}
                        compareSource={abSource === "original" ? "processed" : "original"}
                        heightClassName="h-40"
                        onSeek={() => {}}
                        onReachedEnd={handleReachedEnd}
                      />
                      <div className="flex gap-3 text-[10px] text-neutral-500 dark:text-neutral-400">
                        <span className="flex items-center gap-1">
                          <span className="h-2 w-2 rounded-full bg-emerald-500" />
                          {abSource === "processed" ? "Mastered" : "Original"} (playing)
                        </span>
                        <span className="flex items-center gap-1">
                          <span className="h-2 w-2 rounded-full bg-sky-400" />
                          {abSource === "processed" ? "Original" : "Mastered"} (comparison)
                        </span>
                      </div>
                    </>
                  )}
                </div>
              ) : (
                <Waveform
                  totalFrames={totalFrames}
                  sampleRate={media.sample_rate}
                  isPlaying={isPlaying}
                  source="original"
                  onSeek={() => {}}
                  onReachedEnd={handleReachedEnd}
                />
              )}

              <div className="flex flex-wrap items-center gap-3">
                {isPlaying ? (
                  <button
                    type="button"
                    onClick={() => void handlePause()}
                    className="rounded-lg bg-neutral-900 px-5 py-2 text-sm font-medium text-white dark:bg-white dark:text-neutral-900"
                  >
                    Pause
                  </button>
                ) : (
                  <button
                    type="button"
                    onClick={() => void handlePlay()}
                    className="rounded-lg bg-emerald-600 px-5 py-2 text-sm font-medium text-white hover:bg-emerald-500"
                  >
                    Play
                  </button>
                )}
                <button
                  type="button"
                  onClick={toggleAb}
                  disabled={!report}
                  aria-pressed={abSource === "processed"}
                  title="Toggle original vs mastered (A)"
                  className={`rounded-lg border px-4 py-2 text-sm font-medium transition-colors disabled:cursor-not-allowed disabled:opacity-40 ${
                    abSource === "processed"
                      ? "border-emerald-500 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
                      : "border-neutral-300 text-neutral-600 dark:border-neutral-700 dark:text-neutral-300"
                  }`}
                >
                  A/B: {abSource === "processed" ? "Mastered" : "Original"}
                </button>
                <button
                  type="button"
                  onClick={handleClose}
                  className="rounded-lg border border-neutral-300 px-4 py-2 text-sm font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
                >
                  Close
                </button>
                {error && <span className="text-xs text-red-500">{error}</span>}
              </div>
            </section>

            <aside className="w-full shrink-0 lg:w-[380px]">
              <RightPanel
                activeTab={activeTab}
                onTabChange={setActiveTab}
                masterContent={
                  <MasterPanel
                    hasMedia={!!media}
                    tier={tier}
                    preset={preset}
                    presets={presets}
                    onTierChange={handleTierChange}
                    onPresetChange={handlePresetChange}
                    onMaster={() => void handleMaster()}
                    mastering={mastering}
                    progressVerb={progressVerb}
                    error={masterError}
                    report={report}
                    dirty={dirty}
                    advanced={advanced}
                    onToggleAdvanced={() => setAdvanced((a) => !a)}
                    onToggleModule={handleToggleModule}
                    onModuleStrength={handleModuleStrength}
                    onFix={handleFix}
                  />
                }
                exportContent={<ExportPanel canExport={!!report} sourcePath={sourcePath} />}
              />
            </aside>
              </>
            )}
          </main>
        )}
      </div>

      <footer className="flex flex-col items-center gap-1 px-6 py-3 text-center text-xs text-neutral-500 dark:text-neutral-400">
        <span>
          {info
            ? `chain v${info.chain_version} · ${info.platform}`
            : "connecting to engine…"}
        </span>
        {media && view === "master" && (
          <span>Space play/pause · A A/B · M Master · E Export · ←/→ seek (Shift = 30s)</span>
        )}
      </footer>
    </div>
  );
}

function formatLufsContext(lufs: number): string {
  return `${lufs.toFixed(1)} LUFS`;
}
