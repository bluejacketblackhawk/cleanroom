import type { MasterReport, PresetSummary } from "../api";
import { TIERS, type Tier } from "../lib/presets";
import HealthCard from "./HealthCard";
import LoudnessMeters from "./LoudnessMeters";
import ModuleChips from "./ModuleChips";

interface MasterPanelProps {
  hasMedia: boolean;
  tier: Tier;
  preset: string;
  presets: PresetSummary[];
  onTierChange: (t: Tier) => void;
  onPresetChange: (p: string) => void;
  onMaster: () => void;
  mastering: boolean;
  progressVerb: string;
  error: string | null;
  report: MasterReport | null;
  dirty: boolean;
  advanced: boolean;
  onToggleAdvanced: () => void;
  onToggleModule: (i: number) => void;
  onModuleStrength: (i: number, v: number) => void;
  onFix: (code: string) => void;
}

/** The Master tab (04 §S2, default tab): Health Card, tier/preset, the Master button,
 * before/after meters, and the module chip list — in that priority order. */
export default function MasterPanel({
  hasMedia,
  tier,
  preset,
  presets,
  onTierChange,
  onPresetChange,
  onMaster,
  mastering,
  progressVerb,
  error,
  report,
  dirty,
  advanced,
  onToggleAdvanced,
  onToggleModule,
  onModuleStrength,
  onFix,
}: MasterPanelProps) {
  const buttonLabel = mastering
    ? "Mastering…"
    : report
      ? dirty
        ? "Re-master (M)"
        : "Master again (M)"
      : "Master (M)";

  return (
    <div className="flex flex-col gap-5">
      {report && (
        <section aria-label="Health check">
          <HealthCard findings={report.health_card} onFix={onFix} disabled={mastering} />
        </section>
      )}

      <section className="flex flex-col gap-3">
        <div className="flex flex-col gap-1.5">
          <span
            id="tier-label"
            className="text-xs font-medium uppercase tracking-wide text-neutral-500 dark:text-neutral-400"
          >
            Tier
          </span>
          <div
            role="radiogroup"
            aria-labelledby="tier-label"
            className="grid grid-cols-3 gap-1 rounded-lg bg-neutral-100 p-1 dark:bg-neutral-900"
          >
            {TIERS.map((t) => (
              <button
                key={t.id}
                type="button"
                role="radio"
                aria-checked={tier === t.id}
                title={t.detail}
                onClick={() => onTierChange(t.id)}
                className={`rounded-md px-2 py-1.5 text-xs font-medium transition-colors ${
                  tier === t.id
                    ? "bg-white text-neutral-900 shadow-sm dark:bg-neutral-700 dark:text-white"
                    : "text-neutral-500 dark:text-neutral-400 hover:text-neutral-800 dark:hover:text-neutral-200"
                }`}
              >
                {t.label}
              </button>
            ))}
          </div>
        </div>

        <label className="flex flex-col gap-1.5">
          <span className="text-xs font-medium uppercase tracking-wide text-neutral-500 dark:text-neutral-400">
            Preset
          </span>
          <select
            value={preset}
            onChange={(e) => onPresetChange(e.target.value)}
            className="rounded-lg border border-neutral-300 bg-white px-3 py-2 text-sm dark:border-neutral-700 dark:bg-neutral-900"
          >
            {presets.map((p) => (
              <option key={p.preset_ref} value={p.preset_ref}>
                {p.name}
                {p.source === "user" ? " (yours)" : ""}
              </option>
            ))}
          </select>
        </label>

        <button
          type="button"
          onClick={onMaster}
          disabled={!hasMedia || mastering}
          className="rounded-lg bg-emerald-600 px-4 py-2.5 text-sm font-semibold text-white transition-colors hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {buttonLabel}
        </button>
        {mastering && (
          <div className="flex flex-col gap-1.5">
            <div
              role="progressbar"
              aria-label="Mastering"
              aria-busy="true"
              className="h-1.5 w-full overflow-hidden rounded-full bg-neutral-200 dark:bg-neutral-800"
            >
              <div className="cr-indeterminate h-full w-1/4 rounded-full bg-emerald-500" />
            </div>
            <p aria-live="polite" className="text-xs text-neutral-500 dark:text-neutral-400">
              {progressVerb}
            </p>
          </div>
        )}
        {error && <p className="text-xs text-red-500">{error}</p>}
      </section>

      {report && (
        <>
          <section className="flex flex-col gap-2">
            <h3 className="text-xs font-medium uppercase tracking-wide text-neutral-500 dark:text-neutral-400">
              Before / after
            </h3>
            <LoudnessMeters before={report.before} after={report.after} />
          </section>

          <section className="flex flex-col gap-2">
            <h3 className="text-xs font-medium uppercase tracking-wide text-neutral-500 dark:text-neutral-400">
              What we did
            </h3>
            <ModuleChips
              modules={report.modules}
              onToggle={onToggleModule}
              onStrength={onModuleStrength}
              advanced={advanced}
              onToggleAdvanced={onToggleAdvanced}
            />
          </section>
        </>
      )}
    </div>
  );
}
