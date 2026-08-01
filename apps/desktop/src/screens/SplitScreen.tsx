import { useCallback, useEffect, useRef, useState } from "react";
import {
  appSettingsGet,
  modelsList,
  onSplitProgress,
  seek,
  splitDetect,
  splitPreview,
  splitRender,
  type MediaSummary,
  type ModelPack,
  type SplitPreview,
  type SplitRenderResult,
} from "../api";

interface SplitScreenProps {
  media: MediaSummary | null;
  fileName: string | null;
  sourcePath: string | null;
  isPlaying: boolean;
  onPlay: () => void;
  onPause: () => void;
}

const fmtTime = (secs: number) => {
  const s = Math.max(0, secs);
  const m = Math.floor(s / 60);
  const r = Math.floor(s % 60);
  return `${m}:${r.toString().padStart(2, "0")}`;
};

/** The Split screen (handoff/09 §1): read many teleprompter scripts in one take, say your
 * cut word between them, and export each script as its own mastered file — the cut word
 * (and the dead air around it) deleted entirely. */
export default function SplitScreen({
  media,
  fileName,
  sourcePath,
  isPlaying,
  onPlay,
  onPause,
}: SplitScreenProps) {
  const [cutWord, setCutWord] = useState("");
  const [savedDefault, setSavedDefault] = useState<string | null>(null);
  const [asrPacks, setAsrPacks] = useState<ModelPack[]>([]);
  const [model, setModel] = useState("whisper-base");
  const [preview, setPreview] = useState<SplitPreview | null>(null);
  const [masterEach, setMasterEach] = useState(true);
  const [outDir, setOutDir] = useState("");
  const [busy, setBusy] = useState<"idle" | "detect" | "render">("idle");
  const [progress, setProgress] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<SplitRenderResult | null>(null);
  // The preview belongs to the file it was computed from; a new file resets it.
  const lastSource = useRef<string | null>(null);

  useEffect(() => {
    appSettingsGet()
      .then((s) => {
        setSavedDefault(s.default_cut_word);
        if (s.default_cut_word) setCutWord((w) => w || s.default_cut_word || "");
        setMasterEach(s.split_master_default);
      })
      .catch(() => {
        /* defaults stay — settings are best-effort here */
      });
    modelsList()
      .then((packs) => {
        const asr = packs.filter((p) => p.kind === "asr");
        setAsrPacks(asr);
        const installed = asr.find((p) => p.installed);
        if (installed) setModel(installed.id);
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    if (sourcePath !== lastSource.current) {
      lastSource.current = sourcePath;
      setPreview(null);
      setResult(null);
      setError(null);
    }
  }, [sourcePath]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onSplitProgress((e) => setProgress(e.fraction)).then((u) => {
      unlisten = u;
    });
    return () => unlisten?.();
  }, []);

  const acceptedIndices = useCallback(
    (p: SplitPreview) => p.matches.flatMap((m, i) => (m.accepted ? [i] : [])),
    [],
  );

  const handleDetect = async () => {
    setBusy("detect");
    setError(null);
    setResult(null);
    try {
      setPreview(await splitDetect(cutWord, model));
    } catch (e) {
      setError(typeof e === "string" ? e : "Couldn't look for the cut word.");
    } finally {
      setBusy("idle");
    }
  };

  const handleToggle = async (index: number) => {
    if (!preview) return;
    const next = preview.matches.map((m, i) => (i === index ? { ...m, accepted: !m.accepted } : m));
    const indices = next.flatMap((m, i) => (m.accepted ? [i] : []));
    try {
      setPreview(await splitPreview(indices));
    } catch (e) {
      setError(typeof e === "string" ? e : "Couldn't update the preview.");
    }
  };

  const handleHear = async (start: number) => {
    // Drop the playhead two seconds before the match so the cut word lands in context.
    await seek(Math.max(0, Math.round((start - 2) * 48_000)));
    if (!isPlaying) onPlay();
  };

  const handleRender = async () => {
    if (!preview) return;
    setBusy("render");
    setError(null);
    setResult(null);
    setProgress(0);
    try {
      setResult(
        await splitRender(acceptedIndices(preview), outDir.trim() ? outDir.trim() : null, masterEach, null),
      );
    } catch (e) {
      setError(typeof e === "string" ? e : "Couldn't render the segments.");
    } finally {
      setBusy("idle");
    }
  };

  if (!media) {
    return (
      <main className="flex flex-1 items-center justify-center p-6">
        <div className="max-w-md text-center">
          <h1 className="text-lg font-semibold">Split</h1>
          <p className="mt-2 text-sm text-neutral-500 dark:text-neutral-400">
            Drop a recording here — audio or video. If you said your cut word between
            scripts (or scenes), this screen turns the one take into one clean file per
            piece, cut word deleted. Mastering each piece is optional: already-mixed
            audio comes out untouched apart from the cut.
          </p>
        </div>
      </main>
    );
  }

  const accepted = preview?.matches.filter((m) => m.accepted).length ?? 0;
  const possible = (preview?.matches.length ?? 0) - accepted;
  const installedModel = asrPacks.find((p) => p.id === model)?.installed ?? true;

  return (
    <main className="flex flex-1 flex-col gap-4 overflow-y-auto p-6">
      <div>
        <h1 className="text-lg font-semibold">Split</h1>
        <p className="mt-0.5 text-sm text-neutral-500 dark:text-neutral-400">
          {fileName ?? "This file"} → one file per script, split wherever you said the cut
          word. Everything runs on this computer.
        </p>
      </div>

      <section className="flex flex-wrap items-end gap-3 rounded-xl border border-neutral-200 p-4 dark:border-neutral-800">
        <label className="flex flex-col gap-1 text-xs font-medium">
          Cut word
          <input
            type="text"
            value={cutWord}
            onChange={(e) => setCutWord(e.target.value)}
            placeholder={savedDefault ?? "e.g. kumquat"}
            className="w-44 rounded-lg border border-neutral-300 bg-transparent px-3 py-2 text-sm dark:border-neutral-700"
          />
        </label>
        <label className="flex flex-col gap-1 text-xs font-medium">
          Transcription model
          <select
            value={model}
            onChange={(e) => setModel(e.target.value)}
            className="rounded-lg border border-neutral-300 bg-transparent px-3 py-2 text-sm dark:border-neutral-700"
          >
            {asrPacks.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
                {p.installed ? "" : " (not installed)"}
              </option>
            ))}
          </select>
        </label>
        <button
          type="button"
          onClick={() => void handleDetect()}
          disabled={busy !== "idle" || !cutWord.trim() || !installedModel}
          className="rounded-lg bg-emerald-600 px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-emerald-500 disabled:opacity-40"
        >
          {busy === "detect" ? "Listening for it…" : "Find cut words"}
        </button>
        {!installedModel && (
          <span className="text-xs text-amber-600 dark:text-amber-400">
            Install this model in Models first.
          </span>
        )}
        <p className="w-full text-xs text-neutral-500 dark:text-neutral-400">
          Pick a word you'd never say in a script — "kumquat", "flamingo". Multi-word
          phrases work, and commas separate alternatives ("furthermore, nevertheless,
          regardless" splits on any of them). Save a default in Settings so this is one
          click next time.
        </p>
      </section>

      {error && (
        <div className="rounded-lg border border-red-300 bg-red-50 px-4 py-3 text-sm text-red-700 dark:border-red-900 dark:bg-red-950/40 dark:text-red-300">
          {error}
        </div>
      )}

      {preview && (
        <>
          <section className="rounded-xl border border-neutral-200 p-4 dark:border-neutral-800">
            <h2 className="text-sm font-semibold">
              {preview.matches.length === 0
                ? "No cut words found"
                : `${accepted} cut${accepted === 1 ? "" : "s"} → ${preview.parts.length} segment${preview.parts.length === 1 ? "" : "s"}`}
              {possible > 0 && (
                <span className="ml-2 font-normal text-neutral-500 dark:text-neutral-400">
                  ({possible} possible match{possible === 1 ? "" : "es"} unchecked below)
                </span>
              )}
            </h2>
            {preview.matches.length === 0 ? (
              <p className="mt-2 text-sm text-neutral-500 dark:text-neutral-400">
                Check the spelling, or pick a more distinctive word next take. You said it
                and it's not here? The transcription may have mangled it — try the Small
                model.
              </p>
            ) : (
              <ul className="mt-3 flex flex-col gap-1">
                {preview.matches.map((m, i) => (
                  <li
                    key={`${m.start}-${i}`}
                    className="flex items-center gap-3 rounded-lg px-2 py-1.5 hover:bg-neutral-100 dark:hover:bg-neutral-800/60"
                  >
                    <input
                      type="checkbox"
                      checked={m.accepted}
                      onChange={() => void handleToggle(i)}
                      aria-label={`Split at ${fmtTime(m.start)} ("${m.label}")`}
                      className="h-4 w-4 accent-emerald-600"
                    />
                    <span className="w-14 tabular-nums text-xs text-neutral-500 dark:text-neutral-400">
                      {fmtTime(m.start)}
                    </span>
                    <span className="flex-1 text-sm">heard “{m.label}”</span>
                    <button
                      type="button"
                      onClick={() => void handleHear(m.start)}
                      className="rounded-md border border-neutral-300 px-2 py-1 text-xs hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-800"
                    >
                      ▶ Hear it
                    </button>
                  </li>
                ))}
              </ul>
            )}
            {isPlaying && (
              <button
                type="button"
                onClick={onPause}
                className="mt-2 rounded-md border border-neutral-300 px-2 py-1 text-xs dark:border-neutral-700"
              >
                ⏸ Pause
              </button>
            )}
          </section>

          {preview.parts.length > 0 && (
            <section className="rounded-xl border border-neutral-200 p-4 dark:border-neutral-800">
              <h2 className="text-sm font-semibold">Segments</h2>
              <ul className="mt-2 flex flex-col gap-1">
                {preview.parts.map((p) => (
                  <li key={p.index} className="flex items-baseline gap-3 text-sm">
                    <span className="w-8 tabular-nums text-xs text-neutral-500">
                      {(p.index + 1).toString().padStart(2, "0")}
                    </span>
                    <span className="flex-1 truncate">{p.title || "(no speech found)"}</span>
                    <span className="tabular-nums text-xs text-neutral-500">
                      {fmtTime(p.start)}–{fmtTime(p.end)} · {fmtTime(p.duration)}
                    </span>
                  </li>
                ))}
              </ul>

              <div className="mt-4 flex flex-wrap items-center gap-4 border-t border-neutral-200 pt-4 dark:border-neutral-800">
                <label className="flex items-center gap-2 text-sm">
                  <input
                    type="checkbox"
                    checked={masterEach}
                    onChange={(e) => setMasterEach(e.target.checked)}
                    className="h-4 w-4 accent-emerald-600"
                  />
                  Master each segment
                  <span className="text-xs text-neutral-500 dark:text-neutral-400">
                    {masterEach
                      ? `(denoise + level + loudness${preview.is_video ? ", −14 LUFS for shorts" : ""})`
                      : "(off — a clean cut of your existing mix, audio otherwise untouched)"}
                  </span>
                </label>
                <label className="flex flex-1 items-center gap-2 text-xs font-medium">
                  Output folder
                  <input
                    type="text"
                    value={outDir}
                    onChange={(e) => setOutDir(e.target.value)}
                    placeholder={
                      sourcePath ? `${sourcePath.replace(/\.[^./\\]+$/, "")}_segments` : "next to the recording"
                    }
                    className="min-w-56 flex-1 rounded-lg border border-neutral-300 bg-transparent px-3 py-2 text-sm font-normal dark:border-neutral-700"
                  />
                </label>
                <button
                  type="button"
                  onClick={() => void handleRender()}
                  disabled={busy !== "idle" || accepted === 0}
                  className="rounded-lg bg-emerald-600 px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-emerald-500 disabled:opacity-40"
                >
                  {busy === "render"
                    ? `Splitting… ${Math.round(progress * 100)}%`
                    : `Split into ${preview.parts.length} file${preview.parts.length === 1 ? "" : "s"}`}
                </button>
              </div>
              {preview.is_video && (
                <p className="mt-2 text-xs text-neutral-500 dark:text-neutral-400">
                  Video segments are re-encoded so every cut is frame-accurate — expect a
                  short render, not an instant copy.
                </p>
              )}
            </section>
          )}
        </>
      )}

      {result && (
        <section className="rounded-xl border border-emerald-300 bg-emerald-50/50 p-4 dark:border-emerald-900 dark:bg-emerald-950/20">
          <h2 className="text-sm font-semibold">
            Done — {result.segments.length} file{result.segments.length === 1 ? "" : "s"} in{" "}
            <span className="font-mono text-xs">{result.out_dir}</span>
          </h2>
          <ul className="mt-2 flex flex-col gap-1">
            {result.segments.map((s) => (
              <li key={s.index} className="flex items-baseline gap-3 text-sm">
                <span className="flex-1 truncate font-mono text-xs">{s.path}</span>
                {s.lufs_in != null && s.lufs_out != null && (
                  <span className="tabular-nums text-xs text-neutral-500">
                    {s.lufs_in.toFixed(1)} → {s.lufs_out.toFixed(1)} LUFS
                  </span>
                )}
              </li>
            ))}
          </ul>
        </section>
      )}
    </main>
  );
}
