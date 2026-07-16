import { useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
  batchCancel,
  batchCancelAll,
  batchIsPaused,
  batchPathKind,
  batchPause,
  batchRemove,
  batchReorder,
  batchResume,
  batchRetryFailed,
  batchSnapshot,
  batchSubmitFiles,
  batchSubmitFolder,
  onBatchProgress,
  type BatchItemState,
  type BatchItemStatus,
  type PresetSummary,
} from "../api";
import { DEFAULT_PRESET, DEFAULT_TIER, type Tier } from "../lib/presets";
import PresetPicker from "../components/PresetPicker";

const STATE_LABEL: Record<BatchItemState, string> = {
  queued: "Queued",
  running: "Mastering…",
  done: "Done",
  failed: "Failed",
  cancelled: "Cancelled",
};

const STATE_COLOR: Record<BatchItemState, string> = {
  queued: "text-neutral-500 dark:text-neutral-400",
  running: "text-emerald-600 dark:text-emerald-400",
  done: "text-emerald-600 dark:text-emerald-400",
  failed: "text-red-500",
  cancelled: "text-neutral-500 dark:text-neutral-400",
};

function basename(path: string): string {
  return path.split(/[\\/]/).pop() ?? path;
}

interface BatchScreenProps {
  presets: PresetSummary[];
}

/** The Batch screen (04 §S4): drop N files or a folder → a queue table with pause/resume/
 * reorder/remove/retry-failed, run against `anvil_batch::BatchQueue`. */
