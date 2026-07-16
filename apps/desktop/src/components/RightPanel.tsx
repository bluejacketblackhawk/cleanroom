import type { KeyboardEvent, ReactNode } from "react";

export type PanelTab = "master" | "export";

interface TabDef {
  id: PanelTab;
  label: string;
  hint: string;
}

const TABS: TabDef[] = [
  { id: "master", label: "Master", hint: "" },
  { id: "export", label: "Export", hint: "E" },
];

interface RightPanelProps {
  activeTab: PanelTab;
  onTabChange: (tab: PanelTab) => void;
  masterContent: ReactNode;
  exportContent: ReactNode;
}

/** The S2 right panel: a small accessible tablist (Master default, Export) over a
 * scrollable content area. Both panels stay mounted (just `hidden`) so tab state — like
 * the Export tab's output rows — survives switching tabs. */
export default function RightPanel({
  activeTab,
  onTabChange,
  masterContent,
  exportContent,
}: RightPanelProps) {
  const handleKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const idx = TABS.findIndex((t) => t.id === activeTab);
    if (e.key === "ArrowRight") {
      e.preventDefault();
      onTabChange(TABS[(idx + 1) % TABS.length].id);
    } else if (e.key === "ArrowLeft") {
      e.preventDefault();
      onTabChange(TABS[(idx - 1 + TABS.length) % TABS.length].id);
    }
  };

  return (
    <div className="flex h-full flex-col overflow-hidden rounded-2xl border border-neutral-200 bg-white/60 dark:border-neutral-800 dark:bg-neutral-900/40">
      <div
        role="tablist"
        aria-label="Production panel"
        onKeyDown={handleKeyDown}
        className="flex border-b border-neutral-200 dark:border-neutral-800"
      >
        {TABS.map((t) => (
          <button
            key={t.id}
            type="button"
            role="tab"
            id={`tab-${t.id}`}
            aria-selected={activeTab === t.id}
            aria-controls={`panel-${t.id}`}
            tabIndex={activeTab === t.id ? 0 : -1}
            onClick={() => onTabChange(t.id)}
            className={`flex-1 px-4 py-3 text-sm font-medium transition-colors ${
              activeTab === t.id
                ? "border-b-2 border-emerald-500 text-neutral-900 dark:text-white"
                : "text-neutral-500 dark:text-neutral-400 hover:text-neutral-800 dark:hover:text-neutral-300"
            }`}
          >
            {t.label}
            {t.hint && <span className="ml-1 text-xs text-neutral-500 dark:text-neutral-400">({t.hint})</span>}
          </button>
        ))}
      </div>
      <div className="flex-1 overflow-y-auto p-4">
        <div
          role="tabpanel"
          id="panel-master"
          aria-labelledby="tab-master"
          hidden={activeTab !== "master"}
        >
          {masterContent}
        </div>
        <div
          role="tabpanel"
          id="panel-export"
          aria-labelledby="tab-export"
          hidden={activeTab !== "export"}
        >
          {exportContent}
        </div>
      </div>
    </div>
  );
}
