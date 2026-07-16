import { useCallback, useEffect, useRef, useState } from "react";
import {
  guardClapTest,
  guardListDevices,
  guardMeter,
  guardStart,
  guardStop,
  type ClapResult,
  type GuardDevice,
  type GuardMeter,
} from "../api";

const POLL_INTERVAL_MS = 150;
const CLAP_TEST_SECS = 3;

const HEADROOM_COLOR: Record<string, string> = {
  hot: "bg-red-500",
  good: "bg-emerald-500",
  quiet: "bg-amber-400",
  silent: "bg-neutral-400",
};

const NOISE_COLOR: Record<string, string> = {
  quiet: "text-emerald-600 dark:text-emerald-400",
  some_hiss: "text-amber-500",
  noisy: "text-red-500",
};

/** Map a headroom-level "hot"..."silent" onto a 0-100 bar fill, clamped, using the same
 * dBFS bands `guard.rs::headroom_for` uses so the bar and the message never disagree. */
function meterFill(peakDbfs: number): number {
  const pct = ((peakDbfs + 60) / 60) * 100; // -60 dBFS -> 0%, 0 dBFS -> 100%
  return Math.max(2, Math.min(100, pct));
}

/**
 * S10 Recording Guard (04 §S10): a live input meter with plain-word headroom guidance, a
 * continuously-tracked noise-floor reading, a clap-test room-echo estimate, and the
 * device/sample-rate in use. One screen, zero persistence — a pre-flight check, not a
 * recorder (nothing here is saved to disk).
 */
