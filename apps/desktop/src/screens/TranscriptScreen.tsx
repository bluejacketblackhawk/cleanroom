import { useCallback, useEffect, useState } from "react";
import {
  applyCuts,
  diarize,
  exportTranscript,
  planCuts,
  seek,
  transcribe,
  writeTextFile,
  type Cut,
  type CutPlan,
  type MediaSummary,
  type Speaker,
  type Transcript,
  type TranscriptExportFormat,
} from "../api";
import Waveform from "../Waveform";
import WaveformCutOverlay from "../components/WaveformCutOverlay";
import TranscriptWords from "../components/TranscriptWords";
import CutReviewList from "../components/CutReviewList";
import { formatBytes, formatTime } from "../lib/format";
import { speakerColor } from "../lib/speakers";
import { usePlayhead } from "../lib/usePlayhead";
import { useModelDownloads } from "../lib/useModelDownloads";

interface TranscriptScreenProps {
  media: MediaSummary | null;
  fileName: string | null;
  sourcePath: string | null;
  totalFrames: number;
  isPlaying: boolean;
  onPlay: () => void;
  onPause: () => void;
}

function defaultDestination(sourcePath: string | null, format: TranscriptExportFormat): string {
  if (!sourcePath) return `transcript.${format}`;
  const sep = sourcePath.includes("\\") ? "\\" : "/";
  const parts = sourcePath.split(/[\\/]/);
  const fileName = parts.pop() ?? sourcePath;
  const dir = parts.join(sep);
  const stem = fileName.replace(/\.[^./\\]+$/, "");
  return dir ? `${dir}${sep}${stem}.${format}` : `${stem}.${format}`;
}

const EXPORT_FORMATS: TranscriptExportFormat[] = ["srt", "vtt", "txt", "json"];
const CUT_MODES: { id: "silence" | "filler" | "both"; label: string }[] = [
  { id: "both", label: "Silence + filler" },
  { id: "silence", label: "Silence only" },
  { id: "filler", label: "Filler only" },
];

/**
 * The M3 Transcript tab (04 §S2): transcribe with a model picker, word-level
 * playback-follow + search + SRT/VTT/TXT/JSON export, and the filler/silence review list
 * with play-in-context, accept/reject, and a bulk "apply safe set" — accepted cuts render
 * as strikethrough regions on the waveform.
 */
