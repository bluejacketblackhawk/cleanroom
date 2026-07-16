import { useState } from "react";
import * as Dialog from "@radix-ui/react-dialog";

/** 04 §S9 Onboarding, verbatim: "1) Drop a file, press Master — that's the whole app.
 * 2) Everything runs on this computer. Airplane mode? Still works. 3) More power when you
 * want it: transcripts, batch, watch folders. → offers demo file." Max 3 cards, every
 * card skippable. */
const CARDS = [
  "Drop a file, press Master — that's the whole app.",
  "Everything runs on this computer. Airplane mode? Still works.",
  "More power when you want it: transcripts, batch, watch folders.",
];

interface OnboardingProps {
  open: boolean;
  /** User skipped a card, skipped the demo offer, or dismissed via Escape/overlay click —
   * all treated the same (04's "skippable"). */
  onSkip: () => void;
  /** User chose the bundled demo file on the final screen. */
  onTryDemo: () => void;
}

/** First-run onboarding: `@radix-ui/react-dialog` gives this a real accessible-dialog
 * baseline for free (focus trap, focus-on-open, `Escape`-to-close, `aria-modal`,
 * `role="dialog"`) rather than a hand-rolled overlay div — 04's accessibility rules ask
 * for full keyboard traversal and screen-reader correctness, and a from-scratch modal is
 * the single easiest place to get that wrong. */
export default function Onboarding({ open, onSkip, onTryDemo }: OnboardingProps) {
  const [step, setStep] = useState(0);
  const total = CARDS.length;
  const isCard = step < total;

  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (!next) onSkip();
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-50 bg-black/50" />
        <Dialog.Content
          className="fixed top-1/2 left-1/2 z-50 w-[min(92vw,26rem)] -translate-x-1/2 -translate-y-1/2 rounded-2xl bg-white p-6 shadow-xl outline-none focus-visible:ring-2 focus-visible:ring-emerald-500 dark:bg-neutral-900"
          onEscapeKeyDown={onSkip}
        >
          {isCard ? (
            <>
              <Dialog.Title className="text-base font-semibold">Welcome to Cleanroom</Dialog.Title>
              <Dialog.Description className="mt-3 text-sm text-neutral-600 dark:text-neutral-300">
                {CARDS[step]}
              </Dialog.Description>
              <div className="mt-6 flex items-center justify-between gap-4">
                <div
                  role="img"
                  aria-label={`Step ${step + 1} of ${total}`}
                  className="flex gap-1.5"
                >
                  {CARDS.map((_, i) => (
                    <span
                      key={i}
                      aria-hidden="true"
                      className={`h-1.5 w-1.5 rounded-full ${
                        i === step ? "bg-emerald-500" : "bg-neutral-300 dark:bg-neutral-700"
                      }`}
                    />
                  ))}
                </div>
                <div className="flex gap-2">
                  <button
                    type="button"
                    onClick={onSkip}
                    className="rounded-lg px-3 py-1.5 text-xs font-medium text-neutral-500 dark:text-neutral-400 hover:bg-neutral-100 dark:hover:bg-neutral-800"
                  >
                    Skip
                  </button>
                  <button
                    type="button"
                    onClick={() => setStep((s) => s + 1)}
                    className="rounded-lg bg-emerald-600 px-4 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500"
                  >
                    {step === total - 1 ? "Get started" : "Next"}
                  </button>
                </div>
              </div>
            </>
          ) : (
            <>
              <Dialog.Title className="text-base font-semibold">Want to see it work?</Dialog.Title>
              <Dialog.Description className="mt-3 text-sm text-neutral-600 dark:text-neutral-300">
                We bundled a short, deliberately bad recording — noisy and quiet — so you can
                hear Master fix it in a few seconds.
              </Dialog.Description>
              <div className="mt-6 flex justify-end gap-2">
                <button
                  type="button"
                  onClick={onSkip}
                  className="rounded-lg px-3 py-1.5 text-xs font-medium text-neutral-500 dark:text-neutral-400 hover:bg-neutral-100 dark:hover:bg-neutral-800"
                >
                  I'll drop my own file
                </button>
                <button
                  type="button"
                  onClick={onTryDemo}
                  className="rounded-lg bg-emerald-600 px-4 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500"
                >
                  Try the demo file
                </button>
              </div>
            </>
          )}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
