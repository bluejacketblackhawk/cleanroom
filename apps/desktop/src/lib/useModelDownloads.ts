// Shared model-pack list + live download progress, used by the S7 Models screen and the
// Transcript tab's inline "will download X MB" model picker — both listen to the same
// `models://download` event stream, so a download started from either place shows up in
// both without any shared store beyond the Tauri event bus itself.
import { useCallback, useEffect, useState } from "react";
import {
  downloadModel,
  downloadModelCancel,
  modelsList,
  onModelDownloadProgress,
  type ModelDownloadProgress,
  type ModelPack,
} from "../api";

export function useModelDownloads() {
  const [models, setModels] = useState<ModelPack[]>([]);
  const [progress, setProgress] = useState<Record<string, ModelDownloadProgress>>({});

  const refresh = useCallback(() => {
    modelsList()
      .then(setModels)
      .catch(() => setModels([]));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  useEffect(() => {
    const unlisten = onModelDownloadProgress((e) => {
      setProgress((p) => ({ ...p, [e.pack]: e }));
      // "done" flips `installed` server-side — re-pull the list so buttons update.
      if (e.status === "done") refresh();
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, [refresh]);

  const start = useCallback((pack: string) => {
    setProgress((p) => ({
      ...p,
      [pack]: { pack, downloaded_bytes: 0, total_bytes: 0, status: "downloading", message: null },
    }));
    downloadModel(pack).catch((e) => {
      setProgress((p) => ({
        ...p,
        [pack]: {
          pack,
          downloaded_bytes: 0,
          total_bytes: 0,
          status: "error",
          message: typeof e === "string" ? e : "Could not start the download.",
        },
      }));
    });
  }, []);

  const cancel = useCallback((pack: string) => {
    void downloadModelCancel(pack);
  }, []);

  return { models, progress, refresh, start, cancel };
}
