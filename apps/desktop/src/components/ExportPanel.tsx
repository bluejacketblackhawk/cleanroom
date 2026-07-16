import { useEffect, useState } from "react";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { exportOutputs, onExportProgress, type ExportFormat, type OutputSpec } from "../api";

type RowStatus = "idle" | "exporting" | "done" | "error";

interface OutputRow {
  id: string;
  format: ExportFormat;
  bitrate: number;
  mono: boolean;
  destination: string;
  status: RowStatus;
  progress: number;
  message: string | null;
}

const LOSSY_FORMATS: ExportFormat[] = ["mp3", "opus", "aac"];
const DEFAULT_BITRATE: Record<ExportFormat, number> = {
  wav: 0,
  flac: 0,
  mp3: 192,
  opus: 128,
  aac: 160,
};
const BITRATE_OPTIONS: Record<ExportFormat, number[]> = {
  wav: [],
  flac: [],
  mp3: [128, 192, 256, 320],
  opus: [64, 96, 128, 160],
  aac: [96, 128, 160, 192],
};

// Real extensions per format (matches `anvil_media::encode::OutputFormat::extension` —
// notably AAC's conventional container is `.m4a`, not `.aac`).
const EXTENSION: Record<ExportFormat, string> = {
  wav: "wav",
  mp3: "mp3",
  flac: "flac",
  opus: "opus",
  aac: "m4a",
};

function splitDirAndStem(sourcePath: string): { dir: string; sep: string; stem: string } {
  const sep = sourcePath.includes("\\") ? "\\" : "/";
  const parts = sourcePath.split(/[\\/]/);
  const fileName = parts.pop() ?? sourcePath;
  const dir = parts.join(sep);
  const stem = fileName.replace(/\.[^./\\]+$/, "");
  return { dir, sep, stem };
}

/** Output naming tokens (04 §S8): defaults next to the source file as `{stem}_mastered`.
 * Falls back to a bare filename (no directory) if no file is open yet. */
function defaultDestination(sourcePath: string | null, format: ExportFormat): string {
  const ext = EXTENSION[format];
  if (!sourcePath) return `mastered.${ext}`;
  const { dir, sep, stem } = splitDirAndStem(sourcePath);
  return dir ? `${dir}${sep}${stem}_mastered.${ext}` : `${stem}_mastered.${ext}`;
}

function swapExtension(path: string, format: ExportFormat): string {
  return path.replace(/\.[^./\\]+$/, `.${EXTENSION[format]}`);
}

let nextId = 1;
function makeRow(format: ExportFormat, sourcePath: string | null): OutputRow {
  return {
    id: `out-${nextId++}`,
    format,
    bitrate: DEFAULT_BITRATE[format],
    mono: false,
    destination: defaultDestination(sourcePath, format),
    status: "idle",
    progress: 0,
    message: null,
  };
}

const STATUS_LABEL: Record<RowStatus, string> = {
  idle: "Waiting",
  exporting: "Exporting…",
  done: "Done",
  error: "Failed",
};

function StatusPill({ status }: { status: RowStatus }) {
  const color =
    status === "done"
      ? "text-emerald-600 dark:text-emerald-400"
      : status === "error"
        ? "text-red-500"
        : "text-neutral-500 dark:text-neutral-400";
  return <span className={`text-xs font-medium ${color}`}>{STATUS_LABEL[status]}</span>;
}

interface ExportPanelProps {
  /** Export operates on the mastered result — disabled until a Master run exists. */
  canExport: boolean;
  /** The currently open file's full path, used to default each output's destination next
   * to the source (04 §S8 naming tokens). `null` when nothing is open yet. */
  sourcePath: string | null;
}

/** The Export tab (04 §S2 Export tab): an outputs list (WAV/MP3/FLAC/Opus/AAC, bitrate for
 * lossy, mono/stereo, destination), Export All with per-output progress, "Open folder", and
 * an optional compliance report from the last Master run's real measurements. */
