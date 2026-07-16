import type { Cut } from "../api";
import { formatTime } from "../lib/format";

interface CutReviewListProps {
  cuts: Cut[];
  accepted: Set<number>;
  onToggle: (index: number) => void;
  onPlayInContext: (cut: Cut) => void;
  onApplySafeSet: () => void;
  onApply: () => void;
  applying: boolean;
  planning: boolean;
}

const KIND_LABEL: Record<Cut["kind"], string> = { silence: "Silence", filler: "Filler" };

/**
 * Filler/silence review list (04 §S2 Transcript tab): each cut gets play-in-context and
 * accept/reject, plus a bulk "apply safe set" (silence cuts only — filler removal stays a
 * manual per-word review since it edits speech, not just gaps).
 */
export default function CutReviewList({
  cuts,
  accepted,
  onToggle,
  onPlayInContext,
  onApplySafeSet,
  onApply,
  applying,
  planning,
}: CutReviewListProps) {
  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          onClick={onApplySafeSet}
          disabled={cuts.length === 0 || applying}
          title="Accepts every silence cut (not filler words) and applies immediately"
          className="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 disabled:cursor-not-allowed disabled:opacity-40 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
        >
          Apply safe set
        </button>
        <button
          type="button"
          onClick={onApply}
          disabled={cuts.length === 0 || applying}
          className="ml-auto rounded-lg bg-emerald-600 px-4 py-1.5 text-xs font-semibold text-white transition-colors hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {applying ? "Applying…" : `Apply (${accepted.size} accepted)`}
        </button>
      </div>

      {planning && <p className="text-xs text-neutral-500 dark:text-neutral-400">Listening for gaps and filler words…</p>}
      {!planning && cuts.length === 0 && (
        <p className="rounded-lg border border-dashed border-neutral-300 p-4 text-center text-xs text-neutral-500 dark:text-neutral-400 dark:border-neutral-700">
          No cuts planned yet. Pick what to look for above and find cuts.
        </p>
      )}

      <ul className="flex flex-col gap-2">
        {cuts.map((cut, i) => {
          const isAccepted = accepted.has(i);
          return (
            <li
              key={i}
              className="flex flex-wrap items-center gap-3 rounded-lg border border-neutral-200 p-3 text-xs dark:border-neutral-800"
            >
              <span
                className={`shrink-0 rounded-full px-2 py-0.5 font-medium ${
                  cut.kind === "silence"
                    ? "bg-emerald-500/10 text-emerald-600 dark:text-emerald-400"
                    : "bg-rose-500/10 text-rose-600 dark:text-rose-400"
                }`}
              >
                {KIND_LABEL[cut.kind]}
              </span>
              <span className="shrink-0 tabular-nums text-neutral-500 dark:text-neutral-400">
                {formatTime(cut.start)}–{formatTime(cut.end)}
              </span>
              <span className="min-w-0 flex-1 truncate" title={cut.label}>
                {cut.label}
              </span>
              <button
                type="button"
                onClick={() => onPlayInContext(cut)}
                className="shrink-0 font-medium text-neutral-500 dark:text-neutral-400 hover:text-neutral-800 dark:hover:text-neutral-200"
                aria-label={`Play ${cut.label} in context`}
              >
                ▶ Play
              </button>
              <button
                type="button"
                role="switch"
                aria-checked={isAccepted}
                aria-label={`${isAccepted ? "Reject" : "Accept"} ${cut.label}`}
                onClick={() => onToggle(i)}
                className={`shrink-0 rounded-full border px-2.5 py-1 font-medium transition-colors ${
                  isAccepted
                    ? "border-emerald-500/50 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400"
                    : "border-neutral-300 text-neutral-500 dark:border-neutral-700 dark:text-neutral-400"
                }`}
              >
                {isAccepted ? "Accepted" : "Rejected"}
              </button>
            </li>
          );
        })}
      </ul>
    </div>
  );
}
