import type { ModuleReport } from "../api";

interface ModuleChipsProps {
  modules: ModuleReport[];
  onToggle: (index: number) => void;
  onStrength: (index: number, value: number) => void;
  advanced: boolean;
  onToggleAdvanced: () => void;
}

/**
 * Progressive disclosure for "what we did" (04 §S2 Master tab): collapsed to a plain-
 * sentence summary by default, an "Advanced" disclosure expands per-module chips with an
 * on/off toggle and a strength slider where the module has one.
 */
export default function ModuleChips({
  modules,
  onToggle,
  onStrength,
  advanced,
  onToggleAdvanced,
}: ModuleChipsProps) {
  const engaged = modules.filter((m) => m.engaged);
  const summary = engaged.length
    ? engaged.map((m) => m.detail).join(" ")
    : "Nothing needed changing.";

  return (
    <div className="flex flex-col gap-2">
      <p className="text-sm leading-relaxed text-neutral-600 dark:text-neutral-300">
        {summary}
      </p>
      <button
        type="button"
        onClick={onToggleAdvanced}
        aria-expanded={advanced}
        className="flex w-fit items-center gap-1 text-xs font-medium text-neutral-500 dark:text-neutral-400 hover:text-neutral-800 dark:hover:text-neutral-200"
      >
        <span
          aria-hidden="true"
          className={`inline-block transition-transform ${advanced ? "rotate-90" : ""}`}
        >
          ›
        </span>
        Advanced
      </button>
      {advanced && (
        <ul className="flex flex-col gap-2 rounded-lg border border-neutral-200 p-3 dark:border-neutral-800">
          {modules.map((m, i) => (
            <li
              key={m.name}
              className="flex flex-col gap-1.5 border-b border-neutral-100 pb-2 last:border-0 last:pb-0 dark:border-neutral-800/60"
            >
              <div className="flex items-center justify-between gap-2">
                <div className="min-w-0">
                  <p className="truncate text-sm font-medium">{m.name}</p>
                  <p className="truncate text-xs text-neutral-500 dark:text-neutral-400">{m.detail}</p>
                </div>
                <button
                  type="button"
                  role="switch"
                  aria-checked={m.engaged}
                  aria-label={`${m.engaged ? "Disable" : "Enable"} ${m.name}`}
                  onClick={() => onToggle(i)}
                  className={`relative h-5 w-9 shrink-0 rounded-full transition-colors ${
                    m.engaged ? "bg-emerald-600" : "bg-neutral-300 dark:bg-neutral-700"
                  }`}
                >
                  <span
                    aria-hidden="true"
                    className={`absolute top-0.5 h-4 w-4 rounded-full bg-white transition-transform ${
                      m.engaged ? "translate-x-4" : "translate-x-0.5"
                    }`}
                  />
                </button>
              </div>
              {m.strength !== null && (
                <label className="flex items-center gap-2 text-xs text-neutral-500 dark:text-neutral-400">
                  <span className="w-14 shrink-0">Strength</span>
                  <input
                    type="range"
                    min={0}
                    max={100}
                    value={Math.round(m.strength * 100)}
                    disabled={!m.engaged}
                    onChange={(e) => onStrength(i, Number(e.target.value) / 100)}
                    aria-valuetext={`${Math.round(m.strength * 100)} percent`}
                    aria-label={`${m.name} strength`}
                    className="h-1.5 flex-1 accent-emerald-600 disabled:opacity-40"
                  />
                  <span className="w-9 shrink-0 text-right tabular-nums">
                    {Math.round(m.strength * 100)}%
                  </span>
                </label>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
