import { VIEWS, type View } from "../lib/nav";

const ICONS: Record<View, string> = {
  // Small, calm, single-color line icons — no orange, matches the emerald accent.
  master: "M4 12h16M4 6h16M4 18h10",
  transcript: "M5 4h14v2H5V4Zm0 5h14v2H5V9Zm0 5h9v2H5v-2Zm0 5h6v2H5v-2Z",
  metadata: "M4 5h16M4 5v14M4 19h16M9 5v14M13 9h4M13 12h4M13 15h2",
  multitrack: "M3 6h6M3 12h11M3 18h8M15 4v5M15 15v5M20 6.5v11",
  clip_studio: "M4 5h11v14H4V5Zm11 4 5-2.5v11L15 15",
  guard: "M12 3a4 4 0 0 1 4 4v5a4 4 0 1 1-8 0V7a4 4 0 0 1 4-4Zm-7 8a7 7 0 0 0 14 0M12 18v3",
  batch: "M4 6h16M4 12h16M4 18h10",
  watch: "M12 3v3m0 12v3m9-9h-3M6 12H3m14.5-6.5-2 2m-9 9-2 2m13-2-2-2m-9-9-2-2",
  presets: "M4 6h4v4H4V6Zm6 0h10M4 14h4v4H4v-4Zm6 0h10",
  models: "M4 4h16v6H4V4Zm0 10h16v6H4v-6Z",
  settings:
    "M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm8-3a7.9 7.9 0 0 0-.15-1.5l1.85-1.4-1.5-2.6-2.2.75a8 8 0 0 0-1.3-.75L16.3 4h-4.6l-.4 2.5a8 8 0 0 0-1.3.75l-2.2-.75-1.5 2.6L8.15 10.5A7.9 7.9 0 0 0 8 12c0 .5.05 1 .15 1.5L6.3 14.9l1.5 2.6 2.2-.75c.4.3.85.55 1.3.75l.4 2.5h4.6l.4-2.5a8 8 0 0 0 1.3-.75l2.2.75 1.5-2.6-1.85-1.4c.1-.5.15-1 .15-1.5Z",
};

interface NavRailProps {
  active: View;
  onChange: (v: View) => void;
}

/** Left rail (04 "reachable from the S2 screen via a left rail or top nav"): switches
 * between the single-file Master screen and the M2 Batch/Watch/Presets/Models screens. */
export default function NavRail({ active, onChange }: NavRailProps) {
  return (
    <nav
      aria-label="Screens"
      className="sticky top-0 flex max-h-screen shrink-0 flex-col gap-1 self-start overflow-y-auto border-r border-neutral-200 bg-white/60 p-2 dark:border-neutral-800 dark:bg-neutral-900/40"
    >
      {VIEWS.map((v) => (
        <button
          key={v.id}
          type="button"
          title={v.hint}
          aria-current={active === v.id ? "page" : undefined}
          onClick={() => onChange(v.id)}
          className={`flex flex-col items-center gap-1 rounded-lg px-3 py-2.5 text-[11px] font-medium transition-colors ${
            active === v.id
              ? "bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
              : "text-neutral-500 dark:text-neutral-400 hover:bg-neutral-100 hover:text-neutral-800 dark:hover:bg-neutral-800 dark:hover:text-neutral-200"
          }`}
        >
          <svg
            aria-hidden="true"
            viewBox="0 0 24 24"
            className="h-5 w-5"
            fill="none"
            stroke="currentColor"
            strokeWidth={1.75}
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d={ICONS[v.id]} />
          </svg>
          {v.label}
        </button>
      ))}
    </nav>
  );
}
