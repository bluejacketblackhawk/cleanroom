interface SwitchProps {
  checked: boolean;
  onChange: (checked: boolean) => void;
  /** Accessible name — this toggle has no visible text of its own (the row label next to
   * it is the visible text), so the name has to travel through `aria-label`. */
  label: string;
  disabled?: boolean;
}

/** A real toggle switch (04 §Accessibility: "screen-reader labels on all controls"): a
 * native `<button>` (keyboard-operable for free — Space/Enter, focusable by default) with
 * `role="switch"`/`aria-checked` so assistive tech announces it as on/off rather than as
 * a generic button, a visible focus ring for keyboard users, and state carried by shape
 * (track fill + thumb position) as well as color so it reads correctly for color-blind
 * users too. */
export default function Switch({ checked, onChange, label, disabled }: SwitchProps) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={`relative inline-flex h-6 w-11 shrink-0 items-center rounded-full border transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-emerald-500 disabled:cursor-not-allowed disabled:opacity-50 ${
        checked
          ? "border-emerald-600 bg-emerald-600"
          : "border-neutral-300 bg-neutral-200 dark:border-neutral-600 dark:bg-neutral-700"
      }`}
    >
      <span
        aria-hidden="true"
        className={`inline-block h-4 w-4 transform rounded-full bg-white shadow transition-transform ${
          checked ? "translate-x-6" : "translate-x-1"
        }`}
      />
    </button>
  );
}
