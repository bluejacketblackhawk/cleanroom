// Polls the engine's playhead in seconds — word-level follow doesn't need Waveform's 60fps
// interpolated canvas loop, just a steady tick, so this is a much simpler sibling of the
// polling logic in Waveform.tsx.
import { useEffect, useRef, useState } from "react";
import { playbackPosition } from "../api";

const POLL_INTERVAL_MS = 120;

export function usePlayhead(sampleRate: number, isPlaying: boolean): number {
  const [seconds, setSeconds] = useState(0);
  const rateRef = useRef(sampleRate);
  rateRef.current = sampleRate;

  useEffect(() => {
    let cancelled = false;
    let id: number | undefined;

    const tick = () => {
      playbackPosition()
        .then((frame) => {
          if (cancelled) return;
          setSeconds(rateRef.current > 0 ? frame / rateRef.current : 0);
        })
        .catch(() => {});
    };

    tick();
    if (isPlaying) {
      id = window.setInterval(tick, POLL_INTERVAL_MS);
    }
    return () => {
      cancelled = true;
      if (id !== undefined) window.clearInterval(id);
    };
  }, [isPlaying]);

  return seconds;
}