export default function TranscriptScreen({
  media,
  fileName,
  sourcePath,
  totalFrames,
  isPlaying,
  onPlay,
  onPause,
}: TranscriptScreenProps) {
  const { models, progress, start } = useModelDownloads();
  const asrModels = models.filter((m) => m.kind === "asr");

  const [selectedModel, setSelectedModel] = useState("");
  const [innerTab, setInnerTab] = useState<"transcript" | "cuts">("transcript");

  const [transcript, setTranscript] = useState<Transcript | null>(null);
  const [transcribing, setTranscribing] = useState(false);
  const [transcribeError, setTranscribeError] = useState<string | null>(null);
  const [searchQuery, setSearchQuery] = useState("");

  // Speaker diarization state (04 §S2 "speaker labels").
  const [speakers, setSpeakers] = useState<Speaker[]>([]);
  const [diarizing, setDiarizing] = useState(false);
  const [diarizeError, setDiarizeError] = useState<string | null>(null);
  const [numSpeakers, setNumSpeakers] = useState("auto");

  const [cutMode, setCutMode] = useState<"silence" | "filler" | "both">("both");
  const [cutPlan, setCutPlan] = useState<CutPlan | null>(null);
  const [accepted, setAccepted] = useState<Set<number>>(new Set());
  const [planning, setPlanning] = useState(false);
  const [planError, setPlanError] = useState<string | null>(null);
  const [applying, setApplying] = useState(false);
  const [applyError, setApplyError] = useState<string | null>(null);

  const [exportFormat, setExportFormat] = useState<TranscriptExportFormat>("srt");
  const [exportDest, setExportDest] = useState(() => defaultDestination(sourcePath, "srt"));
  const [exportBanner, setExportBanner] = useState<string | null>(null);

  const currentSeconds = usePlayhead(media?.sample_rate ?? 0, isPlaying);

  // Pick a sensible default model once the list loads: an already-installed one if there
  // is one, otherwise the first (smallest) pack.
  useEffect(() => {
    if (selectedModel || asrModels.length === 0) return;
    const installed = asrModels.find((m) => m.installed);
    setSelectedModel((installed ?? asrModels[0]).id);
  }, [asrModels, selectedModel]);

  // A fresh file clears any transcript/cut state from the previous one (the backend does
  // the same for its own copies on `open_media`).
  useEffect(() => {
    setTranscript(null);
    setTranscribeError(null);
    setSearchQuery("");
    setSpeakers([]);
    setDiarizeError(null);
    setCutPlan(null);
    setAccepted(new Set());
    setPlanError(null);
    setApplyError(null);
    setExportBanner(null);
    setExportDest(defaultDestination(sourcePath, exportFormat));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sourcePath]);

  const handleFormatChange = (fmt: TranscriptExportFormat) => {
    setExportFormat(fmt);
    setExportDest((d) => d.replace(/\.[^./\\]+$/, `.${fmt}`));
  };

  const handleSeekSeconds = useCallback(
    (seconds: number) => {
      if (!media) return;
      void seek(Math.round(seconds * media.sample_rate));
    },
    [media],
  );

  const handlePlayInContext = useCallback(
    (cut: Cut) => {
      if (!media) return;
      const preroll = Math.max(0, cut.start - 0.75);
      void seek(Math.round(preroll * media.sample_rate)).then(() => {
        if (!isPlaying) onPlay();
      });
    },
    [media, isPlaying, onPlay],
  );

  const selectedPack = asrModels.find((m) => m.id === selectedModel);
  const modelReady = selectedPack?.installed ?? false;
  const selectedDownload = selectedModel ? progress[selectedModel] : undefined;

  const handleTranscribe = async () => {
    if (!selectedModel || !media) return;
    setTranscribing(true);
    setTranscribeError(null);
    try {
      const t = await transcribe(selectedModel);
      setTranscript(t);
      setSpeakers([]);
      setDiarizeError(null);
      setCutPlan(null);
      setAccepted(new Set());
    } catch (e) {
      setTranscribeError(typeof e === "string" ? e : "Could not transcribe this file.");
    } finally {
      setTranscribing(false);
    }
  };

  const handleDiarize = async () => {
    if (!transcript) return;
    setDiarizing(true);
    setDiarizeError(null);
    try {
      const count = numSpeakers === "auto" ? null : Number(numSpeakers);
      const result = await diarize(count);
      setTranscript(result.transcript);
      setSpeakers(result.speakers);
      if (result.speakers.length === 0) {
        setDiarizeError("No distinct speakers were found in this file.");
      }
    } catch (e) {
      setDiarizeError(typeof e === "string" ? e : "Could not identify speakers.");
    } finally {
      setDiarizing(false);
    }
  };

  const renameSpeaker = (id: number, label: string) => {
    setSpeakers((prev) => prev.map((s) => (s.id === id ? { ...s, label } : s)));
  };

  const handleFindCuts = async () => {
    if (!media) return;
    setPlanning(true);
    setPlanError(null);
    try {
      const plan = await planCuts(cutMode);
      setCutPlan(plan);
      setAccepted(
        new Set(plan.cuts.reduce<number[]>((acc, c, i) => (c.accepted ? [...acc, i] : acc), [])),
      );
    } catch (e) {
      setPlanError(typeof e === "string" ? e : "Could not plan cuts.");
    } finally {
      setPlanning(false);
    }
  };

  const handleToggleCut = (i: number) => {
    setAccepted((prev) => {
      const next = new Set(prev);
      if (next.has(i)) next.delete(i);
      else next.add(i);
      return next;
    });
  };

  const runApply = async (indices: number[]) => {
    setApplying(true);
    setApplyError(null);
    try {
      await applyCuts(indices);
    } catch (e) {
      setApplyError(typeof e === "string" ? e : "Could not apply cuts.");
    } finally {
      setApplying(false);
    }
  };

  const handleApply = () => void runApply(Array.from(accepted));

  const handleApplySafeSet = () => {
    if (!cutPlan) return;
    const safe = cutPlan.cuts.reduce<number[]>(
      (acc, c, i) => (c.kind === "silence" ? [...acc, i] : acc),
      [],
    );
    setAccepted(new Set(safe));
    void runApply(safe);
  };

  const handleSaveExport = async () => {
    if (!transcript) return;
    setExportBanner(null);
    try {
      const content = await exportTranscript(exportFormat);
      await writeTextFile(exportDest, content);
      setExportBanner(`Saved ${exportDest}`);
    } catch (e) {
      setExportBanner(typeof e === "string" ? e : "Could not save the export.");
    }
  };

  const handleCopyExport = async () => {
    if (!transcript) return;
    setExportBanner(null);
    try {
      const content = await exportTranscript(exportFormat);
      await navigator.clipboard.writeText(content);
      setExportBanner(`Copied ${exportFormat.toUpperCase()} to the clipboard.`);
    } catch {
      setExportBanner("Could not copy to the clipboard.");
    }
  };

  if (!media) {
    return (
      <main className="flex flex-1 items-center justify-center p-6">
        <p className="max-w-sm text-center text-sm text-neutral-500 dark:text-neutral-400">
          Open a file from Master first — the Transcript tab works on whatever is currently
          loaded.
        </p>
      </main>
    );
  }

  return (
    <main className="flex flex-1 flex-col gap-4 overflow-y-auto p-6">
      <div className="flex items-baseline justify-between gap-4">
        <span className="truncate text-sm font-medium" title={fileName ?? ""}>
          {fileName}
        </span>
        <span className="shrink-0 text-xs text-neutral-500 dark:text-neutral-400">{formatTime(media.duration_secs)}</span>
      </div>

      <div className="relative">
        <Waveform
          totalFrames={totalFrames}
          sampleRate={media.sample_rate}
          isPlaying={isPlaying}
          source="original"
          heightClassName="h-24"
          // 04 §Accessibility: the waveform's text alternative includes cut count here —
          // the overlay's colored strikethrough regions (`WaveformCutOverlay`) are visual
          // only, so a screen-reader user needs this in words instead.
          ariaContext={
            cutPlan
              ? `${accepted.size} of ${cutPlan.cuts.length} suggested cut${cutPlan.cuts.length === 1 ? "" : "s"} accepted`
              : undefined
          }
          onSeek={() => {}}
          onReachedEnd={() => {}}
        />
        {cutPlan && (
          <WaveformCutOverlay cuts={cutPlan.cuts} accepted={accepted} durationSecs={media.duration_secs} />
        )}
      </div>

      <div className="flex items-center gap-3">
        <button
          type="button"
          onClick={() => void (isPlaying ? onPause() : onPlay())}
          className="rounded-lg bg-neutral-900 px-4 py-1.5 text-xs font-medium text-white dark:bg-white dark:text-neutral-900"
        >
          {isPlaying ? "Pause" : "Play"}
        </button>
        <span className="text-xs tabular-nums text-neutral-500 dark:text-neutral-400">{formatTime(currentSeconds)}</span>
      </div>

      <div
        role="tablist"
        aria-label="Transcript panel"
        onKeyDown={(e) => {
          if (e.key !== "ArrowRight" && e.key !== "ArrowLeft") return;
          e.preventDefault();
          setInnerTab((t) => (t === "transcript" ? "cuts" : "transcript"));
        }}
        className="flex w-fit gap-1 rounded-lg bg-neutral-100 p-1 text-xs dark:bg-neutral-900"
      >
        {(
          [
            { id: "transcript" as const, label: "Transcript" },
            { id: "cuts" as const, label: `Filler & silence${cutPlan ? ` (${cutPlan.cuts.length})` : ""}` },
          ]
        ).map((t) => (
          <button
            key={t.id}
            type="button"
            role="tab"
            id={`inner-tab-${t.id}`}
            aria-selected={innerTab === t.id}
            aria-controls={`inner-panel-${t.id}`}
            tabIndex={innerTab === t.id ? 0 : -1}
            onClick={() => setInnerTab(t.id)}
            className={`rounded-md px-3 py-1.5 font-medium transition-colors ${
              innerTab === t.id
                ? "bg-white text-neutral-900 shadow-sm dark:bg-neutral-700 dark:text-white"
                : "text-neutral-500 dark:text-neutral-400 hover:text-neutral-800 dark:hover:text-neutral-200"
            }`}
          >
            {t.label}
          </button>
        ))}
      </div>

      {innerTab === "transcript" ? (
        <div
          className="flex flex-col gap-4"
          role="tabpanel"
          id="inner-panel-transcript"
          aria-labelledby="inner-tab-transcript"
        >
          <div className="flex flex-wrap items-end gap-3 rounded-lg border border-neutral-200 p-3 dark:border-neutral-800">
            <label className="flex flex-col gap-1">
              <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Model</span>
              <select
                value={selectedModel}
                onChange={(e) => setSelectedModel(e.target.value)}
                className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
              >
                {asrModels.map((m) => (
                  <option key={m.id} value={m.id}>
                    {m.name} · {m.size}
                    {m.installed ? "" : " — not installed"}
                  </option>
                ))}
              </select>
            </label>

            {modelReady ? (
              <button
                type="button"
                onClick={() => void handleTranscribe()}
                disabled={transcribing}
                className="rounded-lg bg-emerald-600 px-4 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
              >
                {transcribing ? "Transcribing…" : "Transcribe"}
              </button>
            ) : selectedDownload && (selectedDownload.status === "downloading" || selectedDownload.status === "verifying") ? (
              <span className="text-xs text-neutral-500 dark:text-neutral-400">
                {selectedDownload.status === "verifying"
                  ? "Verifying…"
                  : `Downloading… ${formatBytes(selectedDownload.downloaded_bytes)} of ${formatBytes(selectedDownload.total_bytes)}`}
              </span>
            ) : (
              <button
                type="button"
                onClick={() => selectedModel && start(selectedModel)}
                disabled={!selectedPack?.downloadable}
                className="rounded-lg border border-emerald-500/50 px-4 py-1.5 text-xs font-medium text-emerald-600 hover:bg-emerald-500/10 disabled:cursor-not-allowed disabled:opacity-40 dark:text-emerald-400"
              >
                {selectedPack ? `Will download ${selectedPack.size} — click to start` : "Choose a model"}
              </button>
            )}

            <label className="ml-auto flex flex-col gap-1">
              <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Search</span>
              <input
                type="search"
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                placeholder="Find a word…"
                disabled={!transcript}
                className="w-40 rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs disabled:opacity-50 dark:border-neutral-700 dark:bg-neutral-900"
              />
            </label>
          </div>
          {transcribeError && <p className="text-xs text-red-500">{transcribeError}</p>}

          {transcript && (
            <div className="flex flex-wrap items-center gap-3 rounded-lg border border-neutral-200 p-3 dark:border-neutral-800">
              <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Speakers</span>
              <label className="flex items-center gap-1.5 text-xs text-neutral-500 dark:text-neutral-400">
                How many?
                <select
                  value={numSpeakers}
                  onChange={(e) => setNumSpeakers(e.target.value)}
                  className="rounded-md border border-neutral-300 bg-white px-2 py-1 text-xs dark:border-neutral-700 dark:bg-neutral-900"
                >
                  <option value="auto">Auto-detect</option>
                  {[2, 3, 4, 5, 6].map((n) => (
                    <option key={n} value={String(n)}>
                      {n}
                    </option>
                  ))}
                </select>
              </label>
              <button
                type="button"
                onClick={() => void handleDiarize()}
                disabled={diarizing}
                className="rounded-lg border border-emerald-500/50 px-3 py-1.5 text-xs font-medium text-emerald-600 hover:bg-emerald-500/10 disabled:cursor-not-allowed disabled:opacity-50 dark:text-emerald-400"
              >
                {diarizing ? "Identifying…" : speakers.length > 0 ? "Identify again" : "Identify speakers"}
              </button>
              {speakers.map((s) => {
                const color = speakerColor(s.id);
                return (
                  <span key={s.id} className="flex items-center gap-1.5">
                    <span
                      aria-hidden="true"
                      className="h-3 w-3 shrink-0 rounded-full"
                      style={{ background: color.solid }}
                    />
                    <input
                      type="text"
                      value={s.label}
                      onChange={(e) => renameSpeaker(s.id, e.target.value)}
                      aria-label={`Name for speaker ${s.id + 1}`}
                      className="w-24 rounded-md border border-neutral-300 bg-white px-1.5 py-0.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
                    />
                  </span>
                );
              })}
            </div>
          )}
          {diarizeError && <p className="text-xs text-red-500">{diarizeError}</p>}

          <div className="rounded-lg border border-neutral-200 p-4 dark:border-neutral-800">
            {transcript ? (
              <TranscriptWords
                transcript={transcript}
                currentSeconds={currentSeconds}
                searchQuery={searchQuery}
                onSeekSeconds={handleSeekSeconds}
                speakers={speakers}
              />
            ) : (
              <p className="text-xs text-neutral-500 dark:text-neutral-400">
                {transcribing ? "Listening to the file…" : "Transcribe the file to see the words here."}
              </p>
            )}
          </div>

          {transcript && (
            <div className="flex flex-wrap items-end gap-2 rounded-lg border border-neutral-200 p-3 dark:border-neutral-800">
              <label className="flex flex-col gap-1">
                <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Export as</span>
                <select
                  value={exportFormat}
                  onChange={(e) => handleFormatChange(e.target.value as TranscriptExportFormat)}
                  className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs uppercase dark:border-neutral-700 dark:bg-neutral-900"
                >
                  {EXPORT_FORMATS.map((f) => (
                    <option key={f} value={f}>
                      {f.toUpperCase()}
                    </option>
                  ))}
                </select>
              </label>
              <label className="flex min-w-64 flex-1 flex-col gap-1">
                <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Destination</span>
                <input
                  type="text"
                  value={exportDest}
                  onChange={(e) => setExportDest(e.target.value)}
                  className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
                />
              </label>
              <button
                type="button"
                onClick={() => void handleSaveExport()}
                className="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
              >
                Save
              </button>
              <button
                type="button"
                onClick={() => void handleCopyExport()}
                className="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
              >
                Copy
              </button>
              {exportBanner && <span className="text-xs text-neutral-500 dark:text-neutral-400">{exportBanner}</span>}
            </div>
          )}
        </div>
      ) : (
        <div
          className="flex flex-col gap-4"
          role="tabpanel"
          id="inner-panel-cuts"
          aria-labelledby="inner-tab-cuts"
        >
          <div className="flex flex-wrap items-end gap-3 rounded-lg border border-neutral-200 p-3 dark:border-neutral-800">
            <label className="flex flex-col gap-1">
              <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Look for</span>
              <select
                value={cutMode}
                onChange={(e) => setCutMode(e.target.value as typeof cutMode)}
                className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
              >
                {CUT_MODES.map((m) => (
                  <option key={m.id} value={m.id}>
                    {m.label}
                  </option>
                ))}
              </select>
            </label>
            <button
              type="button"
              onClick={() => void handleFindCuts()}
              disabled={planning}
              className="rounded-lg bg-emerald-600 px-4 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
            >
              {planning ? "Finding cuts…" : cutPlan ? "Find cuts again" : "Find cuts"}
            </button>
            {!transcript && (
              <span className="text-xs text-neutral-500 dark:text-neutral-400">
                Transcribing first gives filler-word cuts real timestamps — without it
                you'll get a couple of illustrative ones.
              </span>
            )}
          </div>
          {planError && <p className="text-xs text-red-500">{planError}</p>}
          {applyError && <p className="text-xs text-red-500">{applyError}</p>}

          {cutPlan && (
            <CutReviewList
              cuts={cutPlan.cuts}
              accepted={accepted}
              onToggle={handleToggleCut}
              onPlayInContext={handlePlayInContext}
              onApplySafeSet={handleApplySafeSet}
              onApply={handleApply}
              applying={applying}
              planning={planning}
            />
          )}
        </div>
      )}
    </main>
  );
}
