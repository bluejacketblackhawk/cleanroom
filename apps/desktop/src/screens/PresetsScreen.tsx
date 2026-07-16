import { useEffect, useState } from "react";
import {
  presetsDelete,
  presetsDuplicate,
  presetsExport,
  presetsImport,
  presetsUpdate,
  type PresetSummary,
} from "../api";
import { TIERS, type Tier } from "../lib/presets";

interface PresetsScreenProps {
  presets: PresetSummary[];
  onChanged: () => void;
}

/** The Presets manager (04 §S6): cards for shipped + user presets. Shipped presets are
 * read-only (duplicate to customize); user presets can be edited, deleted, and exported.
 * The selected preset's id is the same `preset_ref` the Master tab and Batch/Watch
 * screens send to `master`. */
export default function PresetsScreen({ presets, onChanged }: PresetsScreenProps) {
  const [selectedRef, setSelectedRef] = useState<string | null>(null);
  const [duplicateName, setDuplicateName] = useState("");
  const [editName, setEditName] = useState("");
  const [editTier, setEditTier] = useState<Tier>("standard");
  const [editTargetLufs, setEditTargetLufs] = useState(-16);
  const [editCeiling, setEditCeiling] = useState(-1);
  const [exportDest, setExportDest] = useState("");
  const [importPath, setImportPath] = useState("");
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);

  const selected = presets.find((p) => p.preset_ref === selectedRef) ?? null;

  useEffect(() => {
    if (!selected) return;
    setDuplicateName(`${selected.name} copy`);
    setEditName(selected.name);
    setEditTier(selected.tier);
    setEditTargetLufs(selected.target_lufs);
    setEditCeiling(selected.true_peak_ceiling_dbtp);
    setExportDest(`${selected.name.replace(/[\\/:*?"<>|]/g, "_")}.anvilpreset`);
    setConfirmingDelete(false);
  }, [selected]);

  const run = async (action: () => Promise<void>) => {
    setError(null);
    setStatus(null);
    try {
      await action();
    } catch (e) {
      setError(typeof e === "string" ? e : "That didn't work.");
    }
  };

  const handleDuplicate = () =>
    run(async () => {
      if (!selected) return;
      const created = await presetsDuplicate(selected.preset_ref, duplicateName || `${selected.name} copy`);
      onChanged();
      setSelectedRef(created.preset_ref);
      setStatus(`Duplicated as "${created.name}".`);
    });

  const handleSaveEdit = () =>
    run(async () => {
      if (!selected) return;
      await presetsUpdate(selected.preset_ref, {
        name: editName,
        tier: editTier,
        target_lufs: editTargetLufs,
        true_peak_ceiling_dbtp: editCeiling,
      });
      onChanged();
      setStatus("Saved.");
    });

  const handleDelete = () =>
    run(async () => {
      if (!selected) return;
      await presetsDelete(selected.preset_ref);
      onChanged();
      setSelectedRef(null);
      setStatus("Deleted.");
    });

  const handleExport = () =>
    run(async () => {
      if (!selected || !exportDest.trim()) return;
      await presetsExport(selected.preset_ref, exportDest);
      setStatus(`Exported to ${exportDest}.`);
    });

  const handleImport = () =>
    run(async () => {
      if (!importPath.trim()) return;
      const created = await presetsImport(importPath);
      onChanged();
      setImportPath("");
      setSelectedRef(created.preset_ref);
      setStatus(`Imported "${created.name}".`);
    });

  return (
    <div className="flex flex-1 flex-col gap-4 overflow-y-auto p-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-lg font-semibold">Presets</h1>
          <p className="text-xs text-neutral-500 dark:text-neutral-400">
            Shipped presets are read-only — duplicate one to make your own. Your presets feed
            the Master tab, Batch, and Watch rules.
          </p>
        </div>
        <div className="flex items-end gap-2">
          <label className="flex flex-col gap-1">
            <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Import .anvilpreset</span>
            <input
              type="text"
              value={importPath}
              onChange={(e) => setImportPath(e.target.value)}
              placeholder="Path to an exported preset file"
              className="w-64 rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
            />
          </label>
          <button
            type="button"
            onClick={() => void handleImport()}
            className="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
          >
            Import
          </button>
        </div>
      </div>

      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
        {presets.map((p) => (
          <button
            key={p.preset_ref}
            type="button"
            onClick={() => setSelectedRef(p.preset_ref)}
            className={`flex flex-col gap-1 rounded-xl border p-4 text-left transition-colors ${
              selectedRef === p.preset_ref
                ? "border-emerald-500 bg-emerald-500/5"
                : "border-neutral-200 hover:border-neutral-300 dark:border-neutral-800 dark:hover:border-neutral-700"
            }`}
          >
            <div className="flex items-center justify-between gap-2">
              <span className="font-medium">{p.name}</span>
              <span
                className={`shrink-0 rounded-full px-2 py-0.5 text-[10px] font-medium ${
                  p.source === "shipped"
                    ? "bg-neutral-200 text-neutral-600 dark:bg-neutral-800 dark:text-neutral-300"
                    : "bg-emerald-500/10 text-emerald-600 dark:text-emerald-400"
                }`}
              >
                {p.source === "shipped" ? "Shipped" : "Yours"}
              </span>
            </div>
            <span className="text-xs text-neutral-500 dark:text-neutral-400">
              {p.target_lufs.toFixed(1)} LUFS · ceiling {p.true_peak_ceiling_dbtp.toFixed(1)} dBTP · {p.tier}
            </span>
          </button>
        ))}
      </div>

      {selected && (
        <section className="flex flex-col gap-4 rounded-xl border border-neutral-200 p-4 dark:border-neutral-800">
          <div className="flex items-center justify-between">
            <h2 className="text-sm font-semibold">{selected.name}</h2>
            <span className="text-xs text-neutral-500 dark:text-neutral-400">{selected.preset_ref}</span>
          </div>

          {error && <p className="text-xs text-red-500">{error}</p>}
          {status && <p className="text-xs text-emerald-600 dark:text-emerald-400">{status}</p>}

          {selected.source === "user" && (
            <div className="flex flex-wrap items-end gap-3">
              <label className="flex flex-col gap-1">
                <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Name</span>
                <input
                  type="text"
                  value={editName}
                  onChange={(e) => setEditName(e.target.value)}
                  className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Tier</span>
                <select
                  value={editTier}
                  onChange={(e) => setEditTier(e.target.value as Tier)}
                  className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
                >
                  {TIERS.map((t) => (
                    <option key={t.id} value={t.id}>
                      {t.label}
                    </option>
                  ))}
                </select>
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Target LUFS</span>
                <input
                  type="number"
                  step={0.1}
                  value={editTargetLufs}
                  onChange={(e) => setEditTargetLufs(Number(e.target.value))}
                  className="w-24 rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Ceiling dBTP</span>
                <input
                  type="number"
                  step={0.1}
                  value={editCeiling}
                  onChange={(e) => setEditCeiling(Number(e.target.value))}
                  className="w-24 rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
                />
              </label>
              <button
                type="button"
                onClick={() => void handleSaveEdit()}
                className="rounded-lg bg-emerald-600 px-4 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500"
              >
                Save
              </button>
            </div>
          )}

          <div className="flex flex-wrap items-end gap-3 border-t border-neutral-200 pt-3 dark:border-neutral-800">
            <label className="flex flex-col gap-1">
              <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Duplicate as</span>
              <input
                type="text"
                value={duplicateName}
                onChange={(e) => setDuplicateName(e.target.value)}
                className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
              />
            </label>
            <button
              type="button"
              onClick={() => void handleDuplicate()}
              className="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
            >
              Duplicate
            </button>

            <label className="flex flex-col gap-1">
              <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Export to</span>
              <input
                type="text"
                value={exportDest}
                onChange={(e) => setExportDest(e.target.value)}
                className="w-56 rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
              />
            </label>
            <button
              type="button"
              onClick={() => void handleExport()}
              className="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
            >
              Export
            </button>

            {selected.source === "user" && (
              <button
                type="button"
                onClick={() => (confirmingDelete ? void handleDelete() : setConfirmingDelete(true))}
                className="ml-auto rounded-lg border border-red-500/40 px-3 py-1.5 text-xs font-medium text-red-600 hover:bg-red-500/10 dark:text-red-400"
              >
                {confirmingDelete ? "Really delete?" : "Delete"}
              </button>
            )}
          </div>
        </section>
      )}
    </div>
  );
}
