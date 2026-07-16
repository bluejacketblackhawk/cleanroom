// Shared preset-list loader: the Master tab's preset dropdown, the Batch/Watch screens'
// preset pickers, and the Presets manager itself all read the same shipped+user list, so a
// preset created/edited/deleted in one place shows up everywhere else without extra
// plumbing (04 §S6 "feed the selected preset into Master + Batch").
import { useCallback, useEffect, useState } from "react";
import { presetsList, type PresetSummary } from "../api";

export function usePresets() {
  const [presets, setPresets] = useState<PresetSummary[]>([]);
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(() => {
    setLoading(true);
    presetsList()
      .then(setPresets)
      .catch(() => setPresets([]))
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  return { presets, loading, refresh };
}
