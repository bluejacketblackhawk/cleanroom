import { useCallback, useEffect, useRef } from "react";
import { getPeaks, getProcessedPeaks, playbackPosition, seek, type Peak } from "./api";
import { formatTime } from "./lib/format";

export type PeaksSource = "original" | "processed";

interface WaveformProps {
  /** Total length of the loaded file in 48 kHz frames. */
  totalFrames: number;
  /** Internal sample rate (frames per second) — used to predict the playhead at 60 fps. */
  sampleRate: number;
  /** Whether the engine is currently advancing. */
  isPlaying: boolean;
  /** Which buffer this lane draws — "original" or the mastered "processed" copy. */
  source: PeaksSource;
  /** When set, overlays the other buffer's envelope on top (A/B "overlay" view). */
  compareSource?: PeaksSource | null;
  /** Tailwind height class for the canvas; defaults to a full-size lane. */
  heightClassName?: string;
  /** Extra context appended to the accessible label (e.g. "-16.0 LUFS"). */
  ariaContext?: string;
  /** Fired when the user clicks to seek (frame in the 48 kHz domain). */
  onSeek: (frame: number) => void;
  /** Fired once when the playhead reaches the end of the file. */
  onReachedEnd: () => void;
}

// The Rust engine streams its playhead at ~30 Hz (ADR-010). We poll at that rate and
// interpolate between polls so the on-screen playhead stays smooth at ~60 fps.
const POLL_INTERVAL_MS = 33;

function fetchPeaksFor(
  source: PeaksSource,
  start: number,
  end: number,
  bins: number,
): Promise<Peak[]> {
  return source === "processed"
    ? getProcessedPeaks(start, end, bins)
    : getPeaks(start, end, bins);
}

/**
 * Canvas waveform: min/max bars from the Rust peaks pyramid, a played/unplayed split at
 * the playhead, click-to-seek, and a playhead animated from the engine's reported
 * position. Re-fetches peaks at the right resolution on resize (one bar per CSS pixel).
 *
 * Optionally overlays a second source's peaks (translucent) for the A/B "overlay" view —
 * `source` stays the played/unplayed lane, `compareSource` is drawn as a flat silhouette
 * on top so the two takes can be compared without switching tabs.
 */
