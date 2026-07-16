import type { Speaker, Transcript } from "../api";
import { speakerColor } from "../lib/speakers";

interface TranscriptWordsProps {
  transcript: Transcript;
  /** Playhead position in seconds, from `usePlayhead` — drives word-level follow. */
  currentSeconds: number;
  /** Lowercased search query; matching words get a highlight independent of the playhead. */
  searchQuery: string;
  /** Seek (and, if paused, nothing else — the transport stays as the user left it) to a
   * word's start time. */
  onSeekSeconds: (seconds: number) => void;
  /** Diarized speakers (with any user-edited names), for per-segment labels + colours. Empty
   * before diarization, in which case no speaker chrome is shown. */
  speakers?: Speaker[];
}

/**
 * Word-level transcript body (04 §S2 Transcript tab): highlights the word under the
 * playhead, search-highlights matches, and seeks on click. Segment boundaries render as
 * paragraph breaks so long transcripts stay readable. When the transcript has been diarized
 * (`speakers` non-empty), each segment paragraph carries its speaker's label + a distinct
 * colour (04 §S2 "speaker labels").
 */
export default function TranscriptWords({
  transcript,
  currentSeconds,
  searchQuery,
  onSeekSeconds,
  speakers = [],
}: TranscriptWordsProps) {
  const query = searchQuery.trim().toLowerCase();
  const labelFor = new Map(speakers.map((s) => [s.id, s.label]));
  const diarized = speakers.length > 0;

  // Group words by which segment (paragraph) they fall in, via each word's start time —
  // the contract doesn't link words to segments by index, so this is a time-range join.
  const paragraphs: {
    key: number;
    speaker: number | null;
    words: { word: Transcript["words"][number]; index: number }[];
  }[] =
    transcript.segments.length > 0
      ? transcript.segments.map((seg, segIndex) => ({
          key: segIndex,
          speaker: seg.speaker ?? null,
          words: transcript.words
            .map((word, index) => ({ word, index }))
            .filter(({ word }) => word.start >= seg.start - 1e-6 && word.start < seg.end + 1e-6),
        }))
      : [{ key: 0, speaker: null, words: transcript.words.map((word, index) => ({ word, index })) }];

  if (transcript.words.length === 0) {
    return <p className="text-xs text-neutral-500 dark:text-neutral-400">No words in this transcript.</p>;
  }

  return (
    <div className="flex flex-col gap-3">
      {paragraphs.map((p) => {
        const color = diarized && p.speaker != null ? speakerColor(p.speaker) : null;
        const label =
          diarized && p.speaker != null
            ? (labelFor.get(p.speaker) ?? `Speaker ${p.speaker + 1}`)
            : null;
        return (
          <div
            key={p.key}
            className="rounded-md"
            style={
              color
                ? { borderLeft: `3px solid ${color.solid}`, background: color.tint, paddingLeft: "0.75rem", paddingRight: "0.5rem", paddingTop: "0.25rem", paddingBottom: "0.25rem" }
                : undefined
            }
          >
            {label && (
              <span
                className="mb-0.5 inline-block text-[10px] font-semibold uppercase tracking-wide"
                style={{ color: color?.solid }}
              >
                {label}
              </span>
            )}
            <p className="text-sm leading-loose">
              {p.words.map(({ word, index }) => {
                const active = currentSeconds >= word.start && currentSeconds < word.end;
                const matches = query.length > 0 && word.text.toLowerCase().includes(query);
                return (
                  <button
                    key={index}
                    type="button"
                    onClick={() => onSeekSeconds(word.start)}
                    title={`${word.start.toFixed(1)}s · ${Math.round(word.confidence * 100)}% confidence`}
                    className={`mr-1 rounded px-0.5 transition-colors ${
                      active
                        ? "bg-emerald-500/25 text-emerald-800 dark:text-emerald-200"
                        : matches
                          ? "bg-amber-400/30"
                          : "hover:bg-neutral-200 dark:hover:bg-neutral-800"
                    }`}
                  >
                    {word.text}
                  </button>
                );
              })}
            </p>
          </div>
        );
      })}
    </div>
  );
}
