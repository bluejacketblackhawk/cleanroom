import { useEffect, useState } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import { check as checkForUpdate } from "@tauri-apps/plugin-updater";
import Switch from "../components/Switch";
import {
  exportDiagnostics,
  settingsGetIntegrationStatus,
  settingsSetAutostart,
  settingsSetContextMenu,
  settingsSetFileAssociations,
  type AppInfo,
} from "../api";

interface SettingsScreenProps {
  info: AppInfo | null;
}

type AsyncStatus = "idle" | "working" | "checking" | "done" | "error";

/** Settings (04 §S8), scoped to what M5 ships: Integration (Explorer context menu /
 * file associations / autostart — thin toggles over `anvil_core::platform`), Updates
 * (a manual "check now" against the GitHub Releases feed — see `tauri.conf.json`'s
 * `plugins.updater`, whose endpoint/pubkey are still owner TODOs, so this honestly
 * reports "not configured yet" rather than pretending to succeed), and About
 * (version + the diagnostics export, 05 §M5.F). Theme/language/processing-defaults/
 * folder-naming (the rest of 04 §S8) aren't part of this pass. */
export default function SettingsScreen({ info }: SettingsScreenProps) {
  // `info.platform` ("windows" | "macos" | "other") comes from the existing `app_info`
  // Tauri command (anvil_core::platform::current().name()), already piped into this
  // screen via props — reused here rather than adding a new detection mechanism.
  const isMac = info?.platform === "macos";

  const [contextMenu, setContextMenu] = useState(false);
  const [fileAssociations, setFileAssociations] = useState(false);
  const [autostart, setAutostart] = useState(false);
  const [integrationError, setIntegrationError] = useState<string | null>(null);

  const [diagStatus, setDiagStatus] = useState<AsyncStatus>("idle");
  const [diagMessage, setDiagMessage] = useState<string | null>(null);

  const [updateStatus, setUpdateStatus] = useState<AsyncStatus>("idle");
  const [updateMessage, setUpdateMessage] = useState<string | null>(null);

  useEffect(() => {
    // Only autostart has a cheap "is it actually on" read (one registry value); the
    // context-menu/file-association toggles are N per-extension keys with no single flag
    // to check, so those two just reflect the last thing this screen set (off by default,
    // same as a fresh install) rather than round-tripping through the registry on mount.
    settingsGetIntegrationStatus()
      .then((s) => setAutostart(s.autostart))
      .catch(() => {
        /* best-effort — the toggle still works, it just starts from "off" */
      });
  }, []);

  const handleContextMenuToggle = async (next: boolean) => {
    setContextMenu(next);
    setIntegrationError(null);
    try {
      await settingsSetContextMenu(next);
    } catch (e) {
      setContextMenu(!next);
      setIntegrationError(
        typeof e === "string" ? e : "Couldn't update the Explorer context menu entry.",
      );
    }
  };

  const handleFileAssociationsToggle = async (next: boolean) => {
    setFileAssociations(next);
    setIntegrationError(null);
    try {
      await settingsSetFileAssociations(next);
    } catch (e) {
      setFileAssociations(!next);
      setIntegrationError(typeof e === "string" ? e : "Couldn't update file associations.");
    }
  };

  const handleAutostartToggle = async (next: boolean) => {
    setAutostart(next);
    setIntegrationError(null);
    try {
      await settingsSetAutostart(next);
    } catch (e) {
      setAutostart(!next);
      setIntegrationError(typeof e === "string" ? e : "Couldn't update the autostart entry.");
    }
  };

  const handleExportDiagnostics = async () => {
    setDiagStatus("working");
    setDiagMessage(null);
    try {
      const suggested = `anvil-diagnostics-${new Date().toISOString().slice(0, 10)}.zip`;
      const target = await save({
        defaultPath: suggested,
        filters: [{ name: "Zip archive", extensions: ["zip"] }],
      });
      if (!target) {
        setDiagStatus("idle");
        return;
      }
      const result = await exportDiagnostics(target);
      setDiagStatus("done");
      const files = result.log_file_count === 1 ? "1 log file" : `${result.log_file_count} log files`;
      setDiagMessage(`Saved to ${result.zip_path} — ${files} plus basic system info. No audio, ever.`);
    } catch (e) {
      setDiagStatus("error");
      setDiagMessage(typeof e === "string" ? e : "Couldn't write the diagnostics zip.");
    }
  };

  const handleCheckUpdates = async () => {
    setUpdateStatus("checking");
    setUpdateMessage(null);
    try {
      const update = await checkForUpdate();
      if (update) {
        setUpdateStatus("done");
        setUpdateMessage(`Version ${update.version} is available.`);
      } else {
        setUpdateStatus("done");
        setUpdateMessage("You're on the latest version.");
      }
    } catch {
      setUpdateStatus("error");
      setUpdateMessage(
        "Couldn't reach the update feed — this build doesn't have one configured yet.",
      );
    }
  };

  // Integration section copy: Windows drives Explorer/registry hooks, macOS drives a
  // Finder Quick Action (Services) + LaunchAgent instead (crates/anvil-core/src/platform/
  // macos.rs) — same controls, different OS mechanism, so the copy has to match per platform.
  const integrationIntro = isMac
    ? "Optional macOS shell hooks. Off by default; each one only ever writes to Cleanroom's own files under ~/Library, and uninstalling removes all of them automatically."
    : "Optional Windows shell hooks. Off by default; each one only ever writes to Cleanroom's own per-user registry keys, and turning uninstall removes all of them automatically.";
  const contextMenuLabel = isMac ? "Finder Quick Action" : "Explorer context menu";
  const contextMenuDescription = isMac
    ? "Adds \"Master with Cleanroom\" to Finder's Quick Actions when you right-click a supported file."
    : "Adds \"Master with Cleanroom\" when you right-click a supported file.";
  const contextMenuAriaLabel = isMac
    ? "Finder Quick Action: Master with Cleanroom"
    : "Explorer context menu: Master with Cleanroom";
  const fileAssociationsDescription = isMac
    ? "Lists Cleanroom in the Finder \"Open With\" menu for audio and video files."
    : "Lists Cleanroom in Windows' \"Open with\" picker for audio and video files.";
  const autostartLabel = isMac ? "Start with macOS" : "Start with Windows";
  const autostartAriaLabel = isMac ? "Start Cleanroom with macOS" : "Start Cleanroom with Windows";

  // FFmpeg ships LGPL-3.0-or-later on Windows (BtbN's win64-lgpl prebuilt, --enable-version3)
  // and the stricter LGPL-2.1-or-later on macOS (built from source by
  // scripts/build-ffmpeg-macos.sh, no --enable-version3) — see scripts/ffmpeg-pin.json and
  // anvil_media::sidecar::FFMPEG_PINS, which this mirrors.
  const ffmpegLicense = isMac ? "LGPL-2.1-or-later" : "LGPL-3.0-or-later";

  return (
    <div className="flex flex-1 flex-col gap-6 overflow-y-auto p-6">
      <div>
        <h1 className="text-lg font-semibold">Settings</h1>
        <p className="text-xs text-neutral-500 dark:text-neutral-400">
          Everything below stays on this computer. Nothing here is ever uploaded.
        </p>
      </div>

      <section
        aria-labelledby="settings-integration-heading"
        className="flex flex-col gap-4 rounded-xl border border-neutral-200 p-4 dark:border-neutral-800"
      >
        <div>
          <h2 id="settings-integration-heading" className="text-sm font-semibold">
            Integration
          </h2>
          <p className="mt-0.5 text-xs text-neutral-500 dark:text-neutral-400">
            {integrationIntro}
          </p>
        </div>

        <div className="flex items-center justify-between gap-4">
          <div className="min-w-0">
            <p className="text-sm font-medium">{contextMenuLabel}</p>
            <p className="text-xs text-neutral-500 dark:text-neutral-400">
              {contextMenuDescription}
            </p>
          </div>
          <Switch
            checked={contextMenu}
            onChange={(v) => void handleContextMenuToggle(v)}
            label={contextMenuAriaLabel}
          />
        </div>

        <div className="flex items-center justify-between gap-4">
          <div className="min-w-0">
            <p className="text-sm font-medium">"Open with" file associations</p>
            <p className="text-xs text-neutral-500 dark:text-neutral-400">
              {fileAssociationsDescription}
            </p>
          </div>
          <Switch
            checked={fileAssociations}
            onChange={(v) => void handleFileAssociationsToggle(v)}
            label="Open with file associations"
          />
        </div>

        <div className="flex items-center justify-between gap-4">
          <div className="min-w-0">
            <p className="text-sm font-medium">{autostartLabel}</p>
            <p className="text-xs text-neutral-500 dark:text-neutral-400">
              Starts Cleanroom at login so watch folders keep mastering in the background.
            </p>
          </div>
          <Switch
            checked={autostart}
            onChange={(v) => void handleAutostartToggle(v)}
            label={autostartAriaLabel}
          />
        </div>

        {integrationError && (
          <p role="alert" className="text-xs text-red-500">
            {integrationError}
          </p>
        )}
      </section>

      <section
        aria-labelledby="settings-updates-heading"
        className="flex flex-col gap-3 rounded-xl border border-neutral-200 p-4 dark:border-neutral-800"
      >
        <div>
          <h2 id="settings-updates-heading" className="text-sm font-semibold">
            Updates
          </h2>
          <p className="mt-0.5 text-xs text-neutral-500 dark:text-neutral-400">
            Cleanroom only ever checks GitHub for a newer version — never your audio, never
            usage data.
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-3">
          <button
            type="button"
            onClick={() => void handleCheckUpdates()}
            disabled={updateStatus === "checking"}
            className="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-700 hover:bg-neutral-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-neutral-700 dark:text-neutral-200 dark:hover:bg-neutral-800"
          >
            {updateStatus === "checking" ? "Checking…" : "Check for updates"}
          </button>
          {updateMessage && (
            <span role="status" className="text-xs text-neutral-500 dark:text-neutral-400">
              {updateMessage}
            </span>
          )}
        </div>
      </section>

      <section
        aria-labelledby="settings-about-heading"
        className="flex flex-col gap-3 rounded-xl border border-neutral-200 p-4 dark:border-neutral-800"
      >
        <div>
          <h2 id="settings-about-heading" className="text-sm font-semibold">
            About
          </h2>
          <p className="mt-0.5 text-xs text-neutral-500 dark:text-neutral-400">
            {info
              ? `Cleanroom v${info.version} · chain v${info.chain_version} · ${info.platform}`
              : "Connecting to the engine…"}
          </p>
        </div>

        <div className="flex flex-col items-start gap-2">
          <button
            type="button"
            onClick={() => void handleExportDiagnostics()}
            disabled={diagStatus === "working"}
            className="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-700 hover:bg-neutral-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-neutral-700 dark:text-neutral-200 dark:hover:bg-neutral-800"
          >
            {diagStatus === "working" ? "Writing zip…" : "Export diagnostics"}
          </button>
          <p className="text-xs text-neutral-500 dark:text-neutral-400">
            Zips your recent logs and basic system info (OS, CPU count, app version) for a
            GitHub issue. Never your audio, never a project, never a file you opened.
          </p>
          {diagMessage && (
            <p
              role="status"
              className={`text-xs ${diagStatus === "error" ? "text-red-500" : "text-emerald-600 dark:text-emerald-400"}`}
            >
              {diagMessage}
            </p>
          )}
        </div>
      </section>

      <section
        aria-labelledby="settings-licenses-heading"
        className="flex flex-col gap-3 rounded-xl border border-neutral-200 p-4 dark:border-neutral-800"
      >
        <div>
          <h2 id="settings-licenses-heading" className="text-sm font-semibold">
            Licenses &amp; attribution
          </h2>
          <p className="mt-0.5 text-xs text-neutral-500 dark:text-neutral-400">
            Cleanroom is MIT-licensed. It bundles these third-party components, used as separate
            sidecar processes or models. Their full license texts ship with the app.
          </p>
        </div>
        <ul className="flex flex-col divide-y divide-neutral-200 dark:divide-neutral-800">
          {buildCredits(ffmpegLicense).map((c) => (
            <li key={c.name} className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-0.5 py-2">
              <div className="min-w-0">
                <p className="text-sm font-medium">{c.name}</p>
                <p className="text-xs text-neutral-500 dark:text-neutral-400">{c.role}</p>
              </div>
              <span
                className={`shrink-0 text-xs ${
                  c.attributionRequired
                    ? "font-medium text-emerald-600 dark:text-emerald-400"
                    : "text-neutral-500 dark:text-neutral-400"
                }`}
              >
                {c.license}
                {c.attributionRequired ? " · attribution required" : ""}
              </span>
            </li>
          ))}
        </ul>
      </section>
    </div>
  );
}