export default function BatchScreen({ presets }: BatchScreenProps) {
  const [items, setItems] = useState<BatchItemStatus[]>([]);
  const [paused, setPaused] = useState(false);
  const [presetRef, setPresetRef] = useState(DEFAULT_PRESET);
  const [tier, setTier] = useState<Tier>(DEFAULT_TIER);
  const [outputDir, setOutputDir] = useState("");
  const [preserveStructure, setPreserveStructure] = useState(false);
  const [dragActive, setDragActive] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  // Client-side only: BatchItemStatus has no preset field, so the "preset" column is
  // filled in from what was selected at submit time (per-job id -> display name).
  const [presetNameById, setPresetNameById] = useState<Record<string, string>>({});

  const wasSettled = useRef(true);

  useEffect(() => {
    if (presetRef === DEFAULT_PRESET && presets.length > 0 && !presets.some((p) => p.preset_ref === DEFAULT_PRESET)) {
      setPresetRef(presets[0].preset_ref);
    }
  }, [presets, presetRef]);

  useEffect(() => {
    batchSnapshot().then(setItems).catch(() => {});
    batchIsPaused().then(setPaused).catch(() => {});
    const unlisten = onBatchProgress(setItems);
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, []);

  // Summary toast (04 §S4 "summary toast"): fires once when a batch that had
  // running/queued work settles into all-terminal.
  useEffect(() => {
    if (items.length === 0) return;
    const settled = items.every((i) => i.state === "done" || i.state === "failed" || i.state === "cancelled");
    if (settled && !wasSettled.current) {
      const done = items.filter((i) => i.state === "done").length;
      const failed = items.filter((i) => i.state === "failed").length;
      const cancelled = items.filter((i) => i.state === "cancelled").length;
      const parts = [`${done} done`];
      if (failed > 0) parts.push(`${failed} failed`);
      if (cancelled > 0) parts.push(`${cancelled} cancelled`);
      setToast(parts.join(" · "));
      window.setTimeout(() => setToast(null), 8000);
    }
    wasSettled.current = settled;
  }, [items]);

  const currentPresetName = useCallback(
    () => presets.find((p) => p.preset_ref === presetRef)?.name ?? presetRef,
    [presets, presetRef],
  );

  const submitFiles = useCallback(
    async (paths: string[]) => {
      if (!outputDir.trim()) {
        setError("Set an output folder first.");
        return;
      }
      setError(null);
      try {
        const ids = await batchSubmitFiles(paths, presetRef, tier, { output_dir: outputDir });
        const name = currentPresetName();
        setPresetNameById((m) => {
          const next = { ...m };
          for (const id of ids) next[id] = name;
          return next;
        });
      } catch (e) {
        setError(typeof e === "string" ? e : "Could not submit those files.");
      }
    },
    [outputDir, presetRef, tier, currentPresetName],
  );

  const submitFolder = useCallback(
    async (root: string) => {
      if (!outputDir.trim()) {
        setError("Set an output folder first.");
        return;
      }
      setError(null);
      try {
        const ids = await batchSubmitFolder(root, presetRef, tier, {
          output_dir: outputDir,
          preserve_structure: preserveStructure,
        });
        const name = currentPresetName();
        setPresetNameById((m) => {
          const next = { ...m };
          for (const id of ids) next[id] = name;
          return next;
        });
      } catch (e) {
        setError(typeof e === "string" ? e : "Could not submit that folder.");
      }
    },
    [outputDir, presetRef, tier, preserveStructure, currentPresetName],
  );

  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      const p = event.payload;
      if (p.type === "enter" || p.type === "over") {
        setDragActive(true);
      } else if (p.type === "leave") {
        setDragActive(false);
      } else if (p.type === "drop") {
        setDragActive(false);
        if (p.paths.length === 0) return;
        if (p.paths.length === 1) {
          void batchPathKind(p.paths[0]).then((kind) => {
            if (kind === "dir") void submitFolder(p.paths[0]);
            else if (kind === "file") void submitFiles(p.paths);
          });
        } else {
          void submitFiles(p.paths);
        }
      }
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, [submitFiles, submitFolder]);

  const handlePauseToggle = async () => {
    if (paused) {
      await batchResume();
      setPaused(false);
    } else {
      await batchPause();
      setPaused(true);
    }
  };

  // Best-effort "up"/"down" within the pending set as currently listed — `BatchQueue`
  // dispatches from the reordered position for real, but its `snapshot()` always reports
  // rows in original submission order (04 §S4 "reorder, where practical"), so the table
  // row order itself won't visibly follow a reorder the way a fully live-sorted table
  // would. Still moves the job earlier/later in the real dispatch queue.
  const pendingIds = items.filter((i) => i.state === "queued").map((i) => i.id);
  const moveInQueue = async (id: string, direction: -1 | 1) => {
    const idx = pendingIds.indexOf(id);
    if (idx === -1) return;
    await batchReorder(id, idx + direction);
  };

  const doneCount = items.filter((i) => i.state === "done").length;
  const failedCount = items.filter((i) => i.state === "failed").length;
  const activeCount = items.filter((i) => i.state === "queued" || i.state === "running").length;

  return (
    <div className="flex flex-1 flex-col gap-4 overflow-y-auto p-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-lg font-semibold">Batch</h1>
          <p className="text-xs text-neutral-500 dark:text-neutral-400">
            Drop files or a folder. Concurrency: auto (uses all but one core).
          </p>
        </div>
        <PresetPicker
          presets={presets}
          presetRef={presetRef}
          tier={tier}
          onPresetChange={setPresetRef}
          onTierChange={setTier}
        />
      </div>

      <div className="flex flex-wrap items-end gap-3 rounded-lg border border-neutral-200 p-3 dark:border-neutral-800">
        <label className="flex min-w-64 flex-1 flex-col gap-1">
          <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Output folder</span>
          <input
            type="text"
            value={outputDir}
            onChange={(e) => setOutputDir(e.target.value)}
            placeholder="Where mastered files are written"
            className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
          />
        </label>
        <label className="flex items-center gap-2 pb-1.5 text-xs text-neutral-600 dark:text-neutral-300">
          <input
            type="checkbox"
            checked={preserveStructure}
            onChange={(e) => setPreserveStructure(e.target.checked)}
          />
          Preserve folder structure (back-catalog)
        </label>
      </div>

      <div
        className={`flex flex-col items-center gap-1 rounded-xl border-2 border-dashed p-6 text-center transition-colors ${
          dragActive
            ? "border-emerald-500 bg-emerald-500/5"
            : "border-neutral-300 dark:border-neutral-700"
        }`}
      >
        <p className="text-sm font-medium">Drop audio/video files or a folder here</p>
        <p className="text-xs text-neutral-500 dark:text-neutral-400">
          Each file runs the selected preset and tier. Nothing leaves your machine.
        </p>
      </div>

      {error && <p className="text-xs text-red-500">{error}</p>}
      {toast && (
        <div className="rounded-lg border border-emerald-500/40 bg-emerald-500/5 px-3 py-2 text-xs text-emerald-700 dark:text-emerald-300">
          {toast}
        </div>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          onClick={() => void handlePauseToggle()}
          disabled={items.length === 0}
          className="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 disabled:cursor-not-allowed disabled:opacity-40 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
        >
          {paused ? "Resume queue" : "Pause queue"}
        </button>
        <button
          type="button"
          onClick={() => void batchRetryFailed().then(() => batchSnapshot().then(setItems))}
          disabled={failedCount === 0}
          className="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 disabled:cursor-not-allowed disabled:opacity-40 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
        >
          Retry failed ({failedCount})
        </button>
        <button
          type="button"
          onClick={() => void batchCancelAll().then(() => batchSnapshot().then(setItems))}
          disabled={activeCount === 0}
          className="ml-auto rounded-lg border border-red-500/40 px-3 py-1.5 text-xs font-medium text-red-600 hover:bg-red-500/10 disabled:cursor-not-allowed disabled:opacity-40 dark:text-red-400"
        >
          Cancel all
        </button>
      </div>

      <div className="overflow-hidden rounded-xl border border-neutral-200 dark:border-neutral-800">
        <table className="w-full text-left text-xs">
          <thead className="bg-neutral-100 text-neutral-500 dark:text-neutral-400 dark:bg-neutral-900">
            <tr>
              <th className="px-3 py-2 font-medium">File</th>
              <th className="px-3 py-2 font-medium">Preset</th>
              <th className="px-3 py-2 font-medium">Status</th>
              <th className="px-3 py-2 font-medium">Progress</th>
              <th className="px-3 py-2 font-medium">Result</th>
              <th className="px-3 py-2 font-medium" />
            </tr>
          </thead>
          <tbody>
            {items.length === 0 && (
              <tr>
                <td colSpan={6} className="px-3 py-6 text-center text-neutral-500 dark:text-neutral-400">
                  Nothing queued yet.
                </td>
              </tr>
            )}
            {items.map((item) => (
              <tr key={item.id} className="border-t border-neutral-200 dark:border-neutral-800">
                <td className="max-w-48 truncate px-3 py-2" title={item.input}>
                  {basename(item.input)}
                </td>
                <td className="px-3 py-2 text-neutral-500 dark:text-neutral-400">
                  {presetNameById[item.id] ?? "—"}
                </td>
                <td className={`px-3 py-2 font-medium ${STATE_COLOR[item.state]}`}>
                  {STATE_LABEL[item.state]}
                </td>
                <td className="px-3 py-2">
                  <div className="h-1.5 w-24 overflow-hidden rounded-full bg-neutral-200 dark:bg-neutral-800">
                    <div
                      className="h-full bg-emerald-500"
                      style={{ width: `${Math.round(item.progress * 100)}%` }}
                    />
                  </div>
                </td>
                <td className="max-w-56 truncate px-3 py-2 text-neutral-500 dark:text-neutral-400" title={item.error ?? item.output}>
                  {item.state === "done" ? basename(item.output) : item.error ?? item.message}
                </td>
                <td className="whitespace-nowrap px-3 py-2 text-right">
                  {item.state === "queued" && (
                    <>
                      <button
                        type="button"
                        onClick={() => void moveInQueue(item.id, -1)}
                        aria-label="Move earlier in the queue"
                        className="px-1 text-neutral-500 dark:text-neutral-400 hover:text-neutral-700 dark:hover:text-neutral-200"
                      >
                        ↑
                      </button>
                      <button
                        type="button"
                        onClick={() => void moveInQueue(item.id, 1)}
                        aria-label="Move later in the queue"
                        className="px-1 text-neutral-500 dark:text-neutral-400 hover:text-neutral-700 dark:hover:text-neutral-200"
                      >
                        ↓
                      </button>
                    </>
                  )}
                  {(item.state === "queued" || item.state === "running") && (
                    <button
                      type="button"
                      onClick={() => void batchCancel(item.id)}
                      className="px-1 text-neutral-500 dark:text-neutral-400 hover:text-red-500"
                    >
                      Cancel
                    </button>
                  )}
                  {(item.state === "done" ||
                    item.state === "failed" ||
                    item.state === "cancelled") && (
                    <button
                      type="button"
                      onClick={() => void batchRemove(item.id).then(() => batchSnapshot().then(setItems))}
                      className="px-1 text-neutral-500 dark:text-neutral-400 hover:text-red-500"
                    >
                      Remove
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {items.length > 0 && (
        <p className="text-xs text-neutral-500 dark:text-neutral-400">
          {doneCount} done · {failedCount} failed · {activeCount} in progress
        </p>
      )}
    </div>
  );
}