export default function Waveform({
  totalFrames,
  sampleRate,
  isPlaying,
  source,
  compareSource = null,
  heightClassName = "h-40",
  ariaContext,
  onSeek,
  onReachedEnd,
}: WaveformProps) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const peaksRef = useRef<Peak[]>([]);
  const comparePeaksRef = useRef<Peak[]>([]);
  // Last engine-reported position and when we heard it, for interpolation.
  const posRef = useRef<{ frame: number; atMs: number }>({ frame: 0, atMs: 0 });
  const lastPollRef = useRef(0);
  const rafRef = useRef(0);
  const endedRef = useRef(false);
  // Latest props for the rAF loop without re-subscribing every render.
  const playingRef = useRef(isPlaying);
  const totalRef = useRef(totalFrames);
  const rateRef = useRef(sampleRate);

  playingRef.current = isPlaying;
  totalRef.current = totalFrames;
  rateRef.current = sampleRate;

  // Predicted playhead frame, clamped to the file.
  const predictFrame = useCallback((now: number) => {
    const { frame, atMs } = posRef.current;
    const predicted = playingRef.current
      ? frame + ((now - atMs) / 1000) * rateRef.current
      : frame;
    return Math.max(0, Math.min(totalRef.current, predicted));
  }, []);

  const draw = useCallback(
    (now: number) => {
      const canvas = canvasRef.current;
      if (!canvas) return;
      const ctx = canvas.getContext("2d");
      if (!ctx) return;

      const dpr = window.devicePixelRatio || 1;
      const cssW = canvas.clientWidth;
      const cssH = canvas.clientHeight;
      const w = Math.floor(cssW * dpr);
      const h = Math.floor(cssH * dpr);
      if (canvas.width !== w || canvas.height !== h) {
        canvas.width = w;
        canvas.height = h;
      }

      const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
      const played = "#10b981"; // emerald-500 (matches the "100% local" accent)
      const unplayed = dark ? "#475569" : "#cbd5e1";
      const compareColor = "rgba(56, 189, 248, 0.35)"; // sky-400, translucent comparison
      const playheadColor = dark ? "#f8fafc" : "#0f172a";
      const mid = h / 2;
      const half = h / 2;

      ctx.clearRect(0, 0, w, h);

      const frame = predictFrame(now);
      const playheadX = totalRef.current > 0 ? (frame / totalRef.current) * w : 0;

      // Comparison layer first, so the primary lane draws on top of it.
      const comparePeaks = comparePeaksRef.current;
      if (comparePeaks.length > 0) {
        const bins = comparePeaks.length;
        const barW = w / bins;
        ctx.fillStyle = compareColor;
        for (let i = 0; i < bins; i++) {
          const [min, max] = comparePeaks[i];
          const x = i * barW;
          const yTop = mid - max * half;
          const yBottom = mid - min * half;
          const barH = Math.max(1, yBottom - yTop);
          ctx.fillRect(x, yTop, Math.max(1, barW - dpr * 0.5), barH);
        }
      }

      const peaks = peaksRef.current;
      const bins = peaks.length;
      if (bins > 0) {
        const barW = w / bins;
        for (let i = 0; i < bins; i++) {
          const [min, max] = peaks[i];
          const x = i * barW;
          const yTop = mid - max * half;
          const yBottom = mid - min * half;
          ctx.fillStyle = x < playheadX ? played : unplayed;
          // At least 1px tall so silence still shows a centre line.
          const barH = Math.max(1, yBottom - yTop);
          ctx.fillRect(x, yTop, Math.max(1, barW - dpr * 0.5), barH);
        }
      }

      // Playhead.
      ctx.strokeStyle = playheadColor;
      ctx.lineWidth = Math.max(1, dpr);
      ctx.beginPath();
      ctx.moveTo(playheadX, 0);
      ctx.lineTo(playheadX, h);
      ctx.stroke();
    },
    [predictFrame],
  );

  // Fetch peaks at one bar per CSS pixel; re-run on size, source, or file change.
  const refreshPeaks = useCallback(async () => {
    const canvas = canvasRef.current;
    if (!canvas || totalFrames <= 0) {
      peaksRef.current = [];
      comparePeaksRef.current = [];
      return;
    }
    const bins = Math.max(1, Math.floor(canvas.clientWidth));
    try {
      peaksRef.current = await fetchPeaksFor(source, 0, totalFrames, bins);
    } catch {
      peaksRef.current = [];
    }
    if (compareSource) {
      try {
        comparePeaksRef.current = await fetchPeaksFor(compareSource, 0, totalFrames, bins);
      } catch {
        comparePeaksRef.current = [];
      }
    } else {
      comparePeaksRef.current = [];
    }
  }, [totalFrames, source, compareSource]);

  // Reset playhead whenever a new file loads; re-fetch peaks on file/source change.
  useEffect(() => {
    posRef.current = { frame: 0, atMs: performance.now() };
    endedRef.current = false;
    void refreshPeaks();
  }, [totalFrames, refreshPeaks]);

  // Redraw + re-fetch peaks on container resize.
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ro = new ResizeObserver(() => {
      void refreshPeaks();
    });
    ro.observe(canvas);
    return () => ro.disconnect();
  }, [refreshPeaks]);

  // Animation loop: poll the engine at ~30 Hz, draw every frame.
  useEffect(() => {
    const loop = (now: number) => {
      if (now - lastPollRef.current >= POLL_INTERVAL_MS) {
        lastPollRef.current = now;
        playbackPosition()
          .then((frame) => {
            posRef.current = { frame, atMs: performance.now() };
            if (
              playingRef.current &&
              totalRef.current > 0 &&
              frame >= totalRef.current &&
              !endedRef.current
            ) {
              endedRef.current = true;
              onReachedEnd();
            } else if (frame < totalRef.current) {
              endedRef.current = false;
            }
          })
          .catch(() => {});
      }
      draw(now);
      rafRef.current = requestAnimationFrame(loop);
    };
    rafRef.current = requestAnimationFrame(loop);
    return () => cancelAnimationFrame(rafRef.current);
  }, [draw, onReachedEnd]);

  const handleClick = useCallback(
    (e: React.MouseEvent<HTMLCanvasElement>) => {
      const canvas = canvasRef.current;
      if (!canvas || totalFrames <= 0) return;
      const rect = canvas.getBoundingClientRect();
      const ratio = (e.clientX - rect.left) / rect.width;
      const frame = Math.max(
        0,
        Math.min(totalFrames, Math.round(ratio * totalFrames)),
      );
      // Snap the local playhead immediately for responsiveness, then tell the engine.
      posRef.current = { frame, atMs: performance.now() };
      endedRef.current = false;
      void seek(frame);
      onSeek(frame);
    },
    [totalFrames, onSeek],
  );

  const durationSecs = sampleRate > 0 ? totalFrames / sampleRate : 0;
  const sourceLabel = source === "processed" ? "mastered" : "original";
  const ariaLabel = `Waveform (${sourceLabel}), duration ${formatTime(durationSecs)}.${
    ariaContext ? ` ${ariaContext}.` : ""
  } Click to seek.`;

  return (
    <canvas
      ref={canvasRef}
      onClick={handleClick}
      role="img"
      aria-label={ariaLabel}
      className={`w-full cursor-pointer rounded-lg bg-neutral-100 dark:bg-neutral-900 ${heightClassName}`}
    />
  );
}
