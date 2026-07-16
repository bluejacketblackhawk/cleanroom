import { useEffect, useState } from "react";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import {
  clipStudioRender,
  onClipProgress,
  type ClipRenderResult,
  type MasterReport,
  type MediaSummary,
} from "../api";
import { formatTime } from "../lib/format";
import { usePlayhead } from "../lib/usePlayhead";

interface ClipStudioScreenProps {
  media: MediaSummary | null;
  fileName: string | null;
  sourcePath: string | null;
  isPlaying: boolean;
  report: MasterReport | null;
}

const ASPECTS: { id: string; label: string; hint: string }[] = [
  { id: "9:16", label: "9:16", hint: "Reels / Shorts / TikTok" },
  { id: "1:1", label: "1:1", hint: "Square feed post" },
  { id: "16:9", label: "16:9", hint: "YouTube / widescreen" },
];

const CAPTION_STYLES: { id: string; label: string }[] = [
  { id: "clean", label: "Clean" },
  { id: "bold", label: "Bold" },
  { id: "minimal", label: "Minimal" },
];

const BACKGROUNDS: { id: string; label: string }[] = [
  { id: "waveform", label: "Waveform" },
  { id: "color", label: "Colour" },
  { id: "cover_art", label: "Cover art" },
];

function defaultClipDestination(sourcePath: string | null): string {
  if (!sourcePath) return "clip.mp4";
  const sep = sourcePath.includes("\\") ? "\\" : "/";
  const parts = sourcePath.split(/[\\/]/);
  const fileName = parts.pop() ?? sourcePath;
  const dir = parts.join(sep);
  const stem = fileName.replace(/\.[^./\\]+$/, "");
  return dir ? `${dir}${sep}${stem}_clip.mp4` : `${stem}_clip.mp4`;
}

/**
 * Clip Studio (04 §Clip Studio, M4): pick a range of the open file, choose aspect/caption
 * style/background/title, and render a real MP4 via the ffmpeg sidecar. Target: a
 * shareable clip in under 60 seconds of user effort — so every control has a sane default
 * and "Render" is reachable without touching anything else.
 */