/** Bundled third-party components credited on the About/licenses screen (04 §S8). The
 * CC-BY-4.0 TitaNet-small credit is a hard redistribution requirement, not optional — its
 * `attributionRequired` flag makes it visually prominent. License strings mirror the engine
 * catalogs (`anvil_asr`/`anvil_llm` `license` fields) and the ffmpeg sidecar pin —
 * `ffmpegLicense` is threaded in per-platform (see `scripts/ffmpeg-pin.json` /
 * `anvil_media::sidecar::FFMPEG_PINS`: LGPL-3.0-or-later on Windows' BtbN prebuilt,
 * LGPL-2.1-or-later on the from-source macOS build) since that's the one entry that isn't
 * the same on every OS. */
function buildCredits(
  ffmpegLicense: string,
): { name: string; role: string; license: string; attributionRequired?: boolean }[] {
  return [
    {
      name: "NVIDIA NeMo TitaNet-small",
      role: "Speaker embeddings for diarization",
      license: "CC-BY-4.0 (© NVIDIA)",
      attributionRequired: true,
    },
    {
      name: "pyannote segmentation 3.0",
      role: "Speaker segmentation for diarization",
      license: "MIT (© 2022 CNRS)",
    },
    {
      name: "sherpa-onnx",
      role: "Speaker diarization sidecar",
      license: "Apache-2.0",
    },
    {
      name: "ONNX Runtime",
      role: "Diarization model inference",
      license: "MIT",
    },
    {
      name: "whisper.cpp + ggml weights",
      role: "Transcription",
      license: "MIT",
    },
    {
      name: "Qwen2.5-Instruct",
      role: "AI shownotes (optional local LLM)",
      license: "Apache-2.0",
    },
    {
      name: "RNNoise (nnnoiseless)",
      role: "Fast-tier speech denoise",
      license: "MIT",
    },
    {
      name: "FFmpeg",
      role: "Media decode/encode + chapters (sidecar process)",
      license: ffmpegLicense,
    },
  ];
}
