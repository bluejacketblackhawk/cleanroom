import { useCallback, useEffect, useRef } from "react";
import { multitrackGetPeaks, type Peak } from "../api";

interface TrackLaneProps {
  trackId: string;
  /** Total length of the track in 48 kHz frames (see `anvil_media`'s internal-rate note —
   * every decoded track lands on the same 48 kHz domain, so `duration_secs * sample_rate`
   * from `TrackWire` reconstructs this). */
  totalFrames: number;
}

/**
 * A small, static waveform for one S3 track lane — no playhead, no click-to-seek, no A/B.
 * The track list can show many of these at once, so it stays much lighter than the
 * transport-synced `Waveform` used on the Master/Transcript screens (fetches peaks once,
 * re-fetches only on resize).
 */
export default function TrackLane({ trackId, totalFrames }: TrackLaneProps) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const peaksRef = useRef<Peak[]>([]);

  const draw = useCallback(() => {
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
    ctx.clearRect(0, 0, w, h);

    const peaks = peaksRef.current;
    const bins = peaks.length;
    if (bins === 0) return;

    const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
    ctx.fillStyle = dark ? "#34d399" : "#059669"; // emerald — matches the app accent
    const mid = h / 2;
    const half = h / 2;
    const barW = w / bins;
    for (let i = 0; i < bins; i++) {
      const [min, max] = peaks[i];
      const x = i * barW;
      const yTop = mid - max * half;
      const yBottom = mid - min * half;
      const barH = Math.max(1, yBottom - yTop);
      ctx.fillRect(x, yTop, Math.max(1, barW - dpr * 0.5), barH);
    }
  }, []);

  const refresh = useCallback(async () => {
    const canvas = canvasRef.current;
    if (!canvas || totalFrames <= 0) {
      peaksRef.current = [];
      draw();
      return;
    }
    const bins = Math.max(1, Math.floor(canvas.clientWidth));
    try {
      peaksRef.current = await multitrackGetPeaks(trackId, 0, totalFrames, bins);
    } catch {
      peaksRef.current = [];
    }
    draw();
  }, [trackId, totalFrames, draw]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ro = new ResizeObserver(() => void refresh());
    ro.observe(canvas);
    return () => ro.disconnect();
  }, [refresh]);

  return (
    <canvas
      ref={canvasRef}
      role="img"
      aria-label="Track waveform"
      className="h-12 w-full rounded bg-neutral-100 dark:bg-neutral-900"
    />
  );
}