export default function ClipStudioScreen({
  media,
  fileName,
  sourcePath,
  isPlaying,
  report,
}: ClipStudioScreenProps) {
  const currentSeconds = usePlayhead(media?.sample_rate ?? 0, isPlaying);

  const [startSecs, setStartSecs] = useState(0);
  const [endSecs, setEndSecs] = useState(0);
  const [aspect, setAspect] = useState("9:16");
  const [captionStyle, setCaptionStyle] = useState("clean");
  const [captionsEnabled, setCaptionsEnabled] = useState(true);
  const [backgroundKind, setBackgroundKind] = useState("waveform");
  const [backgroundColor, setBackgroundColor] = useState("#101014");
  const [coverArtPath, setCoverArtPath] = useState("");
  const [title, setTitle] = useState("");
  const [destPath, setDestPath] = useState(() => defaultClipDestination(sourcePath));
  const [rendering, setRendering] = useState(false);
  const [progress, setProgress] = useState(0);
  const [result, setResult] = useState<ClipRenderResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  // A freshly opened file resets the range/destination to sensible defaults (mirrors how
  // the Export tab resets its rows on a new source — see ExportPanel).
  useEffect(() => {
    setStartSecs(0);
    setEndSecs(Math.min(60, media?.duration_secs ?? 0));
    setDestPath(defaultClipDestination(sourcePath));
    setResult(null);
    setError(null);
    setProgress(0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sourcePath]);

  useEffect(() => {
    const unlisten = onClipProgress(({ fraction }) => setProgress(fraction));
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, []);

  if (!media) {
    return (
      <main className="flex flex-1 items-center justify-center p-6">
        <p className="max-w-sm text-center text-sm text-neutral-500 dark:text-neutral-400">
          Open a file from Master first — Clip Studio works on whatever is currently loaded.
        </p>
      </main>
    );
  }

  const rangeValid = endSecs > startSecs && startSecs >= 0 && endSecs <= media.duration_secs + 0.01;
  const clipLength = Math.max(0, endSecs - startSecs);

  const handleRender = async () => {
    if (!rangeValid || rendering) return;
    setRendering(true);
    setProgress(0);
    setResult(null);
    setError(null);
    try {
      const outcome = await clipStudioRender({
        range: { start_secs: startSecs, end_secs: endSecs },
        aspect,
        caption_style: captionStyle,
        captions_enabled: captionsEnabled,
        background: {
          kind: backgroundKind,
          color: backgroundColor,
          cover_art_path: backgroundKind === "cover_art" ? coverArtPath : null,
        },
        title,
        out_path: destPath,
      });
      setResult(outcome);
      if (!outcome.ok) setError(outcome.message ?? "Rendering failed.");
    } catch (e) {
      setError(typeof e === "string" ? e : "Rendering failed.");
    } finally {
      setRendering(false);
    }
  };

  return (
    <main className="flex flex-1 flex-col gap-5 overflow-y-auto p-6 lg:flex-row">
      <section className="flex min-w-0 flex-1 flex-col gap-4">
        <div className="flex items-baseline justify-between gap-4">
          <span className="truncate text-sm font-medium" title={fileName ?? ""}>
            {fileName}
          </span>
          <span className="shrink-0 text-xs text-neutral-500 dark:text-neutral-400">
            {report ? "Using the mastered audio" : "Using the original audio — Master it first for a cleaner clip"}
          </span>
        </div>

        <div className="flex flex-col gap-2 rounded-lg border border-neutral-200 p-3 dark:border-neutral-800">
          <span className="text-xs font-medium uppercase tracking-wide text-neutral-500 dark:text-neutral-400">
            Range
          </span>
          <div className="flex flex-wrap items-end gap-3">
            <label className="flex flex-col gap-1">
              <span className="text-xs text-neutral-500 dark:text-neutral-400">Start (s)</span>
              <input
                type="number"
                min={0}
                step={0.1}
                value={startSecs}
                onChange={(e) => setStartSecs(Number(e.target.value))}
                className="w-24 rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
              />
            </label>
            <button
              type="button"
              onClick={() => setStartSecs(currentSeconds)}
              className="rounded-md border border-neutral-300 px-2 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
            >
              Mark at playhead
            </button>
            <label className="flex flex-col gap-1">
              <span className="text-xs text-neutral-500 dark:text-neutral-400">End (s)</span>
              <input
                type="number"
                min={0}
                step={0.1}
                value={endSecs}
                onChange={(e) => setEndSecs(Number(e.target.value))}
                className="w-24 rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
              />
            </label>
            <button
              type="button"
              onClick={() => setEndSecs(currentSeconds)}
              className="rounded-md border border-neutral-300 px-2 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
            >
              Mark at playhead
            </button>
            <span className="text-xs text-neutral-500 dark:text-neutral-400">
              {rangeValid ? `${formatTime(clipLength)} clip` : "Pick a valid range within the file"}
            </span>
          </div>
        </div>

        <div className="flex flex-col gap-1.5">
          <span className="text-xs font-medium uppercase tracking-wide text-neutral-500 dark:text-neutral-400">Aspect</span>
          <div role="radiogroup" aria-label="Aspect ratio" className="grid grid-cols-3 gap-1 rounded-lg bg-neutral-100 p-1 dark:bg-neutral-900">
            {ASPECTS.map((a) => (
              <button
                key={a.id}
                type="button"
                role="radio"
                aria-checked={aspect === a.id}
                title={a.hint}
                onClick={() => setAspect(a.id)}
                className={`rounded-md px-2 py-1.5 text-xs font-medium transition-colors ${
                  aspect === a.id
                    ? "bg-white text-neutral-900 shadow-sm dark:bg-neutral-700 dark:text-white"
                    : "text-neutral-500 dark:text-neutral-400 hover:text-neutral-800 dark:hover:text-neutral-200"
                }`}
              >
                {a.label}
              </button>
            ))}
          </div>
        </div>

        <div className="flex flex-col gap-1.5">
          <span className="text-xs font-medium uppercase tracking-wide text-neutral-500 dark:text-neutral-400">Background</span>
          <div role="radiogroup" aria-label="Background" className="grid grid-cols-3 gap-1 rounded-lg bg-neutral-100 p-1 dark:bg-neutral-900">
            {BACKGROUNDS.map((b) => (
              <button
                key={b.id}
                type="button"
                role="radio"
                aria-checked={backgroundKind === b.id}
                onClick={() => setBackgroundKind(b.id)}
                className={`rounded-md px-2 py-1.5 text-xs font-medium transition-colors ${
                  backgroundKind === b.id
                    ? "bg-white text-neutral-900 shadow-sm dark:bg-neutral-700 dark:text-white"
                    : "text-neutral-500 dark:text-neutral-400 hover:text-neutral-800 dark:hover:text-neutral-200"
                }`}
              >
                {b.label}
              </button>
            ))}
          </div>
          {(backgroundKind === "waveform" || backgroundKind === "color") && (
            <label className="flex items-center gap-2 text-xs text-neutral-500 dark:text-neutral-400">
              <span>{backgroundKind === "waveform" ? "Waveform colour" : "Background colour"}</span>
              <input
                type="color"
                value={backgroundColor}
                onChange={(e) => setBackgroundColor(e.target.value)}
                className="h-7 w-12 cursor-pointer rounded border border-neutral-300 dark:border-neutral-700"
              />
            </label>
          )}
          {backgroundKind === "cover_art" && (
            <label className="flex flex-col gap-1">
              <span className="text-xs text-neutral-500 dark:text-neutral-400">Cover art image path</span>
              <input
                type="text"
                value={coverArtPath}
                onChange={(e) => setCoverArtPath(e.target.value)}
                placeholder="Path to a cover image (jpg or png)"
                className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
              />
            </label>
          )}
        </div>

        <div className="flex flex-col gap-1.5">
          <div className="flex items-center justify-between">
            <span className="text-xs font-medium uppercase tracking-wide text-neutral-500 dark:text-neutral-400">Captions</span>
            <label className="flex items-center gap-1.5 text-xs text-neutral-600 dark:text-neutral-300">
              <input
                type="checkbox"
                checked={captionsEnabled}
                onChange={(e) => setCaptionsEnabled(e.target.checked)}
              />
              Show captions
            </label>
          </div>
          {captionsEnabled && (
            <div role="radiogroup" aria-label="Caption style" className="grid grid-cols-3 gap-1 rounded-lg bg-neutral-100 p-1 dark:bg-neutral-900">
              {CAPTION_STYLES.map((c) => (
                <button
                  key={c.id}
                  type="button"
                  role="radio"
                  aria-checked={captionStyle === c.id}
                  onClick={() => setCaptionStyle(c.id)}
                  className={`rounded-md px-2 py-1.5 text-xs font-medium transition-colors ${
                    captionStyle === c.id
                      ? "bg-white text-neutral-900 shadow-sm dark:bg-neutral-700 dark:text-white"
                      : "text-neutral-500 dark:text-neutral-400 hover:text-neutral-800 dark:hover:text-neutral-200"
                  }`}
                >
                  {c.label}
                </button>
              ))}
            </div>
          )}
        </div>

        <label className="flex flex-col gap-1">
          <span className="text-xs font-medium uppercase tracking-wide text-neutral-500 dark:text-neutral-400">Title text</span>
          <input
            type="text"
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            placeholder="Optional — burned in at the top of the clip"
            className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-sm dark:border-neutral-700 dark:bg-neutral-900"
          />
        </label>

        <label className="flex flex-col gap-1">
          <span className="text-xs font-medium uppercase tracking-wide text-neutral-500 dark:text-neutral-400">Destination</span>
          <input
            type="text"
            value={destPath}
            onChange={(e) => setDestPath(e.target.value)}
            className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
          />
        </label>

        <div className="flex items-center gap-3">
          <button
            type="button"
            onClick={() => void handleRender()}
            disabled={!rangeValid || rendering}
            className="rounded-lg bg-emerald-600 px-5 py-2.5 text-sm font-semibold text-white transition-colors hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {rendering ? "Rendering…" : "Render MP4"}
          </button>
          {rendering && (
            <div className="h-1.5 w-40 overflow-hidden rounded-full bg-neutral-200 dark:bg-neutral-800">
              <div
                className="h-full bg-emerald-500 transition-[width]"
                style={{ width: `${Math.round(progress * 100)}%` }}
              />
            </div>
          )}
        </div>

        {error && <p className="text-xs text-red-500">{error}</p>}
        {result?.ok && (
          <div className="flex flex-col gap-1 rounded-lg border border-emerald-500/40 bg-emerald-500/5 p-3 text-xs text-emerald-700 dark:text-emerald-300">
            <div className="flex items-center justify-between gap-2">
              <span>Rendered {result.path}</span>
              <button
                type="button"
                onClick={() => void revealItemInDir(result.path)}
                className="font-medium hover:underline"
              >
                Open folder
              </button>
            </div>
            {result.seam_notes.map((note, i) => (
              <p key={i} className="text-neutral-500 dark:text-neutral-400">
                {note}
              </p>
            ))}
          </div>
        )}
      </section>
    </main>
  );
}
