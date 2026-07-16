import { useCallback, useEffect, useState } from "react";
import {
  generateShownotes,
  metadataRead,
  metadataWrite,
  playbackPosition,
  type ChapterWire,
  type FileMetadata,
  type MediaSummary,
  type ShownotesResult,
} from "../api";

interface MetadataScreenProps {
  media: MediaSummary | null;
  fileName: string | null;
  sourcePath: string | null;
}

/** `1_230_000` -> `"20:30"` (or `"1:02:03"` past an hour). */
function msToClock(ms: number): string {
  const totalSec = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  const mm = m.toString().padStart(2, "0");
  const ss = s.toString().padStart(2, "0");
  return h > 0 ? `${h}:${mm}:${ss}` : `${m}:${ss}`;
}

/** Sort chapters by start time and derive each `end_ms` as the next chapter's start (or the
 * file duration for the last) — ffmetadata needs an explicit end on every chapter. */
function normalizeChapters(chapters: ChapterWire[], durationMs: number): ChapterWire[] {
  const sorted = [...chapters].sort((a, b) => a.start_ms - b.start_ms);
  return sorted.map((c, i) => ({
    ...c,
    end_ms: i + 1 < sorted.length ? sorted[i + 1].start_ms : Math.max(c.start_ms, durationMs),
  }));
}

/**
 * Chapters & Metadata tab (04 §S2): edit standard tags (title/artist/album/year/genre/comment/
 * track) + cover art, add/rename/reorder chapters (add at the playhead), pull an AI "suggest"
 * (shownotes) from the transcript, and write it all back to the file (in place, or to a
 * mastered export). Backed by `anvil_media`'s TagEditor + ffmpeg chapters and `anvil_llm`.
 */
