import type { PresetSummary } from "../api";
import { TIERS, type Tier } from "../lib/presets";

interface PresetPickerProps {
  presets: PresetSummary[];
  presetRef: string;
  tier: Tier;
  onPresetChange: (v: string) => void;
  onTierChange: (v: Tier) => void;
}

/** Preset + Tier pair, shared by Batch and Watch — both feed the same shipped+user
 * preset list a Master run uses (04 §S6 "feed the selected preset into Master + Batch"). */
export default function PresetPicker({
  presets,
  presetRef,
  tier,
  onPresetChange,
  onTierChange,
}: PresetPickerProps) {
  return (
    <div className="flex flex-wrap items-end gap-2">
      <label className="flex flex-col gap-1">
        <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Preset</span>
        <select
          value={presetRef}
          onChange={(e) => onPresetChange(e.target.value)}
          className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
        >
          {presets.map((p) => (
            <option key={p.preset_ref} value={p.preset_ref}>
              {p.name}
              {p.source === "user" ? " (yours)" : ""}
            </option>
          ))}
        </select>
      </label>
      <label className="flex flex-col gap-1">
        <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Tier</span>
        <select
          value={tier}
          onChange={(e) => onTierChange(e.target.value as Tier)}
          className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
        >
          {TIERS.map((t) => (
            <option key={t.id} value={t.id}>
              {t.label}
            </option>
          ))}
        </select>
      </label>
    </div>
  );
}
