import * as Tooltip from "@radix-ui/react-tooltip";
import type { HealthFinding } from "../api";
import { FIX_LABELS } from "../lib/presets";
import { titleCase } from "../lib/format";

function fixLabel(code: string): string {
  return FIX_LABELS[code] ?? titleCase(code);
}

function SeverityIcon({ severity }: { severity: HealthFinding["severity"] }) {
  if (severity === "warn") {
    return (
      <svg
        aria-hidden="true"
        viewBox="0 0 20 20"
        className="mt-0.5 h-4 w-4 shrink-0 text-amber-500"
        fill="currentColor"
      >
        <path d="M10 2 1 18h18L10 2Zm0 5.5a1 1 0 0 1 1 1v4a1 1 0 1 1-2 0v-4a1 1 0 0 1 1-1ZM10 15a1.1 1.1 0 1 1 0-2.2A1.1 1.1 0 0 1 10 15Z" />
      </svg>
    );
  }
  return (
    <svg
      aria-hidden="true"
      viewBox="0 0 20 20"
      className="mt-0.5 h-4 w-4 shrink-0 text-sky-500"
      fill="currentColor"
    >
      <path d="M10 18a8 8 0 1 1 0-16 8 8 0 0 1 0 16Zm-1-9h2v6H9V9Zm0-4h2v2H9V5Z" />
    </svg>
  );
}

interface HealthCardProps {
  findings: HealthFinding[];
  onFix: (code: string) => void;
  disabled?: boolean;
}

/**
 * 3-6 rows, icon + plain sentence + detail on hover + optional "fix" chip
 * (04 §Health Card spec). Never a number without a word — numbers only ever appear
 * inside a full sentence here, never bare.
 */
export default function HealthCard({ findings, onFix, disabled }: HealthCardProps) {
  if (findings.length === 0) return null;
  return (
    <ul className="flex flex-col gap-2" aria-label="Health check">
      {findings.map((f, i) => (
        <li
          key={i}
          className="flex items-start gap-2 rounded-lg border border-neutral-200 bg-white/60 p-3 text-sm dark:border-neutral-800 dark:bg-neutral-900/60"
        >
          <SeverityIcon severity={f.severity} />
          <div className="min-w-0 flex-1">
            <Tooltip.Root>
              <Tooltip.Trigger asChild>
                <p
                  tabIndex={0}
                  className="cursor-default rounded font-medium leading-snug outline-none focus-visible:ring-2 focus-visible:ring-emerald-500"
                >
                  {/* The warn/info distinction is icon color/shape only (SeverityIcon is
                   * `aria-hidden`) — say it in words too, so it isn't sighted-only. */}
                  <span className="sr-only">{f.severity === "warn" ? "Warning: " : "Info: "}</span>
                  {f.title}
                </p>
              </Tooltip.Trigger>
              <Tooltip.Portal>
                <Tooltip.Content
                  side="top"
                  sideOffset={6}
                  className="z-50 max-w-xs rounded-md bg-neutral-900 px-3 py-2 text-xs text-white shadow-lg dark:bg-neutral-100 dark:text-neutral-900"
                >
                  {f.detail}
                  <Tooltip.Arrow className="fill-neutral-900 dark:fill-neutral-100" />
                </Tooltip.Content>
              </Tooltip.Portal>
            </Tooltip.Root>
          </div>
          {f.fix && (
            <button
              type="button"
              disabled={disabled}
              onClick={() => onFix(f.fix as string)}
              className="shrink-0 rounded-full border border-emerald-500/50 px-2.5 py-1 text-xs font-medium text-emerald-600 transition-colors hover:bg-emerald-500/10 disabled:cursor-not-allowed disabled:opacity-50 dark:text-emerald-400"
            >
              {fixLabel(f.fix)}
            </button>
          )}
        </li>
      ))}
    </ul>
  );
}
