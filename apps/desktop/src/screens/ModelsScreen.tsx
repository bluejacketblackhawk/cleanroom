import { formatBytes } from "../lib/format";
import { useModelDownloads } from "../lib/useModelDownloads";
import type { ModelPack } from "../api";

const KIND_LABEL: Record<ModelPack["kind"], string> = {
  denoise: "Denoise",
  asr: "Transcription",
  llm: "Shownotes",
  diarize: "Speakers",
};

/** Models manager (04 §S7): what's running locally, honestly. RNNoise is always
 * installed (compiled in); whisper packs (M3) and the shownotes LLM (M4, coming soon)
 * show a real size + license and, where downloadable, a real download button with
 * progress and resume. */
export default function ModelsScreen() {
  const { models, progress, start, cancel } = useModelDownloads();

  const totalInstalledBytes = models
    .filter((m) => m.installed && m.downloadable)
    .reduce((sum, m) => sum + m.size_bytes, 0);

  return (
    <div className="flex flex-1 flex-col gap-4 overflow-y-auto p-6">
      <div>
        <h1 className="text-lg font-semibold">Models</h1>
        <p className="text-xs text-neutral-500 dark:text-neutral-400">
          Everything Cleanroom runs, runs on this machine. Nothing here ever phones home.
          {totalInstalledBytes > 0 && ` ${formatBytes(totalInstalledBytes)} downloaded so far.`}
        </p>
      </div>

      <ul className="flex flex-col gap-3">
        {models.map((m) => {
          const dl = progress[m.id];
          const isActive = dl && (dl.status === "downloading" || dl.status === "verifying");
          const isPaused = dl?.status === "paused";
          const fraction =
            dl && dl.total_bytes > 0 ? Math.min(1, dl.downloaded_bytes / dl.total_bytes) : 0;

          return (
            <li
              key={m.id}
              className="flex flex-wrap items-center gap-4 rounded-xl border border-neutral-200 p-4 dark:border-neutral-800"
            >
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-medium">{m.name}</span>
                  <span className="rounded-full bg-neutral-100 px-2 py-0.5 text-[10px] font-medium text-neutral-500 dark:text-neutral-400 dark:bg-neutral-800">
                    {KIND_LABEL[m.kind]}
                  </span>
                  <span
                    className={`rounded-full px-2 py-0.5 text-[10px] font-medium ${
                      m.installed
                        ? "bg-emerald-500/10 text-emerald-600 dark:text-emerald-400"
                        : "bg-neutral-200 text-neutral-500 dark:text-neutral-400 dark:bg-neutral-800"
                    }`}
                  >
                    {m.installed ? "Installed" : "Not installed"}
                  </span>
                </div>
                <p className="mt-1 text-xs text-neutral-500 dark:text-neutral-400">{m.detail}</p>
                <p className="mt-1 text-xs text-neutral-500 dark:text-neutral-400">
                  {m.size} · {m.license}
                </p>
                {dl && (isActive || isPaused || dl.status === "error") && (
                  <div className="mt-2 flex flex-col gap-1">
                    <div className="h-1.5 w-full max-w-xs overflow-hidden rounded-full bg-neutral-200 dark:bg-neutral-800">
                      <div
                        className={`h-full transition-[width] ${isPaused ? "bg-neutral-400" : "bg-emerald-500"}`}
                        style={{ width: `${Math.round(fraction * 100)}%` }}
                      />
                    </div>
                    <p className="text-[11px] text-neutral-500 dark:text-neutral-400">
                      {dl.status === "verifying"
                        ? "Verifying…"
                        : dl.status === "error"
                          ? (dl.message ?? "Download failed.")
                          : `${formatBytes(dl.downloaded_bytes)} of ${formatBytes(dl.total_bytes)}${isPaused ? " · paused" : ""}`}
                    </p>
                  </div>
                )}
              </div>

              {m.installed ? (
                <span className="shrink-0 rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-500 dark:text-neutral-400 dark:border-neutral-700">
                  Installed
                </span>
              ) : m.installer_provisioned ? (
                <span
                  title="Provisioned by the app installer, alongside the speaker-ID component."
                  className="shrink-0 rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-500 dark:text-neutral-400 dark:border-neutral-700"
                >
                  Comes with the app
                </span>
              ) : !m.downloadable ? (
                <button
                  type="button"
                  disabled
                  title={`Install lands in ${m.arrives ?? "a later release"}`}
                  className="shrink-0 cursor-not-allowed rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-500 dark:text-neutral-400 dark:border-neutral-700"
                >
                  {m.arrives ? `Coming in ${m.arrives}` : "Install"}
                </button>
              ) : isActive ? (
                <button
                  type="button"
                  onClick={() => cancel(m.id)}
                  className="shrink-0 rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
                >
                  Pause
                </button>
              ) : (
                <button
                  type="button"
                  onClick={() => start(m.id)}
                  className="shrink-0 rounded-lg bg-emerald-600 px-3 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500"
                >
                  {isPaused || m.downloaded_bytes > 0 ? "Resume download" : `Download (${m.size})`}
                </button>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}
