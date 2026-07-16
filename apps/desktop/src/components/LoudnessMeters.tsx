import type { LoudnessTriple } from "../api";

interface MeterRowProps {
  label: string;
  before: number;
  after: number;
  unit: string;
  /** [min, max] used to normalize the bar fill — not the file's real range. */
  domain: [number, number];
}

function MeterRow({ label, before, after, unit, domain }: MeterRowProps) {
  const clamp = (v: number) =>
    Math.min(1, Math.max(0, (v - domain[0]) / (domain[1] - domain[0])));
  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-baseline justify-between text-xs">
        <span className="font-medium text-neutral-500 dark:text-neutral-400">{label}</span>
        <span className="tabular-nums text-neutral-700 dark:text-neutral-300">
          {before.toFixed(1)} {"→"}{" "}
          <span className="font-semibold text-emerald-600 dark:text-emerald-400">
            {after.toFixed(1)}
          </span>{" "}
          {unit}
        </span>
      </div>
      <div className="relative h-1.5 overflow-hidden rounded-full bg-neutral-200 dark:bg-neutral-800">
        <div
          className="absolute inset-y-0 left-0 rounded-full bg-neutral-400 dark:bg-neutral-600"
          style={{ width: `${clamp(before) * 100}%` }}
        />
        <div
          className="absolute inset-y-0 left-0 rounded-full bg-emerald-500/85"
          style={{ width: `${clamp(after) * 100}%` }}
        />
      </div>
    </div>
  );
}

interface LoudnessMetersProps {
  before: LoudnessTriple;
  after: LoudnessTriple;
}

/** Before/after loudness meters: integrated LUFS, true peak, loudness range (04 §S2). */
export default function LoudnessMeters({ before, after }: LoudnessMetersProps) {
  return (
    <div
      className="flex flex-col gap-3"
      role="group"
      aria-label="Loudness, before and after mastering"
    >
      <MeterRow
        label="Loudness"
        before={before.integrated_lufs}
        after={after.integrated_lufs}
        unit="LUFS"
        domain={[-36, -6]}
      />
      <MeterRow
        label="Peak"
        before={before.true_peak_dbtp}
        after={after.true_peak_dbtp}
        unit="dBTP"
        domain={[-12, 0]}
      />
      <MeterRow
        label="Range"
        before={before.loudness_range_lu}
        after={after.loudness_range_lu}
        unit="LU"
        domain={[0, 20]}
      />
    </div>
  );
}
