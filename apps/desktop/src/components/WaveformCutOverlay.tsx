import type { Cut } from "../api";

interface WaveformCutOverlayProps {
  cuts: Cut[];
  accepted: Set<number>;
  durationSecs: number;
}

/**
 * Absolute-positioned strip over a `relative`-wrapped `<Waveform>` (04 §S2 "show accepted
 * cuts as strikethrough regions on the waveform"). Accepted cuts get a filled, colored
 * strikethrough; pending ones get a faint outline so their position is still visible while
 * under review.
 */
export default function WaveformCutOverlay({ cuts, accepted, durationSecs }: WaveformCutOverlayProps) {
  if (durationSecs <= 0 || cuts.length === 0) return null;
  return (
    <div className="pointer-events-none absolute inset-0" aria-hidden="true">
      {cuts.map((cut, i) => {
        const isAccepted = accepted.has(i);
        const left = `${Math.max(0, (cut.start / durationSecs) * 100)}%`;
        const width = `${Math.max(0.4, ((cut.end - cut.start) / durationSecs) * 100)}%`;
        const tone = cut.kind === "silence" ? "emerald" : "rose";
        return (
          <div
            key={i}
            title={`${cut.label}${isAccepted ? " (accepted)" : " (pending)"}`}
            className={
              isAccepted
                ? tone === "emerald"
                  ? "absolute top-0 h-full border-x border-emerald-500/60 bg-emerald-500/15"
                  : "absolute top-0 h-full border-x border-rose-500/60 bg-rose-500/15"
                : "absolute top-0 h-full border-x border-dashed border-neutral-400/40 dark:border-neutral-500/40"
            }
            style={{ left, width }}
          >
            {isAccepted && (
              <span
                className={
                  tone === "emerald"
                    ? "absolute left-0 right-0 top-1/2 h-px -translate-y-1/2 bg-emerald-600"
                    : "absolute left-0 right-0 top-1/2 h-px -translate-y-1/2 bg-rose-600"
                }
              />
            )}
          </div>
        );
      })}
    </div>
  );
}