export default function ExportPanel({ canExport, sourcePath }: ExportPanelProps) {
  const [outputs, setOutputs] = useState<OutputRow[]>(() => [makeRow("wav", sourcePath)]);
  const [exporting, setExporting] = useState(false);
  const [compliance, setCompliance] = useState(false);
  const [complianceReport, setComplianceReport] = useState<string | null>(null);
  const [banner, setBanner] = useState<string | null>(null);

  // A newly opened file resets the outputs list back to a single WAV row defaulted next
  // to the new source (mirrors App.tsx resetting the Master tab's report on file load).
  useEffect(() => {
    setOutputs([makeRow("wav", sourcePath)]);
    setBanner(null);
    setComplianceReport(null);
    setCompliance(false);
  }, [sourcePath]);

  useEffect(() => {
    const unlisten = onExportProgress(({ index, fraction }) => {
      setOutputs((rows) =>
        rows.map((r, i) => (i === index ? { ...r, progress: fraction } : r)),
      );
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, []);

  const updateRow = (id: string, patch: Partial<OutputRow>) => {
    setOutputs((rows) => rows.map((r) => (r.id === id ? { ...r, ...patch } : r)));
  };

  const setFormat = (id: string, format: ExportFormat) => {
    setOutputs((rows) =>
      rows.map((r) =>
        r.id === id
          ? {
              ...r,
              format,
              bitrate: DEFAULT_BITRATE[format],
              destination: swapExtension(r.destination, format),
            }
          : r,
      ),
    );
  };

  const addOutput = () => {
    setOutputs((rows) => [
      ...rows,
      makeRow(rows.length % 2 === 0 ? "wav" : "mp3", sourcePath),
    ]);
  };

  const removeOutput = (id: string) => {
    setOutputs((rows) => (rows.length > 1 ? rows.filter((r) => r.id !== id) : rows));
  };

  const handleExportAll = async () => {
    if (!canExport || outputs.length === 0 || exporting) return;
    setExporting(true);
    setBanner(null);
    setComplianceReport(null);
    setOutputs((rows) =>
      rows.map((r) => ({ ...r, status: "exporting", progress: 0, message: null })),
    );
    try {
      const specs: OutputSpec[] = outputs.map((r) => ({
        format: r.format,
        path: r.destination,
        bitrate: LOSSY_FORMATS.includes(r.format) ? r.bitrate : null,
        mono: r.mono,
      }));
      const result = await exportOutputs(specs, compliance);
      setOutputs((rows) =>
        rows.map((r, i) => {
          const item = result.outputs[i];
          return {
            ...r,
            status: item?.ok ? "done" : "error",
            progress: item?.ok ? 1 : r.progress,
            message: item?.message ?? null,
          };
        }),
      );
      const parts: string[] = [result.ok ? "All outputs exported." : "Some outputs failed."];
      if (compliance) {
        if (result.compliance_report) {
          setComplianceReport(result.compliance_report);
          parts.push("Compliance report written.");
        } else if (result.compliance_error) {
          parts.push(`Compliance report skipped: ${result.compliance_error}`);
        }
      }
      setBanner(parts.join(" "));
    } catch (e) {
      setOutputs((rows) => rows.map((r) => ({ ...r, status: "error" })));
      setBanner(typeof e === "string" ? e : "Export failed.");
    } finally {
      setExporting(false);
    }
  };

  const openFolder = async (destination: string) => {
    try {
      // Reveals the exported file in the OS's file explorer (Explorer/Finder) — the
      // "open-path" permission isn't in opener:default, but "reveal-item-in-dir" is,
      // and it's the better match for "Open folder" anyway (highlights the file).
      await revealItemInDir(destination);
    } catch {
      setBanner("Could not open that folder.");
    }
  };

  return (
    <div className="flex flex-col gap-4">
      {!canExport && (
        <p className="rounded-lg border border-dashed border-neutral-300 p-3 text-xs text-neutral-500 dark:text-neutral-400 dark:border-neutral-700">
          Master the file first — Export works on the mastered result.
        </p>
      )}

      <ul className="flex flex-col gap-3">
        {outputs.map((row) => (
          <li
            key={row.id}
            className="rounded-lg border border-neutral-200 p-3 dark:border-neutral-800"
          >
            <div className="flex flex-wrap items-center gap-2">
              <label className="sr-only" htmlFor={`${row.id}-format`}>
                Format
              </label>
              <select
                id={`${row.id}-format`}
                value={row.format}
                onChange={(e) => setFormat(row.id, e.target.value as ExportFormat)}
                className="rounded-md border border-neutral-300 bg-white px-2 py-1 text-xs dark:border-neutral-700 dark:bg-neutral-900"
              >
                <option value="wav">WAV</option>
                <option value="mp3">MP3</option>
                <option value="flac">FLAC</option>
                <option value="opus">Opus</option>
                <option value="aac">AAC</option>
              </select>

              {BITRATE_OPTIONS[row.format].length > 0 && (
                <label className="flex items-center gap-1 text-xs text-neutral-500 dark:text-neutral-400">
                  <span className="sr-only">Bitrate</span>
                  <select
                    value={row.bitrate}
                    onChange={(e) => updateRow(row.id, { bitrate: Number(e.target.value) })}
                    className="rounded-md border border-neutral-300 bg-white px-2 py-1 text-xs dark:border-neutral-700 dark:bg-neutral-900"
                  >
                    {BITRATE_OPTIONS[row.format].map((b) => (
                      <option key={b} value={b}>
                        {b} kbps
                      </option>
                    ))}
                  </select>
                </label>
              )}

              <button
                type="button"
                role="switch"
                aria-checked={row.mono}
                aria-label={row.mono ? "Mono output — click for stereo" : "Stereo output — click for mono"}
                onClick={() => updateRow(row.id, { mono: !row.mono })}
                className="rounded-full border border-neutral-300 px-2.5 py-1 text-xs font-medium text-neutral-600 dark:border-neutral-700 dark:text-neutral-300"
              >
                {row.mono ? "Mono" : "Stereo"}
              </button>

              <div className="ml-auto flex items-center gap-2">
                <StatusPill status={row.status} />
                {outputs.length > 1 && (
                  <button
                    type="button"
                    onClick={() => removeOutput(row.id)}
                    aria-label="Remove this output"
                    className="text-neutral-500 dark:text-neutral-400 hover:text-red-500"
                  >
                    ×
                  </button>
                )}
              </div>
            </div>

            <label className="mt-2 flex flex-col gap-1">
              <span className="text-xs text-neutral-500 dark:text-neutral-400">Destination</span>
              <input
                type="text"
                value={row.destination}
                onChange={(e) => updateRow(row.id, { destination: e.target.value })}
                className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
              />
            </label>

            {row.status === "exporting" && (
              <div className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-neutral-200 dark:bg-neutral-800">
                <div
                  className="h-full bg-emerald-500 transition-[width]"
                  style={{ width: `${Math.round(row.progress * 100)}%` }}
                />
              </div>
            )}

            {row.status === "done" && (
              <button
                type="button"
                onClick={() => void openFolder(row.destination)}
                className="mt-2 text-xs font-medium text-emerald-600 hover:underline dark:text-emerald-400"
              >
                Open folder
              </button>
            )}
            {row.status === "error" && row.message && (
              <p className="mt-2 text-xs text-red-500">{row.message}</p>
            )}
          </li>
        ))}
      </ul>

      <label className="flex items-center gap-2 text-xs text-neutral-600 dark:text-neutral-300">
        <input
          type="checkbox"
          checked={compliance}
          onChange={(e) => setCompliance(e.target.checked)}
          disabled={!canExport}
        />
        Write a compliance report (HTML + PDF) alongside the first output
      </label>
      {complianceReport && (
        <button
          type="button"
          onClick={() => void openFolder(complianceReport)}
          className="-mt-2 self-start text-xs font-medium text-emerald-600 hover:underline dark:text-emerald-400"
        >
          Open compliance report
        </button>
      )}

      <div className="flex items-center gap-3">
        <button
          type="button"
          onClick={addOutput}
          className="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
        >
          + Add output
        </button>
        <button
          type="button"
          onClick={() => void handleExportAll()}
          disabled={!canExport || exporting}
          className="ml-auto rounded-lg bg-emerald-600 px-4 py-2 text-sm font-semibold text-white transition-colors hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {exporting ? "Exporting…" : "Export all"}
        </button>
      </div>
      {banner && <p className="text-xs text-neutral-500 dark:text-neutral-400">{banner}</p>}
    </div>
  );
}
