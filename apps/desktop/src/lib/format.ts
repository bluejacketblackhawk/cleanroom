// Small display formatters shared across the transport, meters, and health card.

export function formatTime(secs: number): string {
  if (!Number.isFinite(secs) || secs < 0) return "0:00";
  const m = Math.floor(secs / 60);
  const s = Math.floor(secs % 60);
  return `${m}:${s.toString().padStart(2, "0")}`;
}

export function formatLufs(v: number): string {
  return `${v.toFixed(1)} LUFS`;
}

export function formatDbtp(v: number): string {
  return `${v.toFixed(1)} dBTP`;
}

export function formatLu(v: number): string {
  return `${v.toFixed(1)} LU`;
}

/** `"switch_to_studio"` -> `"Switch To Studio"` — fallback label for unmapped fix codes. */
export function titleCase(id: string): string {
  return id
    .replace(/_/g, " ")
    .replace(/\b\w/g, (c) => c.toUpperCase());
}

/** Human byte count for model download progress ("will download X MB" / "42 MB of 466 MB"). */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 MB";
  const mb = bytes / 1_000_000;
  if (mb < 1) return `${Math.max(1, Math.round(bytes / 1000))} KB`;
  if (mb < 1000) return `${mb.toFixed(mb < 10 ? 1 : 0)} MB`;
  return `${(mb / 1000).toFixed(2)} GB`;
}