export default function RecordingGuardScreen() {
  const [devices, setDevices] = useState<GuardDevice[]>([]);
  const [selectedDevice, setSelectedDevice] = useState<string>("");
  const [running, setRunning] = useState(false);
  const [meter, setMeter] = useState<GuardMeter | null>(null);
  const [startError, setStartError] = useState<string | null>(null);
  const [clapRunning, setClapRunning] = useState(false);
  const [clapResult, setClapResult] = useState<ClapResult | null>(null);
  const [clapError, setClapError] = useState<string | null>(null);

  const pollRef = useRef<number | undefined>(undefined);

  useEffect(() => {
    guardListDevices()
      .then((list) => {
        setDevices(list);
        const def = list.find((d) => d.is_default);
        if (def) setSelectedDevice(def.name);
      })
      .catch(() => {});
    // Zero-persistence: stop the input stream when the screen unmounts (e.g. nav away).
    return () => {
      void guardStop().catch(() => {});
    };
  }, []);

  useEffect(() => {
    if (!running) {
      if (pollRef.current !== undefined) window.clearInterval(pollRef.current);
      return;
    }
    const tick = () => {
      guardMeter()
        .then(setMeter)
        .catch(() => {});
    };
    tick();
    pollRef.current = window.setInterval(tick, POLL_INTERVAL_MS);
    return () => {
      if (pollRef.current !== undefined) window.clearInterval(pollRef.current);
    };
  }, [running]);

  const handleStart = useCallback(async () => {
    setStartError(null);
    try {
      const m = await guardStart(selectedDevice || null);
      setMeter(m);
      setRunning(true);
    } catch (e) {
      setStartError(typeof e === "string" ? e : "Could not start the input meter.");
    }
  }, [selectedDevice]);

  const handleStop = useCallback(async () => {
    setRunning(false);
    setMeter(null);
    try {
      await guardStop();
    } catch {
      // ignore
    }
  }, []);

  const handleClapTest = useCallback(async () => {
    setClapRunning(true);
    setClapError(null);
    setClapResult(null);
    try {
      const result = await guardClapTest(CLAP_TEST_SECS);
      setClapResult(result);
      if (!result.ok) setClapError(result.message);
    } catch (e) {
      setClapError(typeof e === "string" ? e : "Could not run the clap test.");
    } finally {
      setClapRunning(false);
    }
  }, []);

  return (
    <main className="flex flex-1 flex-col gap-5 overflow-y-auto p-6">
      <div>
        <h1 className="text-lg font-semibold">Recording Guard</h1>
        <p className="text-xs text-neutral-500 dark:text-neutral-400">
          A pre-flight check, not a recorder — nothing here is saved. Check your levels,
          then go record in whatever app you normally use.
        </p>
      </div>

      <div className="flex flex-wrap items-end gap-3 rounded-lg border border-neutral-200 p-3 dark:border-neutral-800">
        <label className="flex min-w-56 flex-1 flex-col gap-1">
          <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Input device</span>
          <select
            value={selectedDevice}
            onChange={(e) => setSelectedDevice(e.target.value)}
            disabled={running}
            className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs disabled:opacity-50 dark:border-neutral-700 dark:bg-neutral-900"
          >
            {devices.length === 0 && <option value="">No input devices found</option>}
            {devices.map((d) => (
              <option key={d.name} value={d.name}>
                {d.name}
                {d.is_default ? " (default)" : ""}
              </option>
            ))}
          </select>
        </label>
        {running ? (
          <button
            type="button"
            onClick={() => void handleStop()}
            className="rounded-lg border border-neutral-300 px-4 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
          >
            Stop
          </button>
        ) : (
          <button
            type="button"
            onClick={() => void handleStart()}
            disabled={devices.length === 0}
            className="rounded-lg bg-emerald-600 px-4 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
          >
            Start meter
          </button>
        )}
      </div>
      {startError && <p className="text-xs text-red-500">{startError}</p>}

      {meter && (
        <>
          <section className="flex flex-col gap-2 rounded-lg border border-neutral-200 p-4 dark:border-neutral-800">
            <div className="flex items-baseline justify-between">
              <span className="text-xs font-medium uppercase tracking-wide text-neutral-500 dark:text-neutral-400">
                Level
              </span>
              <span className="text-xs text-neutral-500 dark:text-neutral-400">
                {meter.device_name} · {(meter.sample_rate / 1000).toFixed(1)} kHz ·{" "}
                {meter.channels === 1 ? "mono" : `${meter.channels} ch`}
              </span>
            </div>
            <div
              role="meter"
              aria-label="Input level"
              aria-valuemin={-60}
              aria-valuemax={0}
              aria-valuenow={Math.round(meter.peak_dbfs)}
              className="h-4 w-full overflow-hidden rounded-full bg-neutral-200 dark:bg-neutral-800"
            >
              <div
                className={`h-full transition-[width] ${HEADROOM_COLOR[meter.headroom_level] ?? "bg-neutral-400"}`}
                style={{ width: `${meterFill(meter.peak_dbfs)}%` }}
              />
            </div>
            <p className="text-sm leading-relaxed text-neutral-700 dark:text-neutral-300">
              {meter.headroom_message}
            </p>
          </section>

          <section className="flex flex-col gap-1 rounded-lg border border-neutral-200 p-4 dark:border-neutral-800">
            <span className="text-xs font-medium uppercase tracking-wide text-neutral-500 dark:text-neutral-400">
              Noise floor
            </span>
            <p className={`text-sm leading-relaxed ${NOISE_COLOR[meter.noise_rating] ?? "text-neutral-600"}`}>
              {meter.noise_message}
            </p>
          </section>

          <section className="flex flex-col gap-2 rounded-lg border border-neutral-200 p-4 dark:border-neutral-800">
            <span className="text-xs font-medium uppercase tracking-wide text-neutral-500 dark:text-neutral-400">
              Room echo
            </span>
            <div className="flex items-center gap-3">
              <button
                type="button"
                onClick={() => void handleClapTest()}
                disabled={clapRunning}
                className="rounded-lg border border-neutral-300 px-4 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
              >
                {clapRunning ? `Listening… clap once (${CLAP_TEST_SECS}s)` : "Clap test"}
              </button>
              {clapResult?.ok && clapResult.rt60_secs != null && (
                <span className="text-xs text-neutral-500 dark:text-neutral-400" title={`RT60 ≈ ${clapResult.rt60_secs.toFixed(2)}s`}>
                  {clapResult.reverb_bucket}
                </span>
              )}
            </div>
            {clapResult?.ok && (
              <p className="text-sm leading-relaxed text-neutral-700 dark:text-neutral-300">
                {clapResult.message}
              </p>
            )}
            {clapError && <p className="text-xs text-neutral-500 dark:text-neutral-400">{clapError}</p>}
          </section>
        </>
      )}
    </main>
  );
}
