// Distinct per-speaker colours for the diarized transcript (04 §S2 "speaker labels").
// Deliberately no orange/amber (that reads as a warning colour and clashes with the app's
// emerald accent) — a spread of hues that stay legible as a faint tint on both themes.

export interface SpeakerColor {
  /** Solid colour for the label chip + the segment's left border. */
  solid: string;
  /** Faint background tint for the segment paragraph. */
  tint: string;
}

const PALETTE: SpeakerColor[] = [
  { solid: "#10b981", tint: "rgba(16, 185, 129, 0.14)" }, // emerald
  { solid: "#0ea5e9", tint: "rgba(14, 165, 233, 0.14)" }, // sky
  { solid: "#8b5cf6", tint: "rgba(139, 92, 246, 0.14)" }, // violet
  { solid: "#f43f5e", tint: "rgba(244, 63, 94, 0.14)" }, // rose
  { solid: "#14b8a6", tint: "rgba(20, 184, 166, 0.14)" }, // teal
  { solid: "#d946ef", tint: "rgba(217, 70, 239, 0.14)" }, // fuchsia
  { solid: "#06b6d4", tint: "rgba(6, 182, 212, 0.14)" }, // cyan
  { solid: "#6366f1", tint: "rgba(99, 102, 241, 0.14)" }, // indigo
];

/** A stable, distinct colour for a diarized speaker id (wraps past the palette length). */
export function speakerColor(id: number): SpeakerColor {
  return PALETTE[((id % PALETTE.length) + PALETTE.length) % PALETTE.length];
}
