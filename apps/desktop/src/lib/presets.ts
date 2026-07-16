// Tier catalog for the Master tab (04 §S2 Master tab). Preset ids/labels are no longer
// hardcoded here — the Master/Batch/Watch preset pickers all load the shipped + user
// preset list live from the backend (`api.ts::presetsList`, `lib/usePresets.ts`), the same
// registry the S6 Presets manager edits, so a change in one place shows up everywhere.

export type Tier = "fast" | "standard" | "studio";

export interface TierOption {
  id: Tier;
  label: string;
  detail: string;
}

export const TIERS: TierOption[] = [
  { id: "fast", label: "Fast", detail: "Quickest pass — good for previews or weak machines." },
  { id: "standard", label: "Standard", detail: "The one-click default." },
  { id: "studio", label: "Studio", detail: "Best quality for bad audio — takes longer." },
];

/** Matches `anvil_project::preset::PODCAST_STEREO_ID` — the shipped default. */
export const DEFAULT_PRESET = "podcast_stereo";
export const DEFAULT_TIER: Tier = "standard";

/** Verbs cycled while a Master run is in flight (microcopy rules: processing verbs). */
export const PROGRESS_VERBS = [
  "Listening to the file…",
  "Removing noise…",
  "Balancing voices…",
  "Setting loudness…",
];

/** Known Health Card "fix" action codes mapped to their chip label. */
export const FIX_LABELS: Record<string, string> = {
  switch_to_studio: "Switch to Studio",
};
