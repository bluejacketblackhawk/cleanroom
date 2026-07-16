// Top-level screens (04 §S2/S3/S4/S5/S6/S7/S10), reachable from the left rail. "master" is
// the existing drop → waveform → Master/Export screen; "transcript" is the M3 Transcript
// tab (word-level transcript + filler/silence review), which also needs the open file, so
// it lives alongside "master" rather than as an independent M2-style screen. M4 adds
// "multitrack" (S3), "clip_studio" (Clip Studio), and "guard" (S10 Recording Guard).
export type View =
  | "master"
  | "transcript"
  | "metadata"
  | "multitrack"
  | "clip_studio"
  | "guard"
  | "batch"
  | "watch"
  | "presets"
  | "models"
  | "settings";

export interface ViewDef {
  id: View;
  label: string;
  hint: string;
}

export const VIEWS: ViewDef[] = [
  { id: "master", label: "Master", hint: "One file at a time" },
  { id: "transcript", label: "Transcript", hint: "Words, fillers, silence" },
  { id: "metadata", label: "Chapters", hint: "Chapters, tags, cover art, AI shownotes" },
  { id: "multitrack", label: "Multitrack", hint: "Align, duck, and mix several tracks" },
  { id: "clip_studio", label: "Clip Studio", hint: "A shareable clip in under a minute" },
  { id: "guard", label: "Rec. Guard", hint: "Check levels before you hit record" },
  { id: "batch", label: "Batch", hint: "Many files, one preset" },
  { id: "watch", label: "Watch", hint: "Folders that master themselves" },
  { id: "presets", label: "Presets", hint: "Your mastering targets" },
  { id: "models", label: "Models", hint: "What's running locally" },
  { id: "settings", label: "Settings", hint: "Integration, updates, diagnostics" },
];