export default function MetadataScreen({
  media,
  fileName,
  sourcePath,
}: MetadataScreenProps) {
  const durationMs = media ? Math.round(media.duration_secs * 1000) : 0;

  const [meta, setMeta] = useState<FileMetadata | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  // Editable form fields (strings so empty is a clean state).
  const [title, setTitle] = useState("");
  const [artist, setArtist] = useState("");
  const [album, setAlbum] = useState("");
  const [genre, setGenre] = useState("");
  const [date, setDate] = useState("");
  const [comment, setComment] = useState("");
  const [track, setTrack] = useState("");

  const [chapters, setChapters] = useState<ChapterWire[]>([]);

  // Cover art: the existing artwork's data URL (from the file), plus an optional new image
  // path to embed and a "remove" flag.
  const [existingCover, setExistingCover] = useState<string | null>(null);
  const [newCoverPath, setNewCoverPath] = useState("");
  const [removeCover, setRemoveCover] = useState(false);

  const [target, setTarget] = useState("");
  const [saving, setSaving] = useState(false);
  const [saveBanner, setSaveBanner] = useState<string | null>(null);

  const [shownotes, setShownotes] = useState<ShownotesResult | null>(null);
  const [suggesting, setSuggesting] = useState(false);
  const [shownotesError, setShownotesError] = useState<string | null>(null);

  const loadMetadata = useCallback(() => {
    metadataRead()
      .then((m) => {
        setMeta(m);
        setLoadError(null);
        setTitle(m.title ?? "");
        setArtist(m.artist ?? "");
        setAlbum(m.album ?? "");
        setGenre(m.genre ?? "");
        setDate(m.date ?? "");
        setComment(m.comment ?? "");
        setTrack(m.track != null ? String(m.track) : "");
        setChapters(normalizeChapters(m.chapters, durationMs));
        setExistingCover(m.cover_art);
        setNewCoverPath("");
        setRemoveCover(false);
      })
      .catch((e) => {
        setMeta(null);
        setLoadError(typeof e === "string" ? e : "Could not read this file's metadata.");
      });
  }, [durationMs]);

  // (Re)load when the open file changes.
  useEffect(() => {
    if (!sourcePath) {
      setMeta(null);
      return;
    }
    setSaveBanner(null);
    setShownotes(null);
    setShownotesError(null);
    setTarget(sourcePath);
    loadMetadata();
  }, [sourcePath, loadMetadata]);

  const addChapterAtPlayhead = useCallback(async () => {
    if (!media) return;
    const frame = await playbackPosition().catch(() => 0);
    const startMs = media.sample_rate > 0 ? Math.round((frame / media.sample_rate) * 1000) : 0;
    setChapters((prev) => {
      const n = prev.length + 1;
      return normalizeChapters(
        [...prev, { title: `Chapter ${n}`, start_ms: startMs, end_ms: startMs }],
        durationMs,
      );
    });
  }, [media, durationMs]);

  const renameChapter = (index: number, value: string) => {
    setChapters((prev) => prev.map((c, i) => (i === index ? { ...c, title: value } : c)));
  };

  const deleteChapter = (index: number) => {
    setChapters((prev) => normalizeChapters(prev.filter((_, i) => i !== index), durationMs));
  };

  // Move a chapter earlier/later by swapping its start time with its neighbour's, then
  // re-sorting — a real reorder (the title moves to the other time slot).
  const moveChapter = (index: number, dir: -1 | 1) => {
    setChapters((prev) => {
      const other = index + dir;
      if (other < 0 || other >= prev.length) return prev;
      const next = prev.map((c) => ({ ...c }));
      const tmp = next[index].start_ms;
      next[index].start_ms = next[other].start_ms;
      next[other].start_ms = tmp;
      return normalizeChapters(next, durationMs);
    });
  };

  const handleSave = async () => {
    if (!meta) return;
    setSaving(true);
    setSaveBanner(null);
    try {
      await metadataWrite(target, {
        title,
        artist,
        album,
        genre,
        date,
        comment,
        track: track.trim() ? Number(track) : null,
        cover_art_path: newCoverPath.trim() ? newCoverPath.trim() : null,
        remove_cover_art: removeCover,
        chapters: normalizeChapters(chapters, durationMs),
      });
      setSaveBanner(`Saved to ${target}`);
      // Re-read so the form reflects what actually landed (and any new cover art).
      if (target === sourcePath) loadMetadata();
    } catch (e) {
      setSaveBanner(typeof e === "string" ? e : "Could not save the metadata.");
    } finally {
      setSaving(false);
    }
  };

  const handleSuggest = async () => {
    setSuggesting(true);
    setShownotesError(null);
    try {
      const result = await generateShownotes();
      setShownotes(result);
    } catch (e) {
      setShownotesError(
        typeof e === "string" ? e : "Could not generate shownotes for this file.",
      );
    } finally {
      setSuggesting(false);
    }
  };

  const useSuggestedChapters = () => {
    if (!shownotes) return;
    setChapters(
      normalizeChapters(
        shownotes.chapters.map((c) => ({
          title: c.title,
          start_ms: Math.round(c.start_secs * 1000),
          end_ms: Math.round(c.start_secs * 1000),
        })),
        durationMs,
      ),
    );
  };

  const coverPreview = removeCover ? null : existingCover;

  if (!media) {
    return (
      <main className="flex flex-1 items-center justify-center p-6">
        <p className="max-w-sm text-center text-sm text-neutral-500 dark:text-neutral-400">
          Open a file from Master first — Chapters &amp; Metadata works on whatever is currently
          loaded.
        </p>
      </main>
    );
  }

  return (
    <main className="flex flex-1 flex-col gap-4 overflow-y-auto p-6">
      <div className="flex items-baseline justify-between gap-4">
        <span className="truncate text-sm font-medium" title={fileName ?? ""}>
          {fileName}
        </span>
        <span className="shrink-0 text-xs text-neutral-500 dark:text-neutral-400">
          Chapters &amp; Metadata
        </span>
      </div>

      {loadError && <p className="text-xs text-red-500">{loadError}</p>}

      <div className="grid gap-4 lg:grid-cols-2">
        {/* ---- Metadata form ---- */}
        <section className="flex flex-col gap-3 rounded-lg border border-neutral-200 p-4 dark:border-neutral-800">
          <h2 className="text-sm font-semibold">Metadata</h2>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Title" value={title} onChange={setTitle} className="col-span-2" />
            <Field label="Artist" value={artist} onChange={setArtist} />
            <Field label="Album" value={album} onChange={setAlbum} />
            <Field label="Genre" value={genre} onChange={setGenre} />
            <Field label="Year / date" value={date} onChange={setDate} placeholder="2026 or 2026-07-14" />
            <Field label="Track" value={track} onChange={setTrack} placeholder="1" />
            <Field label="Comment" value={comment} onChange={setComment} className="col-span-2" />
          </div>

          <div className="flex flex-col gap-2">
            <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">Cover art</span>
            <div className="flex items-start gap-3">
              {coverPreview ? (
                <img
                  src={coverPreview}
                  alt="Cover art"
                  className="h-20 w-20 shrink-0 rounded-md object-cover ring-1 ring-neutral-300 dark:ring-neutral-700"
                />
              ) : (
                <div className="flex h-20 w-20 shrink-0 items-center justify-center rounded-md border border-dashed border-neutral-300 text-[10px] text-neutral-400 dark:border-neutral-700">
                  No cover
                </div>
              )}
              <div className="flex flex-1 flex-col gap-1">
                <input
                  type="text"
                  value={newCoverPath}
                  onChange={(e) => {
                    setNewCoverPath(e.target.value);
                    if (e.target.value.trim()) setRemoveCover(false);
                  }}
                  placeholder="Path to a new cover image (jpg/png)…"
                  className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
                />
                {(existingCover || newCoverPath.trim()) && (
                  <label className="flex items-center gap-1.5 text-[11px] text-neutral-500 dark:text-neutral-400">
                    <input
                      type="checkbox"
                      checked={removeCover}
                      onChange={(e) => {
                        setRemoveCover(e.target.checked);
                        if (e.target.checked) setNewCoverPath("");
                      }}
                    />
                    Remove cover art on save
                  </label>
                )}
              </div>
            </div>
          </div>
        </section>

        {/* ---- Chapters ---- */}
        <section className="flex flex-col gap-3 rounded-lg border border-neutral-200 p-4 dark:border-neutral-800">
          <div className="flex items-center justify-between">
            <h2 className="text-sm font-semibold">Chapters</h2>
            <button
              type="button"
              onClick={() => void addChapterAtPlayhead()}
              className="rounded-lg bg-emerald-600 px-3 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500"
            >
              Add at playhead
            </button>
          </div>

          {meta && !meta.chapters_available && (
            <p className="text-[11px] text-amber-600 dark:text-amber-400">
              {meta.chapters_note ??
                "Chapters couldn't be read from this file, but you can still add them."}
            </p>
          )}

          {chapters.length === 0 ? (
            <p className="text-xs text-neutral-500 dark:text-neutral-400">
              No chapters yet. Play to a spot and click “Add at playhead”, or use AI suggest below.
            </p>
          ) : (
            <ul className="flex flex-col gap-1.5">
              {chapters.map((c, i) => (
                <li key={i} className="flex items-center gap-2">
                  <span className="w-16 shrink-0 text-right text-[11px] tabular-nums text-neutral-500 dark:text-neutral-400">
                    {msToClock(c.start_ms)}
                  </span>
                  <input
                    type="text"
                    value={c.title}
                    onChange={(e) => renameChapter(i, e.target.value)}
                    className="min-w-0 flex-1 rounded-md border border-neutral-300 bg-white px-2 py-1 text-xs dark:border-neutral-700 dark:bg-neutral-900"
                  />
                  <button
                    type="button"
                    onClick={() => moveChapter(i, -1)}
                    disabled={i === 0}
                    title="Move earlier"
                    className="rounded px-1.5 py-1 text-xs text-neutral-500 hover:bg-neutral-100 disabled:opacity-30 dark:text-neutral-400 dark:hover:bg-neutral-800"
                  >
                    ↑
                  </button>
                  <button
                    type="button"
                    onClick={() => moveChapter(i, 1)}
                    disabled={i === chapters.length - 1}
                    title="Move later"
                    className="rounded px-1.5 py-1 text-xs text-neutral-500 hover:bg-neutral-100 disabled:opacity-30 dark:text-neutral-400 dark:hover:bg-neutral-800"
                  >
                    ↓
                  </button>
                  <button
                    type="button"
                    onClick={() => deleteChapter(i)}
                    title="Delete chapter"
                    className="rounded px-1.5 py-1 text-xs text-neutral-500 hover:bg-red-500/10 hover:text-red-500 dark:text-neutral-400"
                  >
                    ✕
                  </button>
                </li>
              ))}
            </ul>
          )}
        </section>
      </div>

      {/* ---- AI shownotes ---- */}
      <section className="flex flex-col gap-3 rounded-lg border border-neutral-200 p-4 dark:border-neutral-800">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 className="text-sm font-semibold">AI shownotes</h2>
          <button
            type="button"
            onClick={() => void handleSuggest()}
            disabled={suggesting}
            className="rounded-lg border border-emerald-500/50 px-4 py-1.5 text-xs font-medium text-emerald-600 hover:bg-emerald-500/10 disabled:cursor-not-allowed disabled:opacity-50 dark:text-emerald-400"
          >
            {suggesting ? "Thinking…" : "Suggest with AI"}
          </button>
        </div>
        <p className="text-[11px] text-neutral-500 dark:text-neutral-400">
          Summary, chapters, titles and keywords from the transcript. Transcribe the file first
          (Transcript tab). Uses the local Qwen model if installed; otherwise a built-in
          summarizer.
        </p>
        {shownotesError && <p className="text-xs text-red-500">{shownotesError}</p>}

        {shownotes && (
          <div className="flex flex-col gap-3">
            <div className="flex flex-wrap items-center gap-2">
              <span
                className={`rounded-full px-2 py-0.5 text-[10px] font-medium ${
                  shownotes.engine === "llm"
                    ? "bg-emerald-500/10 text-emerald-600 dark:text-emerald-400"
                    : "bg-neutral-200 text-neutral-500 dark:bg-neutral-800 dark:text-neutral-400"
                }`}
              >
                {shownotes.engine === "llm" ? "Written by local AI" : "Built-in summarizer"}
              </span>
              {shownotes.note && (
                <span className="text-[11px] text-neutral-500 dark:text-neutral-400">{shownotes.note}</span>
              )}
            </div>

            {shownotes.summary && (
              <p className="text-xs leading-relaxed text-neutral-700 dark:text-neutral-300">
                {shownotes.summary}
              </p>
            )}

            {shownotes.bullets.length > 0 && (
              <ul className="ml-4 list-disc text-xs text-neutral-700 dark:text-neutral-300">
                {shownotes.bullets.map((b, i) => (
                  <li key={i}>{b}</li>
                ))}
              </ul>
            )}

            {shownotes.titles.length > 0 && (
              <div className="flex flex-col gap-1">
                <span className="text-[11px] font-medium text-neutral-500 dark:text-neutral-400">
                  Title ideas
                </span>
                <div className="flex flex-wrap gap-1.5">
                  {shownotes.titles.map((t, i) => (
                    <button
                      key={i}
                      type="button"
                      onClick={() => setTitle(t)}
                      title="Use as title"
                      className="rounded-md border border-neutral-300 px-2 py-1 text-[11px] text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
                    >
                      {t}
                    </button>
                  ))}
                </div>
              </div>
            )}

            {shownotes.keywords.length > 0 && (
              <div className="flex flex-wrap gap-1">
                {shownotes.keywords.map((k, i) => (
                  <span
                    key={i}
                    className="rounded-full bg-neutral-100 px-2 py-0.5 text-[10px] text-neutral-500 dark:bg-neutral-800 dark:text-neutral-400"
                  >
                    {k}
                  </span>
                ))}
              </div>
            )}

            {shownotes.chapters.length > 0 && (
              <div className="flex flex-wrap items-center gap-2">
                <span className="text-[11px] text-neutral-500 dark:text-neutral-400">
                  {shownotes.chapters.length} suggested chapter
                  {shownotes.chapters.length === 1 ? "" : "s"}
                </span>
                <button
                  type="button"
                  onClick={useSuggestedChapters}
                  className="rounded-md border border-emerald-500/50 px-2 py-1 text-[11px] font-medium text-emerald-600 hover:bg-emerald-500/10 dark:text-emerald-400"
                >
                  Use these chapters
                </button>
              </div>
            )}
          </div>
        )}
      </section>

      {/* ---- Apply ---- */}
      <section className="flex flex-wrap items-end gap-3 rounded-lg border border-neutral-200 p-4 dark:border-neutral-800">
        <label className="flex min-w-64 flex-1 flex-col gap-1">
          <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">
            Write to
          </span>
          <input
            type="text"
            value={target}
            onChange={(e) => setTarget(e.target.value)}
            className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
          />
        </label>
        <button
          type="button"
          onClick={() => void handleSave()}
          disabled={saving || !meta}
          className="rounded-lg bg-emerald-600 px-4 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {saving ? "Saving…" : "Apply to file"}
        </button>
        {saveBanner && <span className="text-xs text-neutral-500 dark:text-neutral-400">{saveBanner}</span>}
      </section>
    </main>
  );
}

function Field({
  label,
  value,
  onChange,
  placeholder,
  className,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  className?: string;
}) {
  return (
    <label className={`flex flex-col gap-1 ${className ?? ""}`}>
      <span className="text-xs font-medium text-neutral-500 dark:text-neutral-400">{label}</span>
      <input
        type="text"
        value={value}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        className="rounded-md border border-neutral-300 bg-white px-2 py-1.5 text-xs dark:border-neutral-700 dark:bg-neutral-900"
      />
    </label>
  );
}
