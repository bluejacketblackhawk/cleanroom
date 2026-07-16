import { useEffect, useState } from "react";
import {
  onWatchStatus,
  watchAddRule,
  watchListRules,
  watchRemoveRule,
  watchRetryUnreachable,
  watchSetEnabled,
  type PresetSummary,
  type WatchRuleStatus,
} from "../api";
import { DEFAULT_PRESET, DEFAULT_TIER, type Tier } from "../lib/presets";
import PresetPicker from "../components/PresetPicker";

interface WatchScreenProps {
  presets: PresetSummary[];
}

function patternLabel(rule: WatchRuleStatus["rule"]): string {
  if (rule.pattern === "any_supported") return "Any supported file";
  return rule.pattern.extensions.map((e) => `.${e}`).join(", ");
}

/** The Watch folders screen (04 §S5): rules (folder → preset → output dir → pattern →
 * on/off) driving `anvil_batch::WatchService`. New files dropped into a watched folder are
 * queued automatically once they've stopped changing size. */
export default function WatchScreen({ presets }: WatchScreenProps) {
  const [rules, setRules] = useState<WatchRuleStatus[]>([]);
  const [folder, setFolder] = useState("");
  const [outputDir, setOutputDir] = useState("");
  const [presetRef, setPresetRef] = useState(DEFAULT_PRESET);
  const [tier, setTier] = useState<Tier>(DEFAULT_TIER);
  const [extensions, setExtensions] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);

  useEffect(() => {
    watchListRules().then(setRules).catch(() => {});
    const unlisten = onWatchStatus(setRules);
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, []);

  useEffect(() => {
    if (presetRef === DEFAULT_PRESET && presets.length > 0 && !presets.some((p) => p.preset_ref === DEFAULT_PRESET)) {
      setPresetRef(presets[0].preset_ref);
    }
  }, [presets, presetRef]);

  const addRule = async () => {
    if (!folder.trim() || !outputDir.trim()) {
      setError("Set both a watch folder and an output folder.");
      return;
    }
    setError(null);
    setAdding(true);
    try {
      const exts = extensions
        .split(",")
        .map((s) => s.trim().replace(/^\./, ""))
        .filter(Boolean);
      await watchAddRule(folder, presetRef, tier, outputDir, exts.length > 0 ? exts : undefined);
      setFolder("");
      setOutputDir("");
      setExtensions("");
      watchListRules().then(setRules).catch(() => {});
    } catch (e) {
      setError(typeof e === "string" ? e : "Could not add that rule.");
    } finally {
      setAdding(false);
    }
  };

  const hasUnreachable = rules.some((r) => r.error);

  return (
    <div className="flex flex-1 flex-col gap-4 overflow-y-auto p-6">
      <div>
        <h1 className="text-lg font-semibold">Watch folders</h1>
        <p className="text-xs text-neutral-500 dark:text-neutral-400">
          New files dropped into a watched folder master themselves automatically, once
          they've stopped changing size. Files already there when you add a rule are left
          alone — use Batch for those.
        </p>
      </div>

      <div className="flex flex-col gap-3 rounded-lg border border-neutral-200 p-3 dark:border-neutral-800">
        <div className="flex flex-wrap gap-3">
          <label className="flex min-w-56 flex-1 flex-col gap-1">
            <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Watch folder</span>
            <input
              type="text"
              value={folder}
              onChange={(e) => setFolder(e.target.value)}
              placeholder="Folder to watch for new recordings"
              className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
            />
          </label>
          <label className="flex min-w-56 flex-1 flex-col gap-1">
            <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Output folder</span>
            <input
              type="text"
              value={outputDir}
              onChange={(e) => setOutputDir(e.target.value)}
              placeholder="Where mastered copies are written"
              className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
            />
          </label>
        </div>
        <div className="flex flex-wrap items-end gap-3">
          <PresetPicker
            presets={presets}
            presetRef={presetRef}
            tier={tier}
            onPresetChange={setPresetRef}
            onTierChange={setTier}
          />
          <label className="flex flex-col gap-1">
            <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">File types (optional)</span>
            <input
              type="text"
              value={extensions}
              onChange={(e) => setExtensions(e.target.value)}
              placeholder="wav, mp3"
              className="w-32 rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
            />
          </label>
          <button
            type="button"
            onClick={() => void addRule()}
            disabled={adding}
            className="rounded-lg bg-emerald-600 px-4 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
          >
            Add rule
          </button>
        </div>
        {error && <p className="text-xs text-red-500">{error}</p>}
      </div>

      {hasUnreachable && (
        <div className="flex items-center justify-between rounded-lg border border-amber-500/40 bg-amber-500/5 px-3 py-2 text-xs text-amber-700 dark:text-amber-300">
          <span>One or more watch folders are unreachable.</span>
          <button
            type="button"
            onClick={() => void watchRetryUnreachable()}
            className="font-medium underline"
          >
            Retry now
          </button>
        </div>
      )}

      <ul className="flex flex-col gap-2">
        {rules.length === 0 && (
          <li className="rounded-lg border border-dashed border-neutral-300 p-4 text-center text-xs text-neutral-500 dark:text-neutral-400 dark:border-neutral-700">
            No watch rules yet.
          </li>
        )}
        {rules.map((status) => (
          <li
            key={status.rule.id}
            className="flex flex-wrap items-center gap-3 rounded-lg border border-neutral-200 p-3 text-xs dark:border-neutral-800"
          >
            <div className="min-w-0 flex-1">
              <p className="truncate font-medium" title={status.rule.folder}>
                {status.rule.folder}
              </p>
              <p className="truncate text-neutral-500 dark:text-neutral-400">
                {status.rule.preset.name} · {patternLabel(status.rule)} → {status.rule.output_dir}
              </p>
            </div>
            <span
              className={`shrink-0 rounded-full px-2 py-0.5 text-[10px] font-medium ${
                status.error
                  ? "bg-amber-500/10 text-amber-600 dark:text-amber-400"
                  : status.rule.enabled
                    ? "bg-emerald-500/10 text-emerald-600 dark:text-emerald-400"
                    : "bg-neutral-200 text-neutral-500 dark:text-neutral-400 dark:bg-neutral-800"
              }`}
              title={status.error ?? undefined}
            >
              {status.error ? "Unreachable" : status.rule.enabled ? "Watching" : "Paused"}
            </span>
            <button
              type="button"
              onClick={() => void watchSetEnabled(status.rule.id, !status.rule.enabled)}
              className="shrink-0 rounded-lg border border-neutral-300 px-2.5 py-1 font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
            >
              {status.rule.enabled ? "Turn off" : "Turn on"}
            </button>
            <button
              type="button"
              onClick={() => void watchRemoveRule(status.rule.id)}
              className="shrink-0 text-neutral-500 dark:text-neutral-400 hover:text-red-500"
            >
              Remove
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
